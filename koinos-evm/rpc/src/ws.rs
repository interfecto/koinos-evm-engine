//! WebSocket JSON-RPC + eth_subscribe (ROADMAP §5 step 3).
//!
//! GET / upgrades to a WebSocket speaking the same JSON-RPC as the POST route,
//! plus `eth_subscribe`/`eth_unsubscribe` with server-pushed
//! `eth_subscription` notifications. Supported subscription kinds:
//!   - "newHeads": pushed from a head poller (every WS_POLL_SECS).
//!   - "logs" {address?, topics?}: pushed as the durable index settles new
//!     blocks (a DB tail, so pushes lag inclusion by one index cycle — a few
//!     seconds — which also means a subscriber only ever sees logs the HTTP
//!     eth_getLogs path would serve).
//!
//! Unblocks viem `watchBlocks`/`watchEvent` and ethers `provider.on(...)`,
//! which silently fail on HTTP-only providers.

use crate::rpc::{self, log_row_to_json, parse_address_topic_filter};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, warn};

/// Broadcast payload from the pollers to every live connection.
pub enum WsEvent {
    /// Rendered newHeads block object (header-style; no transactions list).
    NewHead(Value),
    /// Newly indexed logs (raw match fields + pre-rendered JSON).
    Logs(Vec<WsLog>),
}

pub struct WsLog {
    pub address: Vec<u8>,
    pub topics: Vec<Vec<u8>>,
    pub rendered: Value,
}

enum SubKind {
    NewHeads,
    Logs {
        addresses: Vec<Vec<u8>>,
        topics: [Vec<Vec<u8>>; 4],
    },
}

static SUB_COUNTER: AtomicU64 = AtomicU64::new(1);

pub async fn handle_ws(
    State(state): State<Arc<AppState>>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Same trusted-proxy resolution as HTTP: behind nginx the peer is loopback
    // for everyone, which would collapse WS rate limiting to one shared bucket.
    let client_ip =
        crate::limit::effective_client_ip(client.ip(), &headers, state.config.trust_proxy_headers);
    ws.on_upgrade(move |socket| connection(state, client_ip, socket))
}

