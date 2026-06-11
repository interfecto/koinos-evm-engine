//! Koinos EVM JSON-RPC proxy.
//!
//! Translates Ethereum JSON-RPC requests into Koinos chain RPC / contract calls
//! against our deployed revm engine, and back. Lets MetaMask / Hardhat / web3.js
//! treat Koinos like an Ethereum node.

mod bloom;
mod db;
mod engine_proto;
mod eth_codec;
mod eth_tx;
mod indexer;
mod koinos;
mod koinos_tx;
mod limit;
mod rpc;
mod state;
mod ws;

use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Json},
    routing::post,
};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "koinos_evm_rpc=info,axum=info".into()),
        )
        .init();

    let cfg = state::Config::from_env()?;
    info!(
        listen = %cfg.listen_addr,
        koinos_rpc = %cfg.koinos_rpc_url,
        engine_contract = %cfg.engine_contract_addr_b58,
        evm_chain_id = cfg.evm_chain_id,
        "starting Koinos EVM JSON-RPC proxy"
    );

    let state = Arc::new(AppState::new(cfg)?);
    let listen_addr = state.config.listen_addr.clone();

    // Periodic operator-nonce reconcile: heals cache↔chain drift that the error-path
    // resync can't see (e.g. a tx recorded as submitted that the mempool later dropped).
    let reconcile_secs = state.config.nonce_reconcile_secs;
    if reconcile_secs > 0 {
        let reconcile_state = state.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(reconcile_secs));
            interval.tick().await; // first tick fires immediately; skip it
            loop {
                interval.tick().await;
                rpc::reconcile_operator_nonce(&reconcile_state, reconcile_secs * 2).await;
            }
        });
    }

    // Background receipt poller: settles pending txs into the durable store so
    // receipts/logs survive a restart even if no client ever polled for them.
    let poll_secs = state.config.receipt_poll_secs;
    if poll_secs > 0 {
        let poll_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                rpc::poll_pending_receipts(&poll_state).await;
            }
        });
    }

    // Account-history backfill indexer: builds the FULL durable index (all
    // engine history, from any relayer) and tails the head with authoritative
    // block-global tx/log indexes.
    let indexer_secs = state.config.indexer_poll_secs;
    if indexer_secs > 0 {
        let indexer_state = state.clone();
        tokio::spawn(indexer::run_indexer(indexer_state, indexer_secs));
    }

    // WebSocket push feeds (newHeads + indexed-logs tail).
    let ws_secs = state.config.ws_poll_secs;
    if ws_secs > 0 {
        tokio::spawn(ws::run_pollers(state.clone(), ws_secs));
    }

    let cors = build_cors_layer(&state.config.cors_allowed_origins);
    let max_body = state.config.rpc_max_body_bytes;

    let app = Router::new()
        // POST = HTTP JSON-RPC; GET = WebSocket upgrade (JSON-RPC + eth_subscribe)
        .route("/", post(handle_rpc).get(ws::handle_ws))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// CORS from the configured allowlist. "*" restores the old fully-permissive layer
/// (only sensible behind loopback). Note CORS is a *browser* gate — non-browser
/// clients ignore it; the rate limiter and gas-price floor are the real admission
/// controls.
fn build_cors_layer(allowed: &str) -> CorsLayer {
    // "*" anywhere in the list means permissive: tower-http's AllowOrigin::list
    // panics on a literal "*" entry, so "*,http://host" must not reach it.
    if allowed.split(',').map(str::trim).any(|s| s == "*") {
        warn!("CORS_ALLOWED_ORIGINS contains \"*\" — fully permissive CORS (dev only)");
        return CorsLayer::permissive();
    }
    let origins: Vec<HeaderValue> = allowed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| match HeaderValue::from_str(s) {
            Ok(v) => Some(v),
            Err(_) => {
                warn!(origin = s, "ignoring unparseable CORS origin");
                None
            }
        })
        .collect();
    info!(origins = ?origins, "CORS origin allowlist");
    CorsLayer::new()
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE])
        .allow_origin(origins)
}

async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let client_ip =
        limit::effective_client_ip(client.ip(), &headers, state.config.trust_proxy_headers);
    // Validate batch size BEFORE charging the rate limiter: an over-cap batch is
    // rejected cheaply (charge 1 token, not its length) and must report "batch
    // too large", not "rate limit exceeded" — charging the full length would
    // make any batch larger than RATE_LIMIT_BURST permanently un-servable.
    if let Value::Array(reqs) = &payload
        && reqs.len() > state.config.rpc_max_batch
    {
        state.rate_limiter.allow(client_ip, 1);
        return (
            StatusCode::OK,
            Json(rpc::limit_exceeded_response(
                Value::Null,
                &format!(
                    "batch too large: {} requests (max {})",
                    reqs.len(),
                    state.config.rpc_max_batch
                ),
            )),
        );
    }

    // Admission: one token per JSON-RPC request (batches cost their length).
    let cost = match &payload {
        Value::Array(reqs) => reqs.len(),
        _ => 1,
    };
    if !state.rate_limiter.allow(client_ip, cost) {
        warn!(client = %client_ip, cost = cost, "rate limit exceeded");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(rpc::limit_exceeded_response(
                Value::Null,
                "rate limit exceeded",
            )),
        );
    }

    match payload {
        Value::Array(reqs) => {
            if reqs.is_empty() {
                // Per JSON-RPC 2.0: empty batch → Invalid Request
                return (
                    StatusCode::OK,
                    Json(rpc::invalid_request_response(Value::Null, "empty batch")),
                );
            }
            let mut out = Vec::with_capacity(reqs.len());
            for req in reqs {
                if let Some(resp) = rpc::dispatch(&state, req).await {
                    out.push(resp);
                }
            }
            if out.is_empty() {
                // All requests were notifications → no body per spec.
                // Axum requires a Json body; respond with empty array as a fallback (most clients accept it).
                return (StatusCode::OK, Json(Value::Array(vec![])));
            }
            (StatusCode::OK, Json(Value::Array(out)))
        }
        single => match rpc::dispatch(&state, single).await {
            Some(resp) => (StatusCode::OK, Json(resp)),
            None => (StatusCode::OK, Json(Value::Null)), // notification — no body
        },
    }
}
