//! JSON-RPC method dispatch.

use crate::engine_proto::{
    CallViewArgs, ProtoIter, build_call_view_args, build_get_account_args, build_get_code_args,
    build_get_storage_at_args, decode_account, decode_evm_result,
};
use crate::eth_codec::{
    data, param, param_str, parse_address, parse_b32, parse_data, parse_quantity, parse_u256_be,
    quantity, quantity_from_be32, quantity_u128, u256_be_ge_u128,
};
use crate::koinos::{decode_b64_lax, encode_b64};
use crate::koinos_tx::{build_relayed_tx, encode_nonce};
use crate::state::AppState;
use serde_json::{Value, json};
use sha3::{Digest, Keccak256};
use std::sync::Arc;
use tracing::{debug, info, warn};

// Engine entry-point IDs
const EP_CALL_VIEW: u32 = 0x00000002;
const EP_GET_ACCOUNT: u32 = 0x00000004;
const EP_GET_STORAGE_AT: u32 = 0x00000005;
const EP_GET_CODE: u32 = 0x00000006;
const EP_SUBMIT_RAW_TX: u32 = 0x00000007;

/// Fallback mana ceiling for relayed transactions when RC_LIMIT_MANA env is unset.
/// 200_000_000 raw rc = 2 tKOIN-mana — enough for ERC-20 transfers (~80M observed).
/// For Uniswap/Aave/proxy-init flows you'll want to raise this via env var.
/// Keep tight: mempool reserves the FULL rclimit per pending tx within the IB window.
const FALLBACK_RC_LIMIT_MANA: u64 = 200_000_000;

fn rc_limit_mana() -> u64 {
    std::env::var("RC_LIMIT_MANA")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(FALLBACK_RC_LIMIT_MANA)
}

/// Dispatch a single JSON-RPC request → response Value.
/// Returns `None` for notifications (per JSON-RPC 2.0: request without `id` is a notification
/// and MUST NOT receive a response).
pub async fn dispatch(state: &Arc<AppState>, req: Value) -> Option<Value> {
    if !req.is_object() {
        return Some(error_response(
            Value::Null,
            -32600,
            "request must be an object",
        ));
    }
    let id_present = req.get("id").is_some();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            if !id_present {
                return None;
            }
            return Some(error_response(id, -32600, "missing method"));
        }
    };
    let params = req.get("params").cloned().unwrap_or(Value::Array(vec![]));

    debug!(method = method, "rpc request");

    let result: anyhow::Result<Value> = match method {
        // Static / network info
        "eth_chainId" => Ok(quantity(state.config.evm_chain_id)),
        "net_version" => Ok(json!(state.config.evm_chain_id.to_string())),
        "web3_clientVersion" => Ok(json!("koinos-evm-rpc/0.1.0")),
        // Advertise the admission floor (MIN_GAS_PRICE_WEI) so wallets auto-populate a
        // compliant gas price. The engine still charges 0; with the default floor of 0
        // this is the old behavior.
        "eth_gasPrice" => Ok(quantity_u128(state.config.min_gas_price_wei)),
        "eth_maxPriorityFeePerGas" => Ok(quantity(0)),
        "eth_feeHistory" => Ok(handle_fee_history(state, &params)),

        // Tooling probes (ethers/viem/Hardhat call these on connect)
        "eth_syncing" => Ok(json!(false)),
        "eth_accounts" => Ok(json!([])),
        "net_listening" => Ok(json!(true)),
        "net_peerCount" => Ok(json!("0x1")),
        "eth_getUncleCountByBlockHash" | "eth_getUncleCountByBlockNumber" => Ok(json!("0x0")),
        "eth_getUncleByBlockHashAndIndex" | "eth_getUncleByBlockNumberAndIndex" => Ok(Value::Null),
        "web3_sha3" => handle_web3_sha3(&params),

        "eth_blockNumber" => handle_block_number(state).await,

        // State reads
        "eth_getBalance" => handle_get_balance(state, &params).await,
        "eth_getTransactionCount" => handle_get_tx_count(state, &params).await,
        "eth_getCode" => handle_get_code(state, &params).await,
        "eth_getStorageAt" => handle_get_storage_at(state, &params).await,

        // EVM call
        "eth_call" => handle_eth_call(state, &params).await,

        // Send tx — F4
        "eth_sendRawTransaction" => handle_send_raw_tx(state, &params).await,

        // Receipts — F5
        "eth_getTransactionReceipt" => handle_get_tx_receipt(state, &params).await,
        "eth_getTransactionByHash" => handle_get_tx_by_hash(state, &params).await,

        // Logs (durable store; see handle_get_logs for indexing scope)
        "eth_getLogs" => handle_get_logs(state, &params).await,

        // F6
        "eth_estimateGas" => handle_estimate_gas(state, &params).await,
        "eth_getBlockByNumber" => handle_get_block_by_number(state, &params).await,
        "eth_getBlockByHash" => handle_get_block_by_hash(state, &params).await,

        // Block-level views from the durable index
        "eth_getBlockReceipts" => handle_get_block_receipts(state, &params).await,
        "eth_getBlockTransactionCountByNumber" => {
            handle_block_tx_count(state, &params, false).await
        }
        "eth_getBlockTransactionCountByHash" => handle_block_tx_count(state, &params, true).await,
        "eth_getTransactionByBlockNumberAndIndex" => {
            handle_tx_by_block_and_index(state, &params, false).await
        }
        "eth_getTransactionByBlockHashAndIndex" => {
            handle_tx_by_block_and_index(state, &params, true).await
        }

        _ => {
            warn!(method = method, "unsupported rpc method");
            if !id_present {
                return None;
            }
            return Some(error_response(
                id,
                -32601,
                &format!("method not supported: {}", method),
            ));
        }
    };

    if !id_present {
        // Notification — discard response per spec.
        return None;
    }

    Some(match result {
        Ok(value) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": value
        }),
        Err(e) => {
            // EVM revert: surface as Ethereum-conventional `error.code = 3` with `data = revert_hex`
            // so MetaMask / ethers can decode custom errors. RevertError preserves the raw bytes.
            if let Some(rev) = e.downcast_ref::<RevertError>() {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": 3,
                        "message": "execution reverted",
                        "data": format!("0x{}", hex::encode(&rev.0))
                    }
                })
            } else if let Some(rpc_err) = e.downcast_ref::<RpcError>() {
                error_response(id, rpc_err.code, &rpc_err.message)
            } else if e.to_string().contains("-1013") {
                // Koinos read-compute limit: surface distinguishably as -32005 "limit
                // exceeded" (EIP-1474) so clients can tell "heavy view" from a generic
                // server error. The fix is a raised-read-limit node (ROADMAP §3).
                error_response(
                    id,
                    -32005,
                    &format!("node read-compute limit exceeded (Koinos -1013): {}", e),
                )
            } else {
                // Catch-all server error (upstream Koinos failures, internal errors).
                error_response(id, -32000, &e.to_string())
            }
        }
    })
}

#[derive(Debug)]
pub(crate) struct RevertError(pub Vec<u8>);

impl std::fmt::Display for RevertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "execution reverted: 0x{}", hex::encode(&self.0))
    }
}

impl std::error::Error for RevertError {}

/// A JSON-RPC error with an explicit spec code (EIP-1474), carried through the
/// anyhow chain and unwrapped in `dispatch`. Anything NOT wrapped in one of these
/// (or RevertError) falls back to the -32000 generic server error.
#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// Wrap a parameter-validation failure as -32602 Invalid params.
fn bad_params(e: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(RpcError {
        code: -32602,
        message: format!("invalid params: {}", e),
    })
}

/// -32000 server error with a geth-conventional message (e.g. "transaction underpriced").
fn server_error(msg: String) -> anyhow::Error {
    anyhow::Error::new(RpcError {
        code: -32000,
        message: msg,
    })
}

pub fn invalid_request_response(id: Value, msg: &str) -> Value {
    error_response(id, -32600, msg)
}

