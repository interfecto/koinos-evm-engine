//! Koinos EVM JSON-RPC proxy.
//!
//! Translates Ethereum JSON-RPC requests into Koinos chain RPC / contract calls
//! against our deployed revm engine, and back. Lets MetaMask / Hardhat / web3.js
//! treat Koinos like an Ethereum node.

mod engine_proto;
mod eth_codec;
mod eth_tx;
mod koinos;
mod koinos_tx;
mod rpc;
mod state;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::post,
    Router,
};
use serde_json::Value;
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

    let app = Router::new()
        .route("/", post(handle_rpc))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
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
