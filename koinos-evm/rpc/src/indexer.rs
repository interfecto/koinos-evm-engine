//! Account-history backfill indexer (ROADMAP §2, the restart-safe heart).
//!
//! The engine contract's `account_history` IS the EVM tx feed: every relayed tx
//! is a `call_contract(engine, EP_SUBMIT_RAW_TX, raw_tx)` op whose receipt
//! (inline in the history entry) carries the `evm.result`/`evm.log` events.
//! This task pages that history from a persisted cursor, decodes each entry
//! with the same code paths the live relay uses, resolves the containing block
//! (REST, cached in the blocks table), and writes authoritative rows: real
//! block-global `logIndex`, real `transactionIndex`, per-block tx/log counters.
//!
//! Self-populating and fully restart-safe: a fresh proxy with an empty DB
//! backfills the ENTIRE engine history (txs relayed by anyone, ever), then
//! tails the head.
//!
//! Finality: the cursor only advances past entries whose containing block is at
//! or below the last irreversible block (LIB). Entries above LIB are indexed
//! provisionally and re-processed every cycle (idempotent: `Db::index_tx` skips
//! already-indexed txs and re-indexes on a block change). Limitation (PoC): a
//! tx dropped entirely in a reorg lingers until/unless it is re-included, and a
//! reorged block's counters are not rewound.

use crate::eth_codec::parse_data;
use crate::rpc::decode_receipt_events;
use crate::state::AppState;
use serde_json::{Value, json};
use sha3::{Digest, Keccak256};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

const CURSOR_KEY: &str = "indexer_cursor";
/// Engine entry point for submit_raw_tx (must match rpc.rs EP_SUBMIT_RAW_TX).
const EP_SUBMIT_RAW_TX: u64 = 0x0000_0007;

/// Spawned from main: backfill once, then tail.
pub async fn run_indexer(state: Arc<AppState>, poll_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs.max(1)));
    // In-memory block cache for the current process (height+timestamp by Koinos
    // block id); the blocks table provides the cross-restart cache.
    let mut block_cache: HashMap<String, (u64, u64)> = HashMap::new();
    loop {
        interval.tick().await;
        if let Err(e) = run_cycle(&state, &mut block_cache).await {
            warn!(error = %e, "indexer: cycle failed; retrying next tick");
        }
    }
}

async fn run_cycle(
    state: &Arc<AppState>,
    block_cache: &mut HashMap<String, (u64, u64)>,
) -> anyhow::Result<()> {
    // LIB for cursor finality.
    let head = state.client.get_head_info().await?;
    let lib: u64 = head
        .get("last_irreversible_block")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("head_info missing last_irreversible_block"))?;

    let db = state.db.clone();
    let cursor: Option<u64> = tokio::task::spawn_blocking(move || db.get_meta(CURSOR_KEY))
        .await??
        .and_then(|s| s.parse().ok());
    let mut next_seq: u64 = cursor.map(|c| c + 1).unwrap_or(0);
    let mut cursor_candidate = cursor;
    let mut cursor_frozen = false;
    let page_size = state.config.indexer_page_size;
    let mut indexed = 0usize;

    loop {
        let mut params = json!({
            "address": state.config.engine_contract_addr_b58check,
            "limit": page_size,
            "ascending": true,
        });
        // seq_num is an INCLUSIVE start (probed live); omit for the very first run.
        if next_seq > 0 {
            params["seq_num"] = json!(next_seq.to_string());
        }
        let resp = state
            .client
            .rpc("account_history.get_account_history", params)
            .await?;
        let values = resp
            .get("values")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if values.is_empty() {
            break;
        }
        let page_len = values.len();

        for entry in &values {
            // Missing seq_num means 0 (proto3 default-skip in the JSON encoding).
            let seq: u64 = entry
                .get("seq_num")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            next_seq = seq + 1;

            match process_entry(state, entry, block_cache).await {
                Ok(EntryOutcome::Indexed { block_height }) => {
                    indexed += 1;
                    if !cursor_frozen && block_height <= lib {
                        cursor_candidate = Some(seq);
                    } else {
                        cursor_frozen = true;
                    }
                }
                Ok(EntryOutcome::AlreadyIndexed { block_height }) => {
                    if !cursor_frozen && block_height <= lib {
                        cursor_candidate = Some(seq);
                    } else {
                        cursor_frozen = true;
                    }
                }
                // Entries that aren't relayed EVM txs (funding transfers, engine
                // uploads, ...) — nothing to index; safe to advance past.
                Ok(EntryOutcome::NotEvm) => {
                    if !cursor_frozen {
                        cursor_candidate = Some(seq);
                    }
                }
                Err(e) => {
                    // Freeze the cursor so this entry is retried next cycle, but
                    // keep processing the rest of the page (later entries are
                    // independent; idempotency makes their re-pass free).
                    warn!(seq = seq, error = %e, "indexer: entry failed; will retry");
                    cursor_frozen = true;
                }
            }
        }

        if page_len < page_size {
            break; // caught up to head
        }
    }

    if cursor_candidate != cursor
        && let Some(c) = cursor_candidate
    {
        let db = state.db.clone();
        tokio::task::spawn_blocking(move || db.set_meta(CURSOR_KEY, &c.to_string())).await??;
    }
    if indexed > 0 {
        info!(
            indexed = indexed,
            cursor = ?cursor_candidate,
            lib = lib,
            "indexer: cycle complete"
        );
    } else {
        debug!(cursor = ?cursor_candidate, "indexer: nothing new");
    }
    Ok(())
}