/// -32005 "limit exceeded" (EIP-1474) — rate limit / oversized batch.
pub fn limit_exceeded_response(id: Value, msg: &str) -> Value {
    error_response(id, -32005, msg)
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

// ── Method handlers ─────────────────────────────────────────────────────

/// web3_sha3: keccak256 of the given data (a tooling probe, but trivially real).
fn handle_web3_sha3(params: &Value) -> anyhow::Result<Value> {
    let data_s = param_str(params, 0).map_err(bad_params)?;
    let bytes = parse_data(data_s).map_err(bad_params)?;
    let mut hasher = Keccak256::new();
    hasher.update(&bytes);
    let hash: [u8; 32] = hasher.finalize().into();
    Ok(data(&hash))
}

fn handle_fee_history(state: &Arc<AppState>, params: &Value) -> Value {
    // Params: [blockCount, newestBlock, rewardPercentiles?]
    let count = param(params, 0)
        .ok()
        .and_then(|v| {
            v.as_str()
                .and_then(|s| parse_quantity(s).ok())
                .or(v.as_u64())
        })
        .unwrap_or(1) as usize;
    let count = count.clamp(1, 32);
    let percentiles = param(params, 2)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    // Koinos has no Ethereum-style fee market; report the relay's admission floor as
    // the base fee (zero by default) so 1559 wallets derive a compliant maxFeePerGas.
    let floor = quantity_u128(state.config.min_gas_price_wei);
    let base_fees = vec![floor; count + 1];
    let gas_used = vec![0.0_f64; count];
    let mut out = json!({
        "oldestBlock": "0x0",
        "baseFeePerGas": base_fees,
        "gasUsedRatio": gas_used,
    });
    if !percentiles.is_empty() {
        let zero_row: Vec<Value> = percentiles.iter().map(|_| json!("0x0")).collect();
        let reward: Vec<Value> = (0..count).map(|_| Value::Array(zero_row.clone())).collect();
        out["reward"] = Value::Array(reward);
    }
    out
}

async fn handle_block_number(state: &Arc<AppState>) -> anyhow::Result<Value> {
    let head = state.client.get_head_info().await?;
    let height_str = head
        .get("head_topology")
        .and_then(|t| t.get("height"))
        .and_then(|h| h.as_str())
        .ok_or_else(|| anyhow::anyhow!("head_info missing height"))?;
    let height: u64 = height_str.parse()?;
    Ok(quantity(height))
}

async fn handle_get_balance(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let addr_s = param_str(params, 0).map_err(bad_params)?;
    let addr = parse_address(addr_s).map_err(bad_params)?;
    let acct = read_account(state, &addr).await?;
    match acct {
        Some(a) => Ok(quantity_from_be32(&a.balance)),
        None => Ok(json!("0x0")),
    }
}

async fn handle_get_tx_count(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let addr_s = param_str(params, 0).map_err(bad_params)?;
    let addr = parse_address(addr_s).map_err(bad_params)?;
    let on_chain = read_account(state, &addr)
        .await?
        .map(|a| a.nonce)
        .unwrap_or(0);

    // Only the "pending" tag must account for in-flight relayed txs not yet committed.
    // "latest"/"earliest"/numeric/absent → the committed on-chain nonce. Without this,
    // a wallet sending two txs in a row reuses the nonce and the engine rejects the 2nd.
    let block_tag = param_str(params, 1).unwrap_or("latest");
    if block_tag != "pending" {
        return Ok(quantity(on_chain));
    }
    // Hold a single write lock across the read-and-maybe-remove so a concurrent
    // eth_sendRawTransaction can't advance the entry between our read and our remove
    // (which would drop the value it just wrote).
    let mut pn = state.pending_nonce.write().unwrap();
    match pn.get_unexpired(&addr) {
        // In-flight tx(s) ahead of the chain → report the tracked next nonce.
        Some(p) if p > on_chain => Ok(quantity(p)),
        // Chain caught up to/past our tracked pending → the tx(s) committed; drop the
        // stale entry and report the on-chain nonce. A too-high entry that the chain
        // never catches up to (e.g. a manually-submitted future-nonce tx) expires via
        // the store's TTL instead of wedging this sender forever.
        Some(_) => {
            pn.remove(&addr);
            Ok(quantity(on_chain))
        }
        None => Ok(quantity(on_chain)),
    }
}

async fn handle_get_code(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let addr_s = param_str(params, 0).map_err(bad_params)?;
    let addr = parse_address(addr_s).map_err(bad_params)?;
    let args = build_get_code_args(&addr);
    let bytes = state
        .client
        .read_contract(
            &state.config.engine_contract_addr_b58check,
            EP_GET_CODE,
            &encode_b64(&args),
        )
        .await?;
    Ok(data(&bytes))
}

async fn handle_get_storage_at(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let addr_s = param_str(params, 0).map_err(bad_params)?;
    let slot_s = param_str(params, 1).map_err(bad_params)?;
    let addr = parse_address(addr_s).map_err(bad_params)?;
    let slot = parse_b32(slot_s).map_err(bad_params)?;
    let args = build_get_storage_at_args(&addr, &slot);
    let bytes = state
        .client
        .read_contract(
            &state.config.engine_contract_addr_b58check,
            EP_GET_STORAGE_AT,
            &encode_b64(&args),
        )
        .await?;
    Ok(data(&bytes))
}

async fn handle_eth_call(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    // params: [call_object, block_tag]
    let call_obj = param(params, 0).map_err(bad_params)?;
    let from = call_obj.get("from").and_then(|v| v.as_str());
    let to_s = call_obj
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| bad_params(anyhow::anyhow!("eth_call: 'to' required")))?;
    let data_s = call_obj
        .get("data")
        .and_then(|v| v.as_str())
        .unwrap_or("0x");
    let gas_s = call_obj.get("gas").and_then(|v| v.as_str());
    let value_s = call_obj.get("value").and_then(|v| v.as_str());

    let to = parse_address(to_s).map_err(bad_params)?;
    let calldata = if data_s == "0x" || data_s.is_empty() {
        Vec::new()
    } else {
        parse_data(data_s).map_err(bad_params)?
    };
    let caller = if let Some(f) = from {
        Some(parse_address(f).map_err(bad_params)?)
    } else {
        None
    };
    // gas_limit handled above

    // value: U256, big-endian 32 bytes. Default = 0. Errors are propagated (no silent clamp).
    let value_bytes_buf;
    let value_ref: Option<&[u8; 32]> = if let Some(vs) = value_s {
        value_bytes_buf = parse_u256_be(vs).map_err(bad_params)?;
        Some(&value_bytes_buf)
    } else {
        None
    };

    let gas_limit = match gas_s {
        Some(s) => parse_quantity(s).map_err(bad_params)?,
        None => 30_000_000,
    };

    let args = build_call_view_args(&CallViewArgs {
        caller: caller.as_ref(),
        to: Some(&to),
        value: value_ref,
        data: &calldata,
        gas_limit,
    });

    let bytes = state
        .client
        .read_contract(
            &state.config.engine_contract_addr_b58check,
            EP_CALL_VIEW,
            &encode_b64(&args),
        )
        .await?;
    let result = decode_evm_result(&bytes)?;
    if !result.success {
        return Err(anyhow::Error::new(RevertError(result.output)));
    }
    Ok(data(&result.output))
}