async fn connection(state: Arc<AppState>, client: std::net::IpAddr, mut socket: WebSocket) {
    let mut events = state.ws_events.subscribe();
    let mut subs: HashMap<String, SubKind> = HashMap::new();
    debug!(client = %client, "ws connected");

    loop {
        tokio::select! {
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    Message::Text(text) => {
                        // Same admission as HTTP: one token per JSON-RPC request.
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => {
                                let resp = rpc::invalid_request_response(Value::Null, "parse error");
                                if socket.send(Message::Text(resp.to_string())).await.is_err() { break; }
                                continue;
                            }
                        };
                        let cost = parsed.as_array().map(|a| a.len()).unwrap_or(1);
                        if !state.rate_limiter.allow(client, cost) {
                            let resp = rpc::limit_exceeded_response(Value::Null, "rate limit exceeded");
                            if socket.send(Message::Text(resp.to_string())).await.is_err() { break; }
                            continue;
                        }
                        let responses = match parsed {
                            Value::Array(reqs) => {
                                let mut out = Vec::with_capacity(reqs.len());
                                for req in reqs {
                                    if let Some(r) = handle_request(&state, req, &mut subs).await {
                                        out.push(r);
                                    }
                                }
                                if out.is_empty() { None } else { Some(Value::Array(out)) }
                            }
                            single => handle_request(&state, single, &mut subs).await,
                        };
                        if let Some(resp) = responses
                            && socket.send(Message::Text(resp.to_string())).await.is_err()
                        {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    // Ping/Pong are answered by the protocol layer; ignore binary.
                    _ => {}
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        if push_event(&mut socket, &subs, &ev).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(client = %client, skipped = n, "ws subscriber lagged; events dropped");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    debug!(client = %client, "ws disconnected");
}

/// Handle one JSON-RPC request over WS: intercept (un)subscribe, delegate the
/// rest to the shared dispatcher.
async fn handle_request(
    state: &Arc<AppState>,
    req: Value,
    subs: &mut HashMap<String, SubKind>,
) -> Option<Value> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "eth_subscribe" => {
            let params = req.get("params").cloned().unwrap_or(json!([]));
            let kind = params.get(0).and_then(|v| v.as_str()).unwrap_or("");
            let sub = match kind {
                "newHeads" => SubKind::NewHeads,
                "logs" => {
                    let filter = params.get(1).cloned().unwrap_or(json!({}));
                    match parse_address_topic_filter(&filter) {
                        Ok((addresses, topics)) => SubKind::Logs { addresses, topics },
                        Err(e) => {
                            return Some(error_response(
                                id,
                                -32602,
                                &format!("invalid logs filter: {}", e),
                            ));
                        }
                    }
                }
                other => {
                    return Some(error_response(
                        id,
                        -32602,
                        &format!(
                            "unsupported subscription kind: {:?} (supported: newHeads, logs)",
                            other
                        ),
                    ));
                }
            };
            let sub_id = format!("0x{:x}", SUB_COUNTER.fetch_add(1, Ordering::Relaxed));
            subs.insert(sub_id.clone(), sub);
            Some(json!({"jsonrpc": "2.0", "id": id, "result": sub_id}))
        }
        "eth_unsubscribe" => {
            let sub_id = req
                .get("params")
                .and_then(|p| p.get(0))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let removed = subs.remove(sub_id).is_some();
            Some(json!({"jsonrpc": "2.0", "id": id, "result": removed}))
        }
        _ => rpc::dispatch(state, req).await,
    }
}

async fn push_event(
    socket: &mut WebSocket,
    subs: &HashMap<String, SubKind>,
    ev: &WsEvent,
) -> Result<(), axum::Error> {
    match ev {
        WsEvent::NewHead(head) => {
            for (sub_id, kind) in subs {
                if matches!(kind, SubKind::NewHeads) {
                    socket
                        .send(Message::Text(subscription_msg(sub_id, head).to_string()))
                        .await?;
                }
            }
        }
        WsEvent::Logs(logs) => {
            for (sub_id, kind) in subs {
                let SubKind::Logs { addresses, topics } = kind else {
                    continue;
                };
                for log in logs {
                    if log_matches(addresses, topics, log) {
                        socket
                            .send(Message::Text(
                                subscription_msg(sub_id, &log.rendered).to_string(),
                            ))
                            .await?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn subscription_msg(sub_id: &str, result: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "eth_subscription",
        "params": { "subscription": sub_id, "result": result }
    })
}

fn log_matches(addresses: &[Vec<u8>], topics: &[Vec<Vec<u8>>; 4], log: &WsLog) -> bool {
    if !addresses.is_empty() && !addresses.iter().any(|a| a == &log.address) {
        return false;
    }
    for (i, alternatives) in topics.iter().enumerate() {
        if alternatives.is_empty() {
            continue;
        }
        match log.topics.get(i) {
            Some(t) if alternatives.iter().any(|alt| alt == t) => {}
            _ => return false,
        }
    }
    true
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Head poller + indexed-logs tail. Spawned from main when WS_POLL_SECS > 0.
/// Skipping send errors is fine: broadcast::send only fails with zero
/// receivers, i.e. no live WS connections.
pub async fn run_pollers(state: Arc<AppState>, poll_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs.max(1)));
    let mut last_head: u64 = 0;
    // Start the logs tail at the current index head so a (re)start doesn't
    // replay history at subscribers.
    let mut last_logs_height: u64 = {
        let db = state.db.clone();
        tokio::task::spawn_blocking(move || db.max_indexed_height())
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0)
    };

    loop {
        interval.tick().await;

        // newHeads: push each new height once, in order.
        match head_height(&state).await {
            Ok(height) => {
                if last_head == 0 {
                    last_head = height; // first observation: don't replay
                }
                // Cap per-cycle catch-up so a long gap can't stall the loop.
                let from = (last_head + 1).max(height.saturating_sub(10));
                for h in from..=height {
                    match rpc::fetch_block_as_eth_object(&state, h, false).await {
                        Ok(Value::Null) => {} // not visible yet; retry next tick
                        Ok(mut block) => {
                            // newHeads payloads are headers — no transactions list.
                            if let Some(obj) = block.as_object_mut() {
                                obj.remove("transactions");
                            }
                            let _ = state.ws_events.send(Arc::new(WsEvent::NewHead(block)));
                            last_head = h;
                        }
                        Err(e) => {
                            debug!(error = %e, height = h, "ws head poller: block fetch failed");
                            break;
                        }
                    }
                }
            }
            Err(e) => debug!(error = %e, "ws head poller: head_info failed"),
        }

        // logs: tail the durable index (pushes whatever just settled).
        let db = state.db.clone();
        let from = last_logs_height + 1;
        match tokio::task::spawn_blocking(move || {
            db.query_logs(
                &crate::db::LogFilter {
                    from_block: from,
                    to_block: i64::MAX as u64,
                    ..Default::default()
                },
                1000,
            )
        })
        .await
        {
            Ok(Ok(rows)) if !rows.is_empty() => {
                last_logs_height = rows.iter().map(|r| r.block_height).max().unwrap_or(from);
                let logs: Vec<WsLog> = rows
                    .iter()
                    .map(|row| WsLog {
                        address: row.log.address.clone(),
                        topics: row.log.topics.clone(),
                        rendered: log_row_to_json(row),
                    })
                    .collect();
                let _ = state.ws_events.send(Arc::new(WsEvent::Logs(logs)));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(error = %e, "ws logs tail: query failed"),
            Err(e) => warn!(error = %e, "ws logs tail: join error"),
        }
    }
}

async fn head_height(state: &Arc<AppState>) -> anyhow::Result<u64> {
    let head = state.client.get_head_info().await?;
    head.get("head_topology")
        .and_then(|t| t.get("height"))
        .and_then(|h| h.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("head_info missing height"))
}