enum EntryOutcome {
    Indexed { block_height: u64 },
    AlreadyIndexed { block_height: u64 },
    NotEvm,
}

async fn process_entry(
    state: &Arc<AppState>,
    entry: &Value,
    block_cache: &mut HashMap<String, (u64, u64)>,
) -> anyhow::Result<EntryOutcome> {
    let Some(tx) = entry.get("trx").and_then(|t| t.get("transaction")) else {
        return Ok(EntryOutcome::NotEvm);
    };
    // Find the engine submit_raw_tx op (don't assume operations[0]; require the
    // entry point and, when present, the engine contract id).
    let raw_tx = tx
        .get("operations")
        .and_then(|o| o.as_array())
        .into_iter()
        .flatten()
        .filter_map(|op| op.get("call_contract"))
        .find(|cc| {
            cc.get("entry_point").and_then(|e| e.as_u64()) == Some(EP_SUBMIT_RAW_TX)
                && cc
                    .get("contract_id")
                    .and_then(|c| c.as_str())
                    .is_none_or(|c| c == state.config.engine_contract_addr_b58check)
        })
        .and_then(|cc| cc.get("args").and_then(|a| a.as_str()))
        .and_then(|args_b64| crate::koinos::decode_b64_lax(args_b64).ok())
        .and_then(|args| {
            // SubmitRawTxArgs { bytes raw_tx = 1 }
            crate::engine_proto::ProtoIter::new(&args)
                .flatten()
                .find(|f| f.field == 1 && f.wtype == 2)
                .map(|f| f.payload.to_vec())
        });
    let Some(raw_tx) = raw_tx else {
        return Ok(EntryOutcome::NotEvm);
    };

    // Decode with the same trust-boundary-free decoder the relay uses. An
    // undecodable raw_tx (someone fed the engine garbage directly) is skipped.
    let decoded = match crate::eth_tx::decode_and_recover(&raw_tx) {
        Ok(d) => d,
        Err(e) => {
            debug!(error = %e, "indexer: undecodable raw_tx in history; skipping");
            return Ok(EntryOutcome::NotEvm);
        }
    };
    let mut hasher = Keccak256::new();
    hasher.update(&raw_tx);
    let eth_hash: [u8; 32] = hasher.finalize().into();

    // Koinos tx id ("0x1220<32B>") → 32-byte id.
    let koinos_tx_id = tx
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.strip_prefix("0x1220")
                .unwrap_or(s.trim_start_matches("0x"))
        })
        .and_then(|h| parse_data(&format!("0x{}", h)).ok())
        .unwrap_or_default();

    // Inline receipt events → status/gas/logs. History entries are included
    // txs, so a missing receipt is unexpected — error to retry next cycle.
    let receipt_events = entry
        .get("trx")
        .and_then(|t| t.get("receipt"))
        .ok_or_else(|| anyhow::anyhow!("history entry missing receipt"))?
        .get("events")
        .and_then(|v| v.as_array())
        .cloned();
    let ev = decode_receipt_events(receipt_events.as_ref());

    // Resolve containing block: process cache → blocks table → REST.
    let koinos_id_hex = format!("0x1220{}", hex::encode(&koinos_tx_id));
    let (block_height, block_hash_koinos, timestamp) =
        resolve_block(state, &koinos_id_hex, block_cache).await?;
    let block_hash = parse_data(&format!(
        "0x{}",
        block_hash_koinos
            .strip_prefix("0x1220")
            .unwrap_or(block_hash_koinos.trim_start_matches("0x"))
    ))
    .unwrap_or_default();

    let meta = crate::state::TxMeta {
        koinos_tx_id,
        from: decoded.from,
        to: decoded.to,
        nonce: decoded.nonce,
        value: decoded.value,
        input: decoded.data.clone(),
        gas_limit: decoded.gas_limit,
        gas_price: decoded.gas_price,
        raw_tx,
        tx_type: decoded.tx_type,
        chain_id: decoded.chain_id,
        r: decoded.r,
        s: decoded.s,
        v: decoded.v,
    };
    let receipt = crate::db::ReceiptRow {
        block_height,
        block_hash,
        tx_index: None, // assigned inside index_tx from the block counters
        status: ev.status,
        gas_used: ev.gas_used,
        contract_address: ev.contract_address,
        logs: ev.logs,
    };

    let db = state.db.clone();
    let wrote =
        tokio::task::spawn_blocking(move || db.index_tx(&eth_hash, &meta, &receipt, timestamp))
            .await??;
    Ok(if wrote {
        EntryOutcome::Indexed { block_height }
    } else {
        EntryOutcome::AlreadyIndexed { block_height }
    })
}