async fn handle_send_raw_tx(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let raw_hex = param_str(params, 0).map_err(bad_params)?;
    let raw_tx = parse_data(raw_hex).map_err(bad_params)?;
    if raw_tx.is_empty() {
        return Err(bad_params(anyhow::anyhow!("empty raw_tx")));
    }

    // Ethereum tx hash = keccak256(raw_tx). MetaMask uses this to poll for receipts.
    let mut hasher = Keccak256::new();
    hasher.update(&raw_tx);
    let eth_hash: [u8; 32] = hasher.finalize().into();
    let eth_hash_hex = format!("0x{}", hex::encode(eth_hash));

    // Decode the raw tx so we can store metadata for F5 lookups (from/to/nonce/value/input/...).
    let decoded = crate::eth_tx::decode_and_recover(&raw_tx)
        .map_err(|e| bad_params(anyhow::anyhow!("failed to decode raw tx: {}", e)))?;

    // Admission floor: the sender must commit to at least MIN_GAS_PRICE_WEI per gas
    // (legacy gas_price / 1559 max_fee_per_gas). The engine still charges 0 — this is
    // an anti-spam gate, not a fee market. Wallets auto-comply because eth_gasPrice /
    // eth_feeHistory advertise the floor.
    let floor = state.config.min_gas_price_wei;
    if floor > 0 && !u256_be_ge_u128(&decoded.gas_price, floor) {
        return Err(server_error(format!(
            "transaction underpriced: committed gas price below relay floor of {} wei (set by MIN_GAS_PRICE_WEI)",
            floor
        )));
    }

    // ── Dedupe + placeholder (short critical section) ──────────────────────
    // Lock the operator-nonce gate only long enough to (a) dedupe, (b) record a
    // placeholder tx_meta entry. The placeholder makes dedupe atomic: a concurrent
    // identical raw_tx sees it and returns "already known" instead of double-relaying.
    // The nonce itself is assigned LATER, inside the submit task, while it holds this
    // same gate across the whole chain.submit_transaction round-trip. Submissions are
    // deliberately SERIALIZED: the Koinos mempool admits operator txs strictly in
    // nonce order, so pipelined submits that arrive out of order are rejected with
    // "invalid transaction nonce" (measured live, 2026-06-11: 4 of 5 concurrent
    // relayed txs bounced). One ~10-50 ms round-trip to a local node per tx is the
    // price; a wallet-facing hard failure is not.
    {
        let _nonce_guard = state.operator_nonce.lock().await;
        {
            let map = state.tx_meta.read().unwrap();
            if map.contains(&eth_hash) {
                // NOTE: if the in-flight original later fails and rolls back, this
                // response named a tx that never landed — same semantics as an
                // Ethereum node answering "already known" for a tx that later drops.
                return Ok(json!(eth_hash_hex));
            }
        }
        // Durable dedupe: catches re-submissions after a restart or memory eviction
        // (a sub-ms point lookup; acceptable under the lock for the PoC). Only a
        // SETTLED row blocks the resubmission — a status-NULL row may be a
        // mempool-dropped tx that the sender must be able to resubmit (a re-relay
        // of one that's genuinely still pending just fails the engine nonce check).
        if matches!(state.db.is_settled(&eth_hash), Ok(true)) {
            return Ok(json!(eth_hash_hex));
        }
        // Count the in-flight submit while still under the lock, so the periodic
        // reconciler can't step the nonce down mid-flight.
        state
            .inflight_submits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Placeholder metadata (koinos_tx_id filled in after a successful submit;
        // an empty id makes receipt lookups return null = "pending", which is right).
        let meta = crate::state::TxMeta {
            koinos_tx_id: Vec::new(),
            from: decoded.from,
            to: decoded.to,
            nonce: decoded.nonce,
            value: decoded.value,
            input: decoded.data.clone(),
            gas_limit: decoded.gas_limit,
            gas_price: decoded.gas_price,
            raw_tx: raw_tx.clone(),
            tx_type: decoded.tx_type,
            chain_id: decoded.chain_id,
            r: decoded.r,
            s: decoded.s,
            v: decoded.v,
        };
        state.tx_meta.write().unwrap().insert(eth_hash, meta);
    } // ← gate released; the submit task re-acquires it for the actual send

    // ── Build + submit + settle (detached task) ────────────────────────────
    // CANCELLATION SAFETY: axum drops the request future when the client
    // disconnects. If the submit + settle logic ran inline, a disconnect
    // mid-round-trip would abandon BOTH arms — leaking the nonce reservation
    // and the dedupe placeholder, and (if the tx actually reached the node)
    // never recording its koinos_tx_id, so it would look pending forever.
    // tokio::spawn detaches the work from the request lifetime: it always runs
    // to completion (settle or roll back), whether or not the client is still
    // listening for the response.
    let task_state = state.clone();
    let task_hash_hex = eth_hash_hex.clone();
    let join = tokio::spawn(async move {
        let _inflight = InflightGuard(task_state.clone());
        submit_reserved_tx(&task_state, eth_hash, &task_hash_hex, &decoded, &raw_tx).await
    });
    match join.await {
        Ok(result) => result,
        // JoinError = panic inside the task (it is never cancelled). The
        // InflightGuard still decremented; state heals via resync/reconcile.
        Err(e) => Err(anyhow::anyhow!("submit task failed: {}", e)),
    }
}

/// Decrements `inflight_submits` on drop — runs even if the submit task panics,
/// so the counter can't leak (a stuck nonzero count would permanently disable
/// downward nonce resyncs).
struct InflightGuard(Arc<AppState>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0
            .inflight_submits
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Build, submit, and settle one relayed tx. Runs in a detached task (see
/// handle_send_raw_tx) so it cannot be cancelled mid-settle.
///
/// Holds the operator-nonce gate across the WHOLE submit round-trip. The Koinos
/// mempool admits operator txs strictly in nonce order, so concurrent submits
/// must be serialized — pipelining them produced live "invalid transaction
/// nonce" rejections whenever a user sent two txs back-to-back (approve+swap,
/// swap+paint). Holding the gate also makes the failure path simple: the
/// counter only ever advances on success, so there is no reservation to roll
/// back and no in-flight peer whose nonce a resync could clobber.
async fn submit_reserved_tx(
    state: &Arc<AppState>,
    eth_hash: [u8; 32],
    eth_hash_hex: &str,
    decoded: &crate::eth_tx::DecodedTx,
    raw_tx: &[u8],
) -> anyhow::Result<Value> {
    let mut nonce_guard = state.operator_nonce.lock().await;
    if nonce_guard.is_none() {
        // Lazily fetch the operator's current nonce (startup / post-resync only).
        match fetch_operator_nonce(state).await {
            Ok(n) => *nonce_guard = Some(n),
            Err(e) => {
                state.tx_meta.write().unwrap().remove(&eth_hash);
                return Err(e);
            }
        }
    }

    // On a nonce-related rejection the cache drifted from chain (mempool drop,
    // external operator tx, node restart): resync and retry ONCE. Anything else
    // (or a second nonce failure) bubbles to the wallet.
    let mut resynced = false;
    let relayed = loop {
        let nonce = nonce_guard.unwrap();
        let submit_result = async {
            // Build + sign the relayed Koinos tx. Use the FULL 25-byte base58check-decoded
            // payloads (version + hash160 + checksum) so our hand-encoded header matches what
            // Koinos's JSON deserializer produces (it does NOT strip the version+checksum from
            // ADDRESS/CONTRACT_ID).
            let relayed = build_relayed_tx(
                &state.config.koinos_chain_id_bytes,
                rc_limit_mana(),
                &state.config.operator_addr_full,
                nonce,
                &state.config.engine_contract_addr_full,
                EP_SUBMIT_RAW_TX,
                raw_tx,
                &state.config.operator_key,
            )?;

            // Build the JSON object form of the transaction for chain.submit_transaction.
            let tx_object = build_submit_tx_json(state, &relayed, nonce, raw_tx, rc_limit_mana())?;

            info!(
                eth_hash = %eth_hash_hex,
                nonce = nonce,
                "submitting relayed tx"
            );
            debug!(
                tx_bytes_hex = %hex::encode(&relayed.transaction_bytes),
                tx_id_hex = %hex::encode(&relayed.tx_id_multihash),
                tx_object = %tx_object.to_string(),
                "outgoing submit_transaction body"
            );

            // Debug mode: BROADCAST_FALSE=1 makes submit_transaction skip mempool.
            // Chain still validates the tx (parse + tx.id + merkle + signature) and applies it to
            // a transient state, then returns the receipt. State is NOT persisted to the chain.
            // Useful for proving F4 signing/encoding correctness independent of mempool RC reservations.
            let broadcast = std::env::var("BROADCAST_FALSE").ok().as_deref() != Some("1");

            state
                .client
                .submit_transaction(tx_object, broadcast)
                .await?;
            Ok::<_, anyhow::Error>(relayed)
        }
        .await;

        match submit_result {
            Ok(relayed) => break relayed,
            Err(e) if is_nonce_related_err(&e) && !resynced => {
                resynced = true;
                match fetch_operator_nonce(state).await {
                    Ok(n) => {
                        warn!(
                            stale = nonce,
                            resynced = n,
                            "operator nonce resynced after nonce-related submit error; retrying once"
                        );
                        *nonce_guard = Some(n);
                        continue;
                    }
                    Err(fetch_err) => {
                        warn!(error = %fetch_err, "operator nonce resync failed; clearing cache for lazy re-fetch");
                        *nonce_guard = None;
                        state.tx_meta.write().unwrap().remove(&eth_hash);
                        return Err(e);
                    }
                }
            }
            Err(e) => {
                if is_nonce_related_err(&e) {
                    // Second nonce failure in a row — leave the cache for the lazy
                    // re-fetch / periodic reconciler rather than trusting it.
                    *nonce_guard = None;
                }
                // Drop the placeholder so the tx can be resubmitted.
                state.tx_meta.write().unwrap().remove(&eth_hash);
                return Err(e);
            }
        }
    };

    // Success: advance the counter, then release the gate before the settle
    // bookkeeping (none of it touches the nonce).
    *nonce_guard = Some(nonce_guard.unwrap() + 1);
    drop(nonce_guard);

    state
        .last_submit_ok
        .store(now_unix_secs(), std::sync::atomic::Ordering::Relaxed);

    // Fill in the real koinos_tx_id on the placeholder.
    let mut koinos_id = [0u8; 32];
    koinos_id.copy_from_slice(&relayed.tx_id_multihash[2..34]);
    state
        .tx_meta
        .write()
        .unwrap()
        .set_koinos_tx_id(&eth_hash, koinos_id.to_vec());

    // Persist the relayed tx (restart-safe lookups; receipt fields settle
    // later via the poller / receipt reads). Best-effort: a db error must
    // not fail a tx that the chain already accepted.
    {
        let db = state.db.clone();
        let meta = state.tx_meta.read().unwrap().get(&eth_hash);
        if let Some(meta) = meta {
            tokio::task::spawn_blocking(move || {
                if let Err(e) = db.upsert_tx(&eth_hash, &meta) {
                    warn!(error = %e, "db upsert_tx failed");
                }
            });
        }
    }

    // Advance this sender's pending nonce so a follow-up
    // eth_getTransactionCount(addr, "pending") sees this in-flight tx and the
    // wallet picks nonce+1 (the engine enforces strictly sequential nonces).
    {
        let next = decoded.nonce.saturating_add(1);
        let mut pn = state.pending_nonce.write().unwrap();
        pn.advance(decoded.from, next);
    }

    Ok(json!(format!("0x{}", hex::encode(eth_hash))))
}

/// Heuristic: does a Koinos submit error indicate an operator-nonce mismatch?
/// Koinos node/mempool errors surface as JSON in the message (koinos.rs); nonce
/// failures mention the word "nonce" (e.g. "invalid account nonce").
fn is_nonce_related_err(e: &anyhow::Error) -> bool {
    e.to_string().to_lowercase().contains("nonce")
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Periodic operator-nonce reconciliation (spawned from main).
///
/// Two desync directions:
///   - chain AHEAD of cache: someone/something else used the operator account, or our
///     accounting fell behind. Always safe to jump forward — our own in-flight txs can
///     only ever advance the chain UP TO the cached value, never past it.
///   - cache AHEAD of chain: normal while our txs sit in the mempool (≈3 s blocks), so
///     only step DOWN after a quiet period (2× the reconcile interval) with no successful
///     submits — at that point the gap means recorded-success txs never actually landed.
///     If a tx is somehow still floating after the quiet period, the duplicate-nonce
///     submit error triggers the error-path resync above, so this self-heals either way.
pub async fn reconcile_operator_nonce(state: &Arc<AppState>, quiet_period_secs: u64) {
    let chain_next = match fetch_operator_nonce(state).await {
        Ok(n) => n,
        Err(e) => {
            debug!(error = %e, "nonce reconcile: chain nonce fetch failed; skipping cycle");
            return;
        }
    };
    let mut guard = state.operator_nonce.lock().await;
    match *guard {
        // Cache not populated — leave it; the first submit fetches lazily.
        None => {}
        Some(cached) if chain_next > cached => {
            warn!(
                cached = cached,
                chain = chain_next,
                "nonce reconcile: chain ahead of cache; resyncing"
            );
            *guard = Some(chain_next);
        }
        Some(cached) if chain_next < cached => {
            let last_ok = state
                .last_submit_ok
                .load(std::sync::atomic::Ordering::Relaxed);
            let quiet_for = now_unix_secs().saturating_sub(last_ok);
            // Step down only when BOTH (a) no submit round-trip is in flight —
            // last_submit_ok alone is blind to a reservation whose network call
            // is still pending — and (b) the quiet period has elapsed (covers
            // submitted-but-not-yet-committed mempool txs).
            let inflight = state
                .inflight_submits
                .load(std::sync::atomic::Ordering::SeqCst);
            if inflight == 0 && quiet_for >= quiet_period_secs {
                warn!(
                    cached = cached,
                    chain = chain_next,
                    quiet_secs = quiet_for,
                    "nonce reconcile: cache ahead of chain after quiet period; stepping down"
                );
                *guard = Some(chain_next);
            }
        }
        // In sync — nothing to do.
        Some(_) => {}
    }
}

/// Build the JSON object representation of the relayed transaction for chain.submit_transaction.
///
/// Koinos JSON convention:
///   - bytes with (btype) = ADDRESS / CONTRACT_ID → base58 string
///   - other bytes → URL-safe base64 (with padding)
///   - uint64 → string (jstype = JS_STRING)
///   - nested messages → JSON objects
fn build_submit_tx_json(
    state: &Arc<AppState>,
    relayed: &crate::koinos_tx::RelayedTx,
    nonce: u64,
    raw_eth_tx: &[u8],
    rc_limit: u64,
) -> anyhow::Result<Value> {
    // raw_tx wrapped in submit_raw_tx args proto: { bytes raw_tx = 1 }
    let submit_args = crate::engine_proto::build_submit_raw_tx_args(raw_eth_tx);

    let nonce_bytes = encode_nonce(nonce);

    // All values pulled directly from RelayedTx — no fragile recomputation.
    // Empirically: Koinos JSON-RPC chain.submit_transaction wants the BASE58CHECK form
    // (human-readable, with version byte + checksum) for BOTH payer AND contract_id,
    // NOT the raw base58 of hash160. Verified by capturing koinos-cli's wire format.
    let header = json!({
        "chain_id": encode_b64(&state.config.koinos_chain_id_bytes),
        "rc_limit": rc_limit.to_string(),
        "nonce": encode_b64(&nonce_bytes),
        "operation_merkle_root": encode_b64(&relayed.operation_merkle_root),
        "payer": state.config.operator_addr_b58check.clone(),
    });

    let call_contract = json!({
        "contract_id": state.config.engine_contract_addr_b58check.clone(),
        "entry_point": EP_SUBMIT_RAW_TX,
        "args": encode_b64(&submit_args),
    });

    // transaction.id has (btype) = TRANSACTION_ID → encoded as hex with 0x prefix in JSON
    let tx_id_hex = format!("0x{}", hex::encode(&relayed.tx_id_multihash));

    let tx_object = json!({
        "id": tx_id_hex,
        "header": header,
        "operations": [{ "call_contract": call_contract }],
        "signatures": [encode_b64(&relayed.signature)],
    });

    Ok(tx_object)
}

/// Strip the Koinos `0x1220` multihash prefix from a hash string, leaving a 32-byte Ethereum hash.
/// Ethereum spec mandates 32-byte hashes (`DATA(32)`); MetaMask/ethers reject longer.
/// Accepts either prefixed or already-stripped input.
fn strip_koinos_hash_prefix(hash: &Value) -> Value {
    let s = match hash.as_str() {
        Some(s) => s,
        None => return hash.clone(),
    };
    let stripped = s.strip_prefix("0x1220").map(|rest| format!("0x{}", rest));
    Value::String(stripped.unwrap_or_else(|| s.to_string()))
}

/// Reverse: take a possibly-stripped 32-byte hex and turn it back into a Koinos `0x1220...`
/// multihash for REST lookups.
fn restore_koinos_hash_prefix(eth_hex: &str) -> String {
    if eth_hex.starts_with("0x1220") {
        eth_hex.to_string()
    } else if let Some(rest) = eth_hex.strip_prefix("0x") {
        format!("0x1220{}", rest)
    } else {
        format!("0x1220{}", eth_hex)
    }
}

async fn handle_estimate_gas(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    // Same shape as eth_call: [{from, to, value, data, gas}, blockTag?]
    let call_obj = param(params, 0).map_err(bad_params)?;
    let from = call_obj.get("from").and_then(|v| v.as_str());
    let to_s = call_obj.get("to").and_then(|v| v.as_str());
    let data_s = call_obj
        .get("data")
        .and_then(|v| v.as_str())
        .unwrap_or("0x");
    let value_s = call_obj.get("value").and_then(|v| v.as_str());

    let caller = if let Some(f) = from {
        Some(parse_address(f).map_err(bad_params)?)
    } else {
        None
    };
    let to = if let Some(t) = to_s {
        Some(parse_address(t).map_err(bad_params)?)
    } else {
        None
    };
    let calldata = if data_s == "0x" || data_s.is_empty() {
        Vec::new()
    } else {
        parse_data(data_s).map_err(bad_params)?
    };
    let value_buf;
    let value_ref: Option<&[u8; 32]> = if let Some(vs) = value_s {
        value_buf = parse_u256_be(vs).map_err(bad_params)?;
        Some(&value_buf)
    } else {
        None
    };

    let args = build_call_view_args(&CallViewArgs {
        caller: caller.as_ref(),
        to: to.as_ref(),
        value: value_ref,
        data: &calldata,
        gas_limit: 30_000_000,
    });

    let bytes = match state
        .client
        .read_contract(
            &state.config.engine_contract_addr_b58check,
            EP_CALL_VIEW,
            &encode_b64(&args),
        )
        .await
    {
        Ok(b) => b,
        // The estimation view trips the node's read-compute limit (-1013) for
        // even small writes on a default public node (revm-in-WASM is an
        // interpreter inside an interpreter). Rather than breaking every
        // wallet's send flow, return a generous configured estimate: users pay
        // zero gas and relay mana is independent of the EVM gas figure, so
        // over-estimating costs nothing. A raised-read-limit node (ROADMAP §3)
        // makes real estimates work; this fallback never fires there.
        Err(e) if state.config.estimate_gas_fallback > 0 && e.to_string().contains("-1013") => {
            debug!(
                fallback = state.config.estimate_gas_fallback,
                "estimateGas view hit node read-compute limit; returning configured fallback"
            );
            return Ok(quantity(state.config.estimate_gas_fallback));
        }
        Err(e) => return Err(e),
    };
    let result = decode_evm_result(&bytes)?;
    if !result.success {
        return Err(anyhow::Error::new(RevertError(result.output)));
    }
    // 20% headroom: gas estimation isn't deterministic across state changes
    let estimate = result.gas_used.saturating_mul(120) / 100;
    Ok(quantity(estimate.max(21_000)))
}

/// Resolve a getBlockByNumber-style tag to a height (network call only for tags).
async fn resolve_block_number_tag(state: &Arc<AppState>, tag: &str) -> anyhow::Result<u64> {
    match tag {
        "latest" | "pending" | "safe" | "finalized" => {
            let head = state.client.get_head_info().await?;
            head.get("head_topology")
                .and_then(|t| t.get("height"))
                .and_then(|h| h.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("head_info missing height"))
        }
        "earliest" => Ok(1),
        s if s.starts_with("0x") => parse_quantity(s).map_err(bad_params),
        _ => Err(bad_params(anyhow::anyhow!(
            "unsupported block tag: {}",
            tag
        ))),
    }
}

async fn handle_get_block_by_number(
    state: &Arc<AppState>,
    params: &Value,
) -> anyhow::Result<Value> {
    // params: [block_tag, full_tx_objects?]
    let tag = param_str(params, 0).map_err(bad_params)?;
    let full_txs = param(params, 1)
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let height = resolve_block_number_tag(state, tag).await?;
    fetch_block_as_eth_object(state, height, full_txs).await
}

async fn handle_get_block_by_hash(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let hash_s = param_str(params, 0).map_err(bad_params)?;
    let full_txs = param(params, 1)
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Accept both Ethereum 32-byte hash (we restore the Koinos multihash prefix) and
    // Koinos multihash strings.
    let koinos_hash = restore_koinos_hash_prefix(hash_s);
    let height = match fetch_block_height(state, &koinos_hash).await {
        Some(h) => h,
        None => return Ok(Value::Null),
    };
    fetch_block_as_eth_object(state, height, full_txs).await
}

pub(crate) async fn fetch_block_as_eth_object(
    state: &Arc<AppState>,
    height: u64,
    full_txs: bool,
) -> anyhow::Result<Value> {
    // Koinos REST: /block/<height_or_id>
    let resp = match state.client.rest(&format!("/block/{}", height)).await {
        Ok(j) => j,
        Err(_) => return Ok(Value::Null),
    };
    let block = match resp.get("block") {
        Some(b) => b,
        None => return Ok(Value::Null),
    };
    let header = block.get("header").cloned().unwrap_or(Value::Null);
    let block_id = strip_koinos_hash_prefix(&block.get("id").cloned().unwrap_or(Value::Null));
    let prev = strip_koinos_hash_prefix(&header.get("previous").cloned().unwrap_or(Value::Null));
    let timestamp_ms = header
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let timestamp_sec = timestamp_ms / 1000;
    let num_txs = block
        .get("transactions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    // Body from the durable index: EVM tx list, real gasUsed, block bloom.
    // Completeness tracks the indexer (full once the backfill has run).
    let (tx_hashes, gas_used, block_bloom) = {
        let db = state.db.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let hashes = db.block_tx_hashes(height)?;
            let gas = db.block_gas_used(height)?;
            let mut bloom = crate::bloom::Bloom::default();
            for log in db.block_logs(height)? {
                bloom.union(&crate::bloom::log_bloom(&log.address, &log.topics));
            }
            Ok((hashes, gas, bloom))
        })
        .await??
    };
    let transactions: Vec<Value> = if full_txs {
        let mut out = Vec::with_capacity(tx_hashes.len());
        for h in &tx_hashes {
            let hash_s = format!("0x{}", hex::encode(h));
            // Reuse the canonical tx-object renderer via the dispatch handler.
            let v = handle_get_tx_by_hash(state, &json!([hash_s])).await?;
            if !v.is_null() {
                out.push(v);
            }
        }
        out
    } else {
        tx_hashes
            .iter()
            .map(|h| json!(format!("0x{}", hex::encode(h))))
            .collect()
    };

    Ok(json!({
        "number": quantity(height),
        "hash": block_id,
        "parentHash": prev,
        "timestamp": quantity(timestamp_sec),
        "gasLimit": quantity(30_000_000),
        "gasUsed": quantity(gas_used),
        "miner": "0x0000000000000000000000000000000000000000",
        "difficulty": "0x0",
        "totalDifficulty": "0x0",
        "size": quantity(num_txs as u64 * 200),
        "nonce": "0x0000000000000000",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
        "uncles": [],
        "transactions": transactions,
        "stateRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "transactionsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "logsBloom": block_bloom.to_hex(),
        "extraData": "0x",
        // Advertise the admission floor as the base fee: 1559 wallets (viem,
        // ethers) derive maxFeePerGas from the latest block's baseFeePerGas, so
        // leaving this at 0 while enforcing MIN_GAS_PRICE_WEI would make every
        // wallet-built tx "underpriced". 0 by default (floor disabled).
        "baseFeePerGas": quantity_u128(state.config.min_gas_price_wei),
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    }))
}

/// eth_getBlockReceipts: all EVM receipts in a block, from the durable index.
async fn handle_get_block_receipts(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let tag = param_str(params, 0).map_err(bad_params)?;
    // Accept a 32-byte block hash as well as number/tag (spec: blockNrOrHash).
    let height = if tag.len() == 66 && tag.starts_with("0x") {
        match fetch_block_height(state, &restore_koinos_hash_prefix(tag)).await {
            Some(h) => h,
            None => return Ok(Value::Null),
        }
    } else {
        resolve_block_number_tag(state, tag).await?
    };
    let db = state.db.clone();
    let hashes = tokio::task::spawn_blocking(move || db.block_tx_hashes(height)).await??;
    let mut out = Vec::with_capacity(hashes.len());
    for h in hashes {
        let meta = match load_meta(state, &h).await {
            Some(m) => m,
            None => continue,
        };
        let db = state.db.clone();
        if let Ok(Ok(Some(r))) = tokio::task::spawn_blocking(move || db.get_receipt(&h)).await {
            out.push(receipt_to_json(&format!("0x{}", hex::encode(h)), &meta, &r));
        }
    }
    Ok(Value::Array(out))
}

/// eth_getBlockTransactionCountBy{Number,Hash}: EVM tx count from the index.
async fn handle_block_tx_count(
    state: &Arc<AppState>,
    params: &Value,
    by_hash: bool,
) -> anyhow::Result<Value> {
    let tag = param_str(params, 0).map_err(bad_params)?;
    let height = if by_hash {
        match fetch_block_height(state, &restore_koinos_hash_prefix(tag)).await {
            Some(h) => h,
            None => return Ok(Value::Null),
        }
    } else {
        resolve_block_number_tag(state, tag).await?
    };
    let db = state.db.clone();
    let hashes = tokio::task::spawn_blocking(move || db.block_tx_hashes(height)).await??;
    Ok(quantity(hashes.len() as u64))
}

/// eth_getTransactionByBlock{Number,Hash}AndIndex.
async fn handle_tx_by_block_and_index(
    state: &Arc<AppState>,
    params: &Value,
    by_hash: bool,
) -> anyhow::Result<Value> {
    let tag = param_str(params, 0).map_err(bad_params)?;
    let idx = parse_quantity(param_str(params, 1).map_err(bad_params)?).map_err(bad_params)?;
    let height = if by_hash {
        match fetch_block_height(state, &restore_koinos_hash_prefix(tag)).await {
            Some(h) => h,
            None => return Ok(Value::Null),
        }
    } else {
        resolve_block_number_tag(state, tag).await?
    };
    let db = state.db.clone();
    let hashes = tokio::task::spawn_blocking(move || db.block_tx_hashes(height)).await??;
    match hashes.get(idx as usize) {
        Some(h) => {
            let hash_s = format!("0x{}", hex::encode(h));
            handle_get_tx_by_hash(state, &json!([hash_s])).await
        }
        None => Ok(Value::Null),
    }
}

/// Load tx metadata: in-memory store first (hot path), durable store second
/// (restart-safe path).
async fn load_meta(state: &Arc<AppState>, hash: &[u8; 32]) -> Option<crate::state::TxMeta> {
    if let Some(m) = state.tx_meta.read().unwrap().get(hash) {
        return Some(m);
    }
    let db = state.db.clone();
    let h = *hash;
    match tokio::task::spawn_blocking(move || db.get_tx_meta(&h)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            warn!(error = %e, "db get_tx_meta failed");
            None
        }
        Err(e) => {
            warn!(error = %e, "db get_tx_meta join error");
            None
        }
    }
}

async fn handle_get_tx_by_hash(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let hash_s = param_str(params, 0).map_err(bad_params)?;
    let hash = parse_b32(hash_s).map_err(bad_params)?;
    let meta = match load_meta(state, &hash).await {
        Some(m) => m,
        // Unknown tx: per Ethereum convention return null (not error)
        None => return Ok(Value::Null),
    };

    // Inclusion info: durable store first, live Koinos lookup as fallback.
    let (block_hash_eth, block_number, tx_index) = {
        let db = state.db.clone();
        let stored = tokio::task::spawn_blocking(move || db.get_receipt(&hash))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
        match stored {
            Some(r) => (
                Value::String(format!("0x{}", hex::encode(&r.block_hash))),
                quantity(r.block_height),
                Some(r.tx_index.unwrap_or(0)),
            ),
            None if !meta.koinos_tx_id.is_empty() => {
                let (bh, bn) = fetch_koinos_inclusion(state, &meta.koinos_tx_id).await;
                let bh = bh
                    .as_ref()
                    .map(strip_koinos_hash_prefix)
                    .unwrap_or(Value::Null);
                let included = !bh.is_null();
                (bh, bn.unwrap_or(Value::Null), included.then_some(0))
            }
            None => (Value::Null, Value::Null, None),
        }
    };

    Ok(json!({
        "hash": hash_s,
        "from": format!("0x{}", hex::encode(meta.from)),
        "to": meta.to.map(|a| Value::String(format!("0x{}", hex::encode(a)))).unwrap_or(Value::Null),
        "nonce": quantity(meta.nonce),
        "value": quantity_from_be32(&meta.value),
        "input": data(&meta.input),
        "gas": quantity(meta.gas_limit),
        // Legacy txs: the sender-provided gas price (spec). Type-2 txs: the spec
        // field is the EFFECTIVE price paid, which this chain defines as 0 — and
        // must agree with the receipt's effectiveGasPrice (the committed
        // max_fee_per_gas would contradict it).
        "gasPrice": if meta.tx_type == 0 { quantity_from_be32(&meta.gas_price) } else { quantity(0) },
        "type": format!("0x{:x}", meta.tx_type),
        "chainId": meta.chain_id.map(quantity).unwrap_or(json!(null)),
        "blockHash": block_hash_eth,
        "blockNumber": block_number,
        "transactionIndex": tx_index.map(|i| json!(format!("0x{:x}", i))).unwrap_or(Value::Null),
        "r": data(&meta.r),
        "s": data(&meta.s),
        "v": quantity(meta.v),
    }))
}

/// Fetch + decode an included tx's receipt from the Koinos node.
/// Returns Ok(None) while the tx is not yet included/indexed.
///
/// Status + gas_used + contract_address come from the engine's `evm.result` event,
/// NOT from Koinos's compute_bandwidth_used (which is Koinos VM compute cost, not
/// EVM gas); logs come from `evm.log` events.
pub(crate) async fn fetch_receipt_from_chain(
    state: &Arc<AppState>,
    koinos_tx_id: &[u8],
) -> anyhow::Result<Option<crate::db::ReceiptRow>> {
    if koinos_tx_id.is_empty() {
        return Ok(None);
    }
    let koinos_tx_id_hex = format!("0x1220{}", hex::encode(koinos_tx_id));
    let receipt_json = match state
        .client
        .rest(&format!("/transaction/{}", koinos_tx_id_hex))
        .await
    {
        Ok(j) => j,
        // Tx not yet indexed → not included yet (clients keep polling)
        Err(_) => return Ok(None),
    };
    let receipt = receipt_json.get("receipt");
    let containing_blocks = receipt_json
        .get("containing_blocks")
        .and_then(|v| v.as_array());
    if receipt.is_none() || containing_blocks.is_none_or(|a| a.is_empty()) {
        return Ok(None);
    }
    let receipt = receipt.unwrap();
    let block_hash_koinos = containing_blocks
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Fetch the containing block to get its height (uses the FULL Koinos multihash for REST)
    let block_height = match fetch_block_height(state, &block_hash_koinos).await {
        Some(h) => h,
        // Block not resolvable yet — treat as not-included; the next poll settles it.
        None => return Ok(None),
    };
    // Strip the Koinos multihash prefix at the Ethereum boundary.
    let block_hash_hex = block_hash_koinos
        .strip_prefix("0x1220")
        .unwrap_or(block_hash_koinos.trim_start_matches("0x"));
    let block_hash = hex::decode(block_hash_hex).unwrap_or_default();

    let ev = decode_receipt_events(receipt.get("events").and_then(|v| v.as_array()));

    Ok(Some(crate::db::ReceiptRow {
        block_height,
        block_hash,
        tx_index: None, // provisional; the indexer assigns the real value
        status: ev.status,
        gas_used: ev.gas_used,
        contract_address: ev.contract_address,
        logs: ev.logs,
    }))
}

/// Decoded `evm.result` / `evm.log` events from a Koinos receipt's `events`
/// array (same JSON shape from REST /transaction and account_history).
pub(crate) struct DecodedEvents {
    pub status: bool,
    pub gas_used: u64,
    pub contract_address: Option<Vec<u8>>,
    /// Logs with PER-TX sequential log_index (0..) over evm.log events only.
    pub logs: Vec<crate::db::LogRow>,
}

/// Status + gas_used + contract_address come from the engine's `evm.result`
/// event, NOT from Koinos's compute_bandwidth_used (which is Koinos VM compute
/// cost, not EVM gas); logs come from `evm.log` events.
pub(crate) fn decode_receipt_events(events: Option<&Vec<Value>>) -> DecodedEvents {
    let mut out = DecodedEvents {
        status: true,
        gas_used: 0,
        contract_address: None,
        logs: Vec::new(),
    };
    let Some(events) = events else {
        return out;
    };
    let mut log_index: u64 = 0;
    for ev in events {
        let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let data_b64 = ev.get("data").and_then(|v| v.as_str()).unwrap_or("");
        match name {
            "evm.result" => {
                let bytes = match decode_b64_lax(data_b64) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // EvmResultEvent { bool success=1, uint64 gas_used=2, bytes contract_address=3 }
                for f in ProtoIter::new(&bytes).flatten() {
                    match (f.field, f.wtype) {
                        (1, 0) => {
                            if f.varint == 0 {
                                out.status = false;
                            }
                        }
                        (2, 0) => out.gas_used = f.varint,
                        (3, 2) if f.payload.len() == 20 => {
                            out.contract_address = Some(f.payload.to_vec());
                        }
                        _ => {}
                    }
                }
            }
            "evm.log" => {
                let bytes = match decode_b64_lax(data_b64) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // EvmLog { bytes address=1, repeated bytes topics=2, bytes data=3 }
                let mut address: Vec<u8> = Vec::new();
                let mut topics: Vec<Vec<u8>> = Vec::new();
                let mut log_data: Vec<u8> = Vec::new();
                for f in ProtoIter::new(&bytes).flatten() {
                    match (f.field, f.wtype) {
                        (1, 2) => {
                            if f.payload.len() == 20 {
                                address = f.payload.to_vec();
                            }
                        }
                        (2, 2) => topics.push(f.payload.to_vec()),
                        (3, 2) => log_data = f.payload.to_vec(),
                        _ => {}
                    }
                }
                out.logs.push(crate::db::LogRow {
                    // Per-tx index over evm.log events only (the old code indexed
                    // over ALL receipt events, so indexes could skip).
                    log_index,
                    address,
                    topics,
                    data: log_data,
                });
                log_index += 1;
            }
            _ => {}
        }
    }
    out
}

/// Render an Ethereum receipt object from tx metadata + a (chain- or db-sourced)
/// receipt row, including a real logsBloom.
fn receipt_to_json(hash_s: &str, meta: &crate::state::TxMeta, r: &crate::db::ReceiptRow) -> Value {
    let block_hash = Value::String(format!("0x{}", hex::encode(&r.block_hash)));
    let block_number = quantity(r.block_height);
    // Provisionally-settled receipts (indexer hasn't visited yet) report 0.
    let tx_index = format!("0x{:x}", r.tx_index.unwrap_or(0));

    let mut bloom = crate::bloom::Bloom::default();
    let logs: Vec<Value> = r
        .logs
        .iter()
        .map(|log| {
            bloom.union(&crate::bloom::log_bloom(&log.address, &log.topics));
            let topics: Vec<String> = log
                .topics
                .iter()
                .map(|t| format!("0x{}", hex::encode(t)))
                .collect();
            json!({
                "address": format!("0x{}", hex::encode(&log.address)),
                "topics": topics,
                "data": format!("0x{}", hex::encode(&log.data)),
                "blockHash": block_hash.clone(),
                "blockNumber": block_number.clone(),
                "transactionHash": hash_s,
                "transactionIndex": tx_index.clone(),
                "logIndex": format!("0x{:x}", log.log_index),
                "removed": false,
            })
        })
        .collect();

    // contractAddress: prefer the event-emitted address (handles CREATE2 correctly).
    // Fall back to keccak256(rlp([sender, nonce]))[12..] for plain CREATE.
    let contract_address = match &r.contract_address {
        Some(addr) => Value::String(format!("0x{}", hex::encode(addr))),
        None if meta.to.is_none() => {
            let addr = create_address(&meta.from, meta.nonce);
            Value::String(format!("0x{}", hex::encode(addr)))
        }
        _ => Value::Null,
    };

    json!({
        "transactionHash": hash_s,
        "transactionIndex": tx_index,
        "blockHash": block_hash,
        "blockNumber": block_number,
        "from": format!("0x{}", hex::encode(meta.from)),
        "to": meta.to.map(|a| Value::String(format!("0x{}", hex::encode(a)))).unwrap_or(Value::Null),
        "cumulativeGasUsed": quantity(r.gas_used),
        "gasUsed": quantity(r.gas_used),
        "contractAddress": contract_address,
        "logs": logs,
        "logsBloom": bloom.to_hex(),
        "status": if r.status { "0x1" } else { "0x0" },
        "type": format!("0x{:x}", meta.tx_type),
        "effectiveGasPrice": "0x0",
    })
}

async fn handle_get_tx_receipt(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let hash_s = param_str(params, 0).map_err(bad_params)?;
    let hash = parse_b32(hash_s).map_err(bad_params)?;
    let meta = match load_meta(state, &hash).await {
        Some(m) => m,
        None => return Ok(Value::Null),
    };

    // Durable store first: settled receipts survive proxy restarts.
    {
        let db = state.db.clone();
        if let Ok(Ok(Some(stored))) =
            tokio::task::spawn_blocking(move || db.get_receipt(&hash)).await
        {
            return Ok(receipt_to_json(hash_s, &meta, &stored));
        }
    }

    let fetched = fetch_receipt_from_chain(state, &meta.koinos_tx_id).await?;
    match fetched {
        // Not yet included → null (MetaMask keeps polling)
        None => Ok(Value::Null),
        Some(receipt) => {
            // Best-effort persist so the receipt outlives memory + restarts.
            // Upsert the tx row first: set_receipt requires the parent row, and
            // the fire-and-forget upsert at submit time may have failed.
            let db = state.db.clone();
            let row = receipt.clone();
            let meta_for_db = meta.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || {
                db.upsert_tx(&hash, &meta_for_db)
                    .and_then(|_| db.set_receipt(&hash, &row))
            })
            .await
            {
                warn!(error = %e, "db receipt persist failed");
            }
            Ok(receipt_to_json(hash_s, &meta, &receipt))
        }
    }
}

/// Background poller: settle receipts for submitted-but-unsettled txs into the
/// durable store so they survive restarts even if no client polls for them.
pub async fn poll_pending_receipts(state: &Arc<AppState>) {
    let db = state.db.clone();
    let pending = match tokio::task::spawn_blocking(move || db.pending_txs(25)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            warn!(error = %e, "receipt poller: pending_txs query failed");
            return;
        }
        Err(e) => {
            warn!(error = %e, "receipt poller: join error");
            return;
        }
    };
    for (eth_hash, koinos_tx_id) in pending {
        match fetch_receipt_from_chain(state, &koinos_tx_id).await {
            Ok(Some(receipt)) => {
                let db = state.db.clone();
                match tokio::task::spawn_blocking(move || db.set_receipt(&eth_hash, &receipt)).await
                {
                    Ok(Ok(())) => {
                        debug!(eth_hash = %hex::encode(eth_hash), "receipt poller: settled");
                    }
                    Ok(Err(e)) => warn!(error = %e, "receipt poller: set_receipt failed"),
                    Err(e) => warn!(error = %e, "receipt poller: join error"),
                }
            }
            // Still pending (or fetch failed): count the attempt so never-included
            // zombies age out of the poll window instead of starving newer txs.
            Ok(None) | Err(_) => {
                let db = state.db.clone();
                if let Ok(Err(e)) =
                    tokio::task::spawn_blocking(move || db.bump_poll_attempts(&eth_hash)).await
                {
                    warn!(error = %e, "receipt poller: bump_poll_attempts failed");
                }
            }
        }
    }
}

