//! JSON-RPC method dispatch.

use crate::engine_proto::{
    build_call_view_args, build_get_account_args, build_get_code_args, build_get_storage_at_args,
    decode_account, decode_evm_result, CallViewArgs, ProtoIter,
};
use crate::eth_codec::{
    data, param, param_str, parse_address, parse_b32, parse_data, parse_quantity, parse_u256_be,
    quantity, quantity_from_be32,
};
use crate::koinos::{decode_b64_lax, encode_b64};
use crate::koinos_tx::{build_relayed_tx, encode_nonce};
use crate::state::AppState;
use serde_json::{json, Value};
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
        "eth_gasPrice" => Ok(quantity(0)),
        "eth_maxPriorityFeePerGas" => Ok(quantity(0)),
        "eth_feeHistory" => Ok(handle_fee_history(&params)),

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

        // F6
        "eth_estimateGas" => handle_estimate_gas(state, &params).await,
        "eth_getBlockByNumber" => handle_get_block_by_number(state, &params).await,
        "eth_getBlockByHash" => handle_get_block_by_hash(state, &params).await,

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
            } else {
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

pub fn invalid_request_response(id: Value, msg: &str) -> Value {
    error_response(id, -32600, msg)
}

fn not_implemented(method: &str) -> anyhow::Result<Value> {
    anyhow::bail!("not yet implemented: {}", method)
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

fn handle_fee_history(params: &Value) -> Value {
    // Params: [blockCount, newestBlock, rewardPercentiles?]
    let count = param(params, 0)
        .ok()
        .and_then(|v| v.as_str().and_then(|s| parse_quantity(s).ok()).or(v.as_u64()))
        .unwrap_or(1) as usize;
    let count = count.min(32).max(1);
    let percentiles = param(params, 2)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    // All-zero fee history (Koinos has no Ethereum-style fee market)
    let base_fees = vec![json!("0x0"); count + 1];
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
    let addr_s = param_str(params, 0)?;
    let addr = parse_address(addr_s)?;
    let acct = read_account(state, &addr).await?;
    match acct {
        Some(a) => Ok(quantity_from_be32(&a.balance)),
        None => Ok(json!("0x0")),
    }
}

async fn handle_get_tx_count(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let addr_s = param_str(params, 0)?;
    let addr = parse_address(addr_s)?;
    let on_chain = read_account(state, &addr).await?.map(|a| a.nonce).unwrap_or(0);

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
    match pn.get(&addr).copied() {
        // In-flight tx(s) ahead of the chain → report the tracked next nonce.
        Some(p) if p > on_chain => Ok(quantity(p)),
        // Chain caught up to/past our tracked pending → the tx(s) committed; drop the
        // stale entry and report the on-chain nonce. NOTE: an EVM tx that the engine
        // accepts at the Koinos layer but never advances the on-chain nonce (e.g. a
        // manually-submitted future-nonce tx) would leave a too-high entry that never
        // self-heals here. Acceptable for the sequential-send (MetaMask) flow this fixes;
        // a TTL / receipt-driven reset is a Phase-H hardening item before public exposure.
        Some(_) => { pn.remove(&addr); Ok(quantity(on_chain)) }
        None => Ok(quantity(on_chain)),
    }
}

async fn handle_get_code(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let addr_s = param_str(params, 0)?;
    let addr = parse_address(addr_s)?;
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
    let addr_s = param_str(params, 0)?;
    let slot_s = param_str(params, 1)?;
    let addr = parse_address(addr_s)?;
    let slot = parse_b32(slot_s)?;
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
    let call_obj = param(params, 0)?;
    let from = call_obj.get("from").and_then(|v| v.as_str());
    let to_s = call_obj
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("eth_call: 'to' required"))?;
    let data_s = call_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
    let gas_s = call_obj.get("gas").and_then(|v| v.as_str());
    let value_s = call_obj.get("value").and_then(|v| v.as_str());

    let to = parse_address(to_s)?;
    let calldata = if data_s == "0x" || data_s.is_empty() {
        Vec::new()
    } else {
        parse_data(data_s)?
    };
    let caller = if let Some(f) = from {
        Some(parse_address(f)?)
    } else {
        None
    };
    // gas_limit handled above

    // value: U256, big-endian 32 bytes. Default = 0. Errors are propagated (no silent clamp).
    let value_bytes_buf;
    let value_ref: Option<&[u8; 32]> = if let Some(vs) = value_s {
        value_bytes_buf = parse_u256_be(vs)?;
        Some(&value_bytes_buf)
    } else {
        None
    };

    let gas_limit = match gas_s {
        Some(s) => parse_quantity(s)?,
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
    let raw_hex = param_str(params, 0)?;
    let raw_tx = parse_data(raw_hex)?;
    if raw_tx.is_empty() {
        anyhow::bail!("empty raw_tx");
    }

    // Ethereum tx hash = keccak256(raw_tx). MetaMask uses this to poll for receipts.
    let mut hasher = Keccak256::new();
    hasher.update(&raw_tx);
    let eth_hash: [u8; 32] = hasher.finalize().into();
    let eth_hash_hex = format!("0x{}", hex::encode(eth_hash));

    // Decode the raw tx so we can store metadata for F5 lookups (from/to/nonce/value/input/...).
    let decoded = crate::eth_tx::decode_and_recover(&raw_tx)
        .map_err(|e| anyhow::anyhow!("failed to decode raw tx: {}", e))?;

    // Serialize operator-nonce gate — concurrent F4 calls must not race.
    // Dedupe is atomic: BOTH the dedupe check below AND the tx_meta insert on success
    // happen while this nonce mutex is held (the guard is dropped only after the insert,
    // in the Ok arm). So two concurrent identical raw_txs can't both pass dedupe before
    // one records — the second acquires the lock only after the first's insert is visible.
    let mut nonce_guard = state.operator_nonce.lock().await;
    {
        let map = state.tx_meta.read().unwrap();
        if map.contains_key(&eth_hash) {
            return Ok(json!(eth_hash_hex));
        }
    }
    if nonce_guard.is_none() {
        // Lazily fetch the operator's current nonce from the chain.
        let n = fetch_operator_nonce(state).await?;
        *nonce_guard = Some(n);
    }
    let nonce = nonce_guard.unwrap();

    // Build + sign the relayed Koinos tx. Use the FULL 25-byte base58check-decoded payloads
    // (version + hash160 + checksum) so our hand-encoded header matches what Koinos's JSON
    // deserializer produces (it does NOT strip the version+checksum from ADDRESS/CONTRACT_ID).
    let relayed = build_relayed_tx(
        &state.config.koinos_chain_id_bytes,
        rc_limit_mana(),
        &state.config.operator_addr_full,
        nonce,
        &state.config.engine_contract_addr_full,
        EP_SUBMIT_RAW_TX,
        &raw_tx,
        &state.config.operator_key,
    )?;

    // Build the JSON object form of the transaction for chain.submit_transaction.
    let tx_object = build_submit_tx_json(state, &relayed, nonce, &raw_tx, rc_limit_mana())?;

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

    match state.client.submit_transaction(tx_object, broadcast).await {
        Ok(_resp) => {
            // Successful submission — advance operator nonce. Keep holding nonce_guard
            // through the tx_meta insert + pending-nonce update below so the dedupe check
            // (also under this lock, above) and the record write are atomic: a concurrent
            // identical raw_tx can't pass dedupe before we record this one.
            *nonce_guard = Some(nonce + 1);

            // Record eth_hash → metadata (incl. koinos_tx_id)
            let mut koinos_id = [0u8; 32];
            koinos_id.copy_from_slice(&relayed.tx_id_multihash[2..34]);
            let meta = crate::state::TxMeta {
                koinos_tx_id: koinos_id.to_vec(),
                from: decoded.from,
                to: decoded.to,
                nonce: decoded.nonce,
                value: decoded.value,
                input: decoded.data.clone(),
                gas_limit: decoded.gas_limit,
                raw_tx: raw_tx.clone(),
                tx_type: decoded.tx_type,
                chain_id: decoded.chain_id,
                r: decoded.r,
                s: decoded.s,
                v: decoded.v,
            };
            {
                let mut map = state.tx_meta.write().unwrap();
                map.insert(eth_hash, meta);
            }
            // Advance this sender's pending nonce so a follow-up
            // eth_getTransactionCount(addr, "pending") sees this in-flight tx and the
            // wallet picks nonce+1 (the engine enforces strictly sequential nonces).
            {
                let next = decoded.nonce.saturating_add(1);
                let mut pn = state.pending_nonce.write().unwrap();
                pn.entry(decoded.from)
                    .and_modify(|e| { if next > *e { *e = next; } })
                    .or_insert(next);
            }
            // Dedupe + record are now complete; release the operator-nonce gate.
            drop(nonce_guard);

            Ok(json!(eth_hash_hex))
        }
        Err(e) => {
            // Do NOT advance the operator nonce on submission failure.
            drop(nonce_guard);
            Err(e)
        }
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
    let mut submit_args = Vec::new();
    crate::engine_proto::write_bytes_field(&mut submit_args, 1, raw_eth_tx);

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
    let call_obj = param(params, 0)?;
    let from = call_obj.get("from").and_then(|v| v.as_str());
    let to_s = call_obj.get("to").and_then(|v| v.as_str());
    let data_s = call_obj.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
    let value_s = call_obj.get("value").and_then(|v| v.as_str());

    let caller = if let Some(f) = from { Some(parse_address(f)?) } else { None };
    let to = if let Some(t) = to_s { Some(parse_address(t)?) } else { None };
    let calldata = if data_s == "0x" || data_s.is_empty() { Vec::new() } else { parse_data(data_s)? };
    let value_buf;
    let value_ref: Option<&[u8; 32]> = if let Some(vs) = value_s {
        value_buf = parse_u256_be(vs)?;
        Some(&value_buf)
    } else { None };

    let args = build_call_view_args(&CallViewArgs {
        caller: caller.as_ref(),
        to: to.as_ref(),
        value: value_ref,
        data: &calldata,
        gas_limit: 30_000_000,
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
    // 20% headroom: gas estimation isn't deterministic across state changes
    let estimate = result.gas_used.saturating_mul(120) / 100;
    Ok(quantity(estimate.max(21_000)))
}

async fn handle_get_block_by_number(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    // params: [block_tag, full_tx_objects?]
    let tag = param_str(params, 0)?;
    let height = match tag {
        "latest" | "pending" => {
            let head = state.client.get_head_info().await?;
            head.get("head_topology")
                .and_then(|t| t.get("height"))
                .and_then(|h| h.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| anyhow::anyhow!("head_info missing height"))?
        }
        "earliest" => 1,
        s if s.starts_with("0x") => parse_quantity(s)?,
        _ => return Err(anyhow::anyhow!("unsupported block tag: {}", tag)),
    };
    fetch_block_as_eth_object(state, height).await
}

async fn handle_get_block_by_hash(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let hash_s = param_str(params, 0)?;
    // Accept both Ethereum 32-byte hash (we restore the Koinos multihash prefix) and
    // Koinos multihash strings.
    let koinos_hash = restore_koinos_hash_prefix(hash_s);
    let height = match fetch_block_height(state, &koinos_hash).await {
        Some(h) => h,
        None => return Ok(Value::Null),
    };
    fetch_block_as_eth_object(state, height).await
}

async fn fetch_block_as_eth_object(state: &Arc<AppState>, height: u64) -> anyhow::Result<Value> {
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
    let num_txs = block.get("transactions").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);

    Ok(json!({
        "number": quantity(height),
        "hash": block_id,
        "parentHash": prev,
        "timestamp": quantity(timestamp_sec),
        "gasLimit": quantity(30_000_000),
        "gasUsed": quantity(0),
        "miner": "0x0000000000000000000000000000000000000000",
        "difficulty": "0x0",
        "totalDifficulty": "0x0",
        "size": quantity(num_txs as u64 * 200),
        "nonce": "0x0000000000000000",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
        "uncles": [],
        "transactions": [],
        "stateRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "transactionsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "extraData": "0x",
        "baseFeePerGas": "0x0",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    }))
}

async fn handle_get_tx_by_hash(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let hash_s = param_str(params, 0)?;
    let hash = parse_b32(hash_s)?;
    let meta = {
        let map = state.tx_meta.read().unwrap();
        map.get(&hash).cloned()
    };
    let meta = match meta {
        Some(m) => m,
        // Unknown tx: per Ethereum convention return null (not error)
        None => return Ok(Value::Null),
    };

    // Best-effort: include block info if the Koinos tx has been included.
    let (block_hash, block_number) = fetch_koinos_inclusion(state, &meta.koinos_tx_id).await;
    let has_block = block_hash.as_ref().is_some_and(|v| !v.is_null());

    let block_hash_eth = block_hash.as_ref().map(strip_koinos_hash_prefix).unwrap_or(Value::Null);

    Ok(json!({
        "hash": hash_s,
        "from": format!("0x{}", hex::encode(&meta.from)),
        "to": meta.to.map(|a| Value::String(format!("0x{}", hex::encode(&a)))).unwrap_or(Value::Null),
        "nonce": quantity(meta.nonce),
        "value": quantity_from_be32(&meta.value),
        "input": data(&meta.input),
        "gas": quantity(meta.gas_limit),
        "gasPrice": quantity(0),
        "type": format!("0x{:x}", meta.tx_type),
        "chainId": meta.chain_id.map(quantity).unwrap_or(json!(null)),
        "blockHash": block_hash_eth,
        "blockNumber": block_number.unwrap_or(Value::Null),
        "transactionIndex": if has_block { json!("0x0") } else { Value::Null },
        "r": data(&meta.r),
        "s": data(&meta.s),
        "v": quantity(meta.v),
    }))
}

async fn handle_get_tx_receipt(state: &Arc<AppState>, params: &Value) -> anyhow::Result<Value> {
    let hash_s = param_str(params, 0)?;
    let hash = parse_b32(hash_s)?;
    let meta = {
        let map = state.tx_meta.read().unwrap();
        map.get(&hash).cloned()
    };
    let meta = match meta {
        Some(m) => m,
        None => return Ok(Value::Null),
    };

    // Fetch Koinos receipt by tx_id via REST API
    let koinos_tx_id_hex = format!("0x1220{}", hex::encode(&meta.koinos_tx_id));
    let koinos_resp = state
        .client
        .rest(&format!("/transaction/{}", koinos_tx_id_hex))
        .await;
    let receipt_json = match koinos_resp {
        Ok(j) => j,
        // Tx not yet indexed → return null (MetaMask keeps polling)
        Err(_) => return Ok(Value::Null),
    };
    let receipt = receipt_json.get("receipt");
    let containing_blocks = receipt_json.get("containing_blocks").and_then(|v| v.as_array());
    if receipt.is_none() || containing_blocks.map_or(true, |a| a.is_empty()) {
        // Not yet included
        return Ok(Value::Null);
    }
    let receipt = receipt.unwrap();
    let block_hash_koinos = containing_blocks.and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
    // Fetch the containing block to get its height (uses the FULL Koinos multihash for REST)
    let block_number = match block_hash_koinos.as_str() {
        Some(bh) => fetch_block_height(state, bh).await.map(quantity).unwrap_or(Value::Null),
        None => Value::Null,
    };
    // Strip the Koinos multihash prefix when returning at the Ethereum boundary
    let block_hash = strip_koinos_hash_prefix(&block_hash_koinos);

    // Status + gas_used + contract_address come from the engine's `evm.result` event,
    // NOT from Koinos's compute_bandwidth_used (which is Koinos VM compute cost, not EVM gas).
    let mut status = "0x1";
    let mut gas_used: u64 = 0;
    let mut contract_address_from_event: Option<String> = None;
    if let Some(events) = receipt.get("events").and_then(|v| v.as_array()) {
        for ev in events {
            if ev.get("name").and_then(|v| v.as_str()) != Some("evm.result") {
                continue;
            }
            let data_b64 = ev.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = match decode_b64_lax(data_b64) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Decode EvmResultEvent { bool success=1, uint64 gas_used=2, bytes contract_address=3 }
            for f in ProtoIter::new(&bytes) {
                let f = match f { Ok(v) => v, Err(_) => continue };
                match (f.field, f.wtype) {
                    (1, 0) => { if f.varint == 0 { status = "0x0"; } }
                    (2, 0) => gas_used = f.varint,
                    (3, 2) => {
                        if f.payload.len() == 20 {
                            contract_address_from_event = Some(format!("0x{}", hex::encode(f.payload)));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Extract logs from evm.log events
    let mut logs = Vec::new();
    if let Some(events) = receipt.get("events").and_then(|v| v.as_array()) {
        for (log_index, ev) in events.iter().enumerate() {
            if ev.get("name").and_then(|v| v.as_str()) != Some("evm.log") {
                continue;
            }
            let data_b64 = ev.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = match decode_b64_lax(data_b64) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Decode our evm.log proto: { bytes address=1, repeated bytes topics=2, bytes data=3 }
            let mut addr: Option<[u8; 20]> = None;
            let mut topics: Vec<String> = Vec::new();
            let mut log_data: Vec<u8> = Vec::new();
            for f in ProtoIter::new(&bytes) {
                let f = match f { Ok(v) => v, Err(_) => continue };
                match (f.field, f.wtype) {
                    (1, 2) => {
                        if f.payload.len() == 20 {
                            let mut a = [0u8; 20];
                            a.copy_from_slice(f.payload);
                            addr = Some(a);
                        }
                    }
                    (2, 2) => topics.push(format!("0x{}", hex::encode(f.payload))),
                    (3, 2) => log_data = f.payload.to_vec(),
                    _ => {}
                }
            }
            let address = addr.map(|a| format!("0x{}", hex::encode(&a))).unwrap_or_default();
            logs.push(json!({
                "address": address,
                "topics": topics,
                "data": format!("0x{}", hex::encode(&log_data)),
                "blockHash": block_hash.clone(), // already stripped above
                "blockNumber": block_number.clone(),
                "transactionHash": hash_s,
                "transactionIndex": "0x0",
                "logIndex": format!("0x{:x}", log_index),
                "removed": false,
            }));
        }
    }

    // contractAddress: prefer the event-emitted address (handles CREATE2 correctly).
    // Fall back to keccak256(rlp([sender, nonce]))[12..] for plain CREATE.
    let contract_address = match contract_address_from_event {
        Some(s) => Value::String(s),
        None if meta.to.is_none() => {
            let addr = create_address(&meta.from, meta.nonce);
            Value::String(format!("0x{}", hex::encode(&addr)))
        }
        _ => Value::Null,
    };

    Ok(json!({
        "transactionHash": hash_s,
        "transactionIndex": "0x0",
        "blockHash": block_hash,
        "blockNumber": block_number,
        "from": format!("0x{}", hex::encode(&meta.from)),
        "to": meta.to.map(|a| Value::String(format!("0x{}", hex::encode(&a)))).unwrap_or(Value::Null),
        "cumulativeGasUsed": quantity(gas_used),
        "gasUsed": quantity(gas_used),
        "contractAddress": contract_address,
        "logs": logs,
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "status": status,
        "type": format!("0x{:x}", meta.tx_type),
        "effectiveGasPrice": "0x0",
    }))
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