/// Containing block (height, koinos block id, unix-secs timestamp) for a tx.
async fn resolve_block(
    state: &Arc<AppState>,
    koinos_tx_id_hex: &str,
    block_cache: &mut HashMap<String, (u64, u64)>,
) -> anyhow::Result<(u64, String, u64)> {
    let tx_info = state
        .client
        .rest(&format!("/transaction/{}", koinos_tx_id_hex))
        .await?;
    let block_id = tx_info
        .get("containing_blocks")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("transaction has no containing block yet"))?
        .to_string();

    if let Some((h, ts)) = block_cache.get(&block_id) {
        return Ok((*h, block_id, *ts));
    }
    // Cross-restart cache: the blocks table.
    {
        let db = state.db.clone();
        let stripped = parse_data(&format!(
            "0x{}",
            block_id
                .strip_prefix("0x1220")
                .unwrap_or(block_id.trim_start_matches("0x"))
        ))
        .unwrap_or_default();
        if let Ok(Some(row)) =
            tokio::task::spawn_blocking(move || db.get_block_by_hash(&stripped)).await?
        {
            block_cache.insert(block_id.clone(), (row.height, row.timestamp));
            return Ok((row.height, block_id, row.timestamp));
        }
    }

    let block = state.client.rest(&format!("/block/{}", block_id)).await?;
    let header = block
        .get("block")
        .and_then(|b| b.get("header"))
        .ok_or_else(|| anyhow::anyhow!("block lookup missing header"))?;
    let height: u64 = header
        .get("height")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("block header missing height"))?;
    let timestamp_ms: u64 = header
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ts = timestamp_ms / 1000;
    block_cache.insert(block_id.clone(), (height, ts));
    Ok((height, block_id, ts))
}