// ── eth_getLogs ─────────────────────────────────────────────────────────

/// Resolve a block-tag param ("latest"/"pending"/"earliest"/hex/number) to a height.
/// `head` is fetched lazily by the caller only when needed.
fn resolve_block_tag(tag: &Value, head: u64) -> anyhow::Result<u64> {
    match tag {
        Value::String(s) => match s.as_str() {
            "latest" | "pending" | "safe" | "finalized" => Ok(head),
            "earliest" => Ok(1),
            hex if hex.starts_with("0x") => parse_quantity(hex),
            other => Err(anyhow::anyhow!("unsupported block tag: {}", other)),
        },
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("block number out of range")),
        _ => Err(anyhow::anyhow!("invalid block tag")),
    }
}

/// eth_getLogs over the durable log index.
///
/// Honest scope note: the index covers txs relayed through THIS proxy (settled by
/// the receipt poller / receipt lookups) since persistence was enabled — there is
/// no backfill of older on-chain history yet (ROADMAP §2 indexer).
/// Address allowlist + per-position topic OR-lists of a log filter ("any" = empty).
pub(crate) type AddressTopicFilter = (Vec<Vec<u8>>, [Vec<Vec<u8>>; 4]);

/// Parse the {address, topics} part of an eth_getLogs / eth_subscribe("logs")
/// filter object.
pub(crate) fn parse_address_topic_filter(filter_obj: &Value) -> anyhow::Result<AddressTopicFilter> {
    // address: single string or array of strings
    let mut addresses: Vec<Vec<u8>> = Vec::new();
    match filter_obj.get("address") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => addresses.push(parse_address(s).map_err(bad_params)?.to_vec()),
        Some(Value::Array(arr)) => {
            for a in arr {
                let s = a.as_str().ok_or_else(|| {
                    bad_params(anyhow::anyhow!("address entries must be strings"))
                })?;
                addresses.push(parse_address(s).map_err(bad_params)?.to_vec());
            }
        }
        Some(_) => return Err(bad_params(anyhow::anyhow!("invalid 'address' filter"))),
    }

    // topics: up to 4 positions; each null (any) | string | array of strings (OR)
    let mut topics: [Vec<Vec<u8>>; 4] = Default::default();
    if let Some(t) = filter_obj.get("topics")
        && !t.is_null()
    {
        let arr = t
            .as_array()
            .ok_or_else(|| bad_params(anyhow::anyhow!("'topics' must be an array")))?;
        if arr.len() > 4 {
            return Err(bad_params(anyhow::anyhow!("more than 4 topic positions")));
        }
        for (i, entry) in arr.iter().enumerate() {
            match entry {
                Value::Null => {}
                Value::String(s) => topics[i].push(parse_b32(s).map_err(bad_params)?.to_vec()),
                Value::Array(alts) => {
                    for alt in alts {
                        let s = alt.as_str().ok_or_else(|| {
                            bad_params(anyhow::anyhow!("topic entries must be strings"))
                        })?;
                        topics[i].push(parse_b32(s).map_err(bad_params)?.to_vec());
                    }
                }
                _ => return Err(bad_params(anyhow::anyhow!("invalid topic filter"))),
            }
        }
    }
    Ok((addresses, topics))
}

async fn handle_get_logs(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let filter_obj = param(params, 0).map_err(bad_params)?;
    if !filter_obj.is_object() {
        return Err(bad_params(anyhow::anyhow!("filter must be an object")));
    }

    let (addresses, topics) = parse_address_topic_filter(filter_obj)?;

    // blockHash XOR from/to range (EIP-234)
    let block_hash = match filter_obj.get("blockHash") {
        Some(Value::String(s)) => Some(parse_b32(s).map_err(bad_params)?.to_vec()),
        None | Some(Value::Null) => None,
        Some(_) => return Err(bad_params(anyhow::anyhow!("invalid 'blockHash'"))),
    };
    let (from_block, to_block) = if block_hash.is_some() {
        if filter_obj.get("fromBlock").is_some_and(|v| !v.is_null())
            || filter_obj.get("toBlock").is_some_and(|v| !v.is_null())
        {
            return Err(bad_params(anyhow::anyhow!(
                "blockHash is mutually exclusive with fromBlock/toBlock"
            )));
        }
        (0, 0)
    } else {
        let from_v = filter_obj.get("fromBlock").cloned();
        let to_v = filter_obj.get("toBlock").cloned();
        // Only hit the node for head height when a tag actually needs it.
        let needs_head = |v: &Option<Value>| match v {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => !s.starts_with("0x"),
            _ => false,
        };
        let head = if needs_head(&from_v) || needs_head(&to_v) {
            let head_info = state.client.get_head_info().await?;
            head_info
                .get("head_topology")
                .and_then(|t| t.get("height"))
                .and_then(|h| h.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("head_info missing height"))?
        } else {
            0
        };
        let latest = Value::String("latest".into());
        let from =
            resolve_block_tag(from_v.as_ref().unwrap_or(&latest), head).map_err(bad_params)?;
        let to = resolve_block_tag(to_v.as_ref().unwrap_or(&latest), head).map_err(bad_params)?;
        if from > to {
            return Err(bad_params(anyhow::anyhow!("fromBlock > toBlock")));
        }
        // checked: from=0, to=u64::MAX must hit the range cap, not overflow.
        let range = to
            .checked_sub(from)
            .and_then(|d| d.checked_add(1))
            .unwrap_or(u64::MAX);
        if range > state.config.getlogs_max_block_range {
            return Err(anyhow::Error::new(RpcError {
                code: -32005,
                message: format!(
                    "block range too large: {} blocks (max {}); narrow the range",
                    range, state.config.getlogs_max_block_range
                ),
            }));
        }
        (from, to)
    };

    let filter = crate::db::LogFilter {
        from_block,
        to_block,
        block_hash,
        addresses,
        topics,
    };
    let max_results = state.config.getlogs_max_results;
    let db = state.db.clone();
    let rows = tokio::task::spawn_blocking(move || db.query_logs(&filter, max_results + 1))
        .await
        .map_err(|e| anyhow::anyhow!("getLogs join error: {}", e))??;
    if rows.len() > max_results {
        return Err(anyhow::Error::new(RpcError {
            code: -32005,
            message: format!(
                "query returned more than {} results; narrow the filter",
                max_results
            ),
        }));
    }

    let out: Vec<Value> = rows.iter().map(log_row_to_json).collect();
    Ok(Value::Array(out))
}

/// Render one indexed log row as a spec log object (shared by eth_getLogs and
/// the WebSocket logs subscription).
pub(crate) fn log_row_to_json(row: &crate::db::LogQueryRow) -> Value {
    let topics: Vec<String> = row
        .log
        .topics
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();
    json!({
        "address": format!("0x{}", hex::encode(&row.log.address)),
        "topics": topics,
        "data": format!("0x{}", hex::encode(&row.log.data)),
        "blockHash": format!("0x{}", hex::encode(&row.block_hash)),
        "blockNumber": quantity(row.block_height),
        "transactionHash": format!("0x{}", hex::encode(&row.eth_hash)),
        "transactionIndex": format!("0x{:x}", row.tx_index),
        "logIndex": format!("0x{:x}", row.log.log_index),
        "removed": false,
    })
}

async fn fetch_koinos_inclusion(
    state: &Arc<AppState>,
    koinos_tx_id: &[u8],
) -> (Option<Value>, Option<Value>) {
    let id_hex = format!("0x1220{}", hex::encode(koinos_tx_id));
    let resp = match state.client.rest(&format!("/transaction/{}", id_hex)).await {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    let block_hash = resp
        .get("containing_blocks")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .cloned();
    let block_height = match block_hash.as_ref().and_then(|v| v.as_str()) {
        Some(bh) => fetch_block_height(state, bh).await.map(quantity),
        None => None,
    };
    (block_hash, block_height)
}

async fn fetch_block_height(state: &Arc<AppState>, block_id_hex: &str) -> Option<u64> {
    let resp = state
        .client
        .rest(&format!("/block/{}", block_id_hex))
        .await
        .ok()?;
    resp.get("block")
        .and_then(|b| b.get("header"))
        .and_then(|h| h.get("height"))
        .and_then(|h| h.as_str())
        .and_then(|s| s.parse().ok())
}

/// Compute the EVM CREATE address: keccak256(rlp([sender, nonce]))[12..32].
fn create_address(sender: &[u8; 20], nonce: u64) -> [u8; 20] {
    use sha3::{Digest, Keccak256};
    // RLP encode [sender(20), nonce]
    let nonce_bytes = if nonce == 0 {
        vec![0x80]
    } else {
        let be = nonce.to_be_bytes();
        let start = be.iter().position(|&b| b != 0).unwrap_or(8);
        let n_bytes = &be[start..];
        if n_bytes.len() == 1 && n_bytes[0] <= 0x7f {
            vec![n_bytes[0]]
        } else {
            let mut v = vec![0x80 + n_bytes.len() as u8];
            v.extend_from_slice(n_bytes);
            v
        }
    };
    let mut sender_item = vec![0x80 + 20u8];
    sender_item.extend_from_slice(sender);
    let payload_len = sender_item.len() + nonce_bytes.len();
    let mut rlp = Vec::with_capacity(payload_len + 1);
    rlp.push(0xc0 + payload_len as u8);
    rlp.extend_from_slice(&sender_item);
    rlp.extend_from_slice(&nonce_bytes);
    let mut h = Keccak256::new();
    h.update(&rlp);
    let hash: [u8; 32] = h.finalize().into();
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash[12..]);
    addr
}

/// Fetch the operator's next nonce from the chain.
/// Koinos returns the current nonce as a serialized `value_type { uint64_value = 5 }`.
async fn fetch_operator_nonce(state: &Arc<AppState>) -> anyhow::Result<u64> {
    let resp = state
        .client
        .get_account_nonce(&state.config.operator_addr_b58check)
        .await?;
    let nonce_b64 = resp
        .get("nonce")
        .and_then(|v| v.as_str())
        .or_else(|| resp.get("value").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow::anyhow!("get_account_nonce response missing nonce/value"))?;
    let bytes = decode_b64_lax(nonce_b64)?;
    // Decode value_type { uint64_value = 5 } → just look for tag (5<<3|0) = 0x28
    let mut current: u64 = 0;
    for field in ProtoIter::new(&bytes) {
        let f = field?;
        if f.field == 5 && f.wtype == 0 {
            current = f.varint;
        }
    }
    // Next nonce = current + 1
    // (Koinos `get_account_nonce` returns the LAST USED nonce; next-to-use is current+1.
    // For never-used accounts, it returns 0 → first tx uses nonce=1, not 0.)
    Ok(current + 1)
}

// ── Helpers ─────────────────────────────────────────────────────────────

async fn read_account(
    state: &Arc<AppState>,
    addr: &[u8; 20],
) -> anyhow::Result<Option<crate::engine_proto::EvmAccount>> {
    let args = build_get_account_args(addr);
    let bytes = state
        .client
        .read_contract(
            &state.config.engine_contract_addr_b58check,
            EP_GET_ACCOUNT,
            &encode_b64(&args),
        )
        .await?;
    decode_account(&bytes)
}
