//! Durable SQLite store for txs, receipts, logs, and blocks (ROADMAP §2).
//!
//! Two writer tiers: the relay/receipt paths write rows PROVISIONALLY (receipt
//! known, tx_index unknown), and the account-history indexer (indexer.rs) is
//! AUTHORITATIVE — it backfills the engine's entire history and (re)writes rows
//! with block-global log indexes, real transaction indexes, and per-block
//! counters. Known limitation: a tx dropped entirely in a reorg lingers until
//! re-included, and a reorged block's counters are not rewound.
//!
//! Concurrency model: one bundled-SQLite connection behind a std Mutex. Queries
//! are short (point lookups + indexed range scans); callers on the async path
//! wrap calls in `tokio::task::spawn_blocking`.

use crate::state::TxMeta;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

/// Receipt fields + logs for one included tx (block_hash is the 32-byte
/// eth-style hash, i.e. the Koinos block id with the 0x1220 multihash prefix
/// stripped).
#[derive(Clone)]
pub struct ReceiptRow {
    pub block_height: u64,
    pub block_hash: Vec<u8>,
    /// Position among the block's EVM txs. None = provisionally settled by the
    /// receipt path/poller (which can't know it); the indexer writes the real
    /// value and is authoritative.
    pub tx_index: Option<u64>,
    pub status: bool,
    pub gas_used: u64,
    pub contract_address: Option<Vec<u8>>,
    pub logs: Vec<LogRow>,
}

/// One row of the blocks index. The counters are maintained inside `index_tx`
/// via SQL; on this struct they're read by tests (and kept as API surface).
#[derive(Clone)]
pub struct BlockRow {
    pub height: u64,
    #[allow(dead_code)]
    pub block_hash: Vec<u8>,
    pub timestamp: u64,
    #[allow(dead_code)]
    pub tx_count: u64,
    #[allow(dead_code)]
    pub log_count: u64,
}

#[derive(Clone)]
pub struct LogRow {
    pub log_index: u64,
    pub address: Vec<u8>,
    pub topics: Vec<Vec<u8>>,
    pub data: Vec<u8>,
}

/// One eth_getLogs result row (log + its tx/block context).
pub struct LogQueryRow {
    pub eth_hash: Vec<u8>,
    pub block_height: u64,
    pub block_hash: Vec<u8>,
    pub tx_index: u64,
    pub log: LogRow,
}

/// Parsed eth_getLogs filter. Empty address/topic lists mean "any".
/// `topics[i]` is the OR-list for topic position i.
#[derive(Default)]
pub struct LogFilter {
    pub from_block: u64,
    pub to_block: u64,
    pub block_hash: Option<Vec<u8>>,
    pub addresses: Vec<Vec<u8>>,
    pub topics: [Vec<Vec<u8>>; 4],
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS txs (
    eth_hash     BLOB PRIMARY KEY,
    koinos_tx_id BLOB,
    from_addr    BLOB NOT NULL,
    to_addr      BLOB,
    nonce        INTEGER NOT NULL,
    value        BLOB NOT NULL,
    input        BLOB NOT NULL,
    gas_limit    INTEGER NOT NULL,
    gas_price    BLOB NOT NULL,
    raw_tx       BLOB NOT NULL,
    tx_type      INTEGER NOT NULL,
    chain_id     INTEGER,
    sig_r        BLOB NOT NULL,
    sig_s        BLOB NOT NULL,
    sig_v        INTEGER NOT NULL,
    block_height INTEGER,
    block_hash   BLOB,
    tx_index     INTEGER,
    status       INTEGER,
    gas_used     INTEGER,
    contract_address BLOB,
    poll_attempts INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_txs_block ON txs(block_height);
CREATE INDEX IF NOT EXISTS idx_txs_pending ON txs(status) WHERE status IS NULL;

CREATE TABLE IF NOT EXISTS logs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    eth_hash     BLOB NOT NULL,
    block_height INTEGER NOT NULL,
    block_hash   BLOB NOT NULL,
    tx_index     INTEGER NOT NULL DEFAULT 0,
    log_index    INTEGER NOT NULL,
    address      BLOB NOT NULL,
    topic0       BLOB,
    topic1       BLOB,
    topic2       BLOB,
    topic3       BLOB,
    data         BLOB NOT NULL,
    UNIQUE(eth_hash, log_index)
);
CREATE INDEX IF NOT EXISTS idx_logs_addr   ON logs(address, block_height);
CREATE INDEX IF NOT EXISTS idx_logs_topic0 ON logs(topic0, block_height);
CREATE INDEX IF NOT EXISTS idx_logs_height ON logs(block_height);

CREATE TABLE IF NOT EXISTS blocks (
    height     INTEGER PRIMARY KEY,
    block_hash BLOB NOT NULL,
    timestamp  INTEGER NOT NULL,
    tx_count   INTEGER NOT NULL DEFAULT 0,
    log_count  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_blocks_hash ON blocks(block_hash);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Give up background-polling a pending tx after this many failed attempts
/// (~10 minutes at the default 3 s poll interval — far beyond Koinos inclusion
/// time, so the row is a mempool-dropped zombie). Client-driven
/// eth_getTransactionReceipt lookups still settle such a tx if it ever lands;
/// the cap only stops zombies from starving the poller's LIMIT window.
const POLL_ATTEMPTS_CAP: i64 = 200;

fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).context("applying db schema")?;
    // Migration for stores created before poll_attempts existed; the error on
    // a duplicate column is expected and ignored.
    let _ = conn.execute(
        "ALTER TABLE txs ADD COLUMN poll_attempts INTEGER NOT NULL DEFAULT 0",
        [],
    );
    Ok(())
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("opening sqlite db at {}", path))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        apply_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        apply_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert (or refresh the koinos_tx_id of) a relayed tx. Receipt fields are
    /// never touched here, so a re-relay of an evicted tx can't wipe its receipt.
    pub fn upsert_tx(&self, eth_hash: &[u8; 32], meta: &TxMeta) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO txs (eth_hash, koinos_tx_id, from_addr, to_addr, nonce, value,
                              input, gas_limit, gas_price, raw_tx, tx_type, chain_id,
                              sig_r, sig_s, sig_v)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(eth_hash) DO UPDATE SET koinos_tx_id = excluded.koinos_tx_id",
            params![
                eth_hash.as_slice(),
                (!meta.koinos_tx_id.is_empty()).then_some(meta.koinos_tx_id.as_slice()),
                meta.from.as_slice(),
                meta.to.as_ref().map(|a| a.as_slice()),
                meta.nonce as i64,
                meta.value.as_slice(),
                meta.input.as_slice(),
                meta.gas_limit as i64,
                meta.gas_price.as_slice(),
                meta.raw_tx.as_slice(),
                meta.tx_type as i64,
                meta.chain_id.map(|c| c as i64),
                meta.r.as_slice(),
                meta.s.as_slice(),
                meta.v as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_tx_meta(&self, eth_hash: &[u8; 32]) -> Result<Option<TxMeta>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT koinos_tx_id, from_addr, to_addr, nonce, value, input, gas_limit,
                    gas_price, raw_tx, tx_type, chain_id, sig_r, sig_s, sig_v
             FROM txs WHERE eth_hash = ?1",
            params![eth_hash.as_slice()],
            |row| {
                Ok(TxMeta {
                    koinos_tx_id: row.get::<_, Option<Vec<u8>>>(0)?.unwrap_or_default(),
                    from: blob20(row.get(1)?),
                    to: row.get::<_, Option<Vec<u8>>>(2)?.map(blob20),
                    nonce: row.get::<_, i64>(3)? as u64,
                    value: blob32(row.get(4)?),
                    input: row.get(5)?,
                    gas_limit: row.get::<_, i64>(6)? as u64,
                    gas_price: blob32(row.get(7)?),
                    raw_tx: row.get(8)?,
                    tx_type: row.get::<_, i64>(9)? as u8,
                    chain_id: row.get::<_, Option<i64>>(10)?.map(|c| c as u64),
                    r: blob32(row.get(11)?),
                    s: blob32(row.get(12)?),
                    v: row.get::<_, i64>(13)? as u64,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Persist receipt fields + logs for an included tx (idempotent), as a
    /// PROVISIONAL settle: tx_index is left NULL and log indexes are per-tx —
    /// the indexer later overwrites with authoritative block-global values.
    /// Errors if the parent txs row is missing — inserting logs without their tx
    /// would leave an internally inconsistent store (logs served by eth_getLogs
    /// whose receipt can never be retrieved). Callers persist the tx row first.
    pub fn set_receipt(&self, eth_hash: &[u8; 32], r: &ReceiptRow) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE txs SET block_height = ?2, block_hash = ?3, tx_index = NULL,
                            status = ?4, gas_used = ?5, contract_address = ?6
             WHERE eth_hash = ?1",
            params![
                eth_hash.as_slice(),
                r.block_height as i64,
                r.block_hash.as_slice(),
                r.status as i64,
                r.gas_used as i64,
                r.contract_address.as_deref(),
            ],
        )?;
        if updated == 0 {
            anyhow::bail!("set_receipt: no txs row for this hash (persist the tx first)");
        }
        Self::replace_logs(&tx, eth_hash, r, 0, 0)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace a tx's log rows. `tx_index` and `log_index_base` come from the
    /// indexer (authoritative); the provisional settle path passes 0/0.
    fn replace_logs(
        tx: &rusqlite::Transaction<'_>,
        eth_hash: &[u8; 32],
        r: &ReceiptRow,
        tx_index: u64,
        log_index_base: u64,
    ) -> Result<()> {
        tx.execute(
            "DELETE FROM logs WHERE eth_hash = ?1",
            params![eth_hash.as_slice()],
        )?;
        for (i, log) in r.logs.iter().enumerate() {
            let topic = |i: usize| log.topics.get(i).map(|t| t.as_slice());
            tx.execute(
                "INSERT INTO logs (eth_hash, block_height, block_hash, tx_index, log_index,
                                   address, topic0, topic1, topic2, topic3, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    eth_hash.as_slice(),
                    r.block_height as i64,
                    r.block_hash.as_slice(),
                    tx_index as i64,
                    (log_index_base + i as u64) as i64,
                    log.address.as_slice(),
                    topic(0),
                    topic(1),
                    topic(2),
                    topic(3),
                    log.data.as_slice(),
                ],
            )?;
        }
        Ok(())
    }

    /// Authoritative indexer write: upsert the tx row with its receipt fields and
    /// real block-global tx_index/log_index, maintain the blocks-table counters,
    /// all in one transaction. Returns false when this tx is already indexed for
    /// the same block (idempotent re-pass over the reversible tail).
    pub fn index_tx(
        &self,
        eth_hash: &[u8; 32],
        meta: &TxMeta,
        r: &ReceiptRow,
        block_timestamp: u64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Already indexed for this block? (tx_index NOT NULL marks an indexer
        // write; a provisional settle leaves it NULL and gets overwritten here.)
        let existing: Option<(Option<i64>, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT tx_index, block_height, status FROM txs WHERE eth_hash = ?1",
                params![eth_hash.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((Some(_), Some(h), st)) = existing {
            if h == r.block_height as i64 {
                return Ok(false);
            }
            // Same eth tx indexed from a DIFFERENT Koinos tx/block. The engine's
            // EVM nonce check means a raw tx can only execute successfully once —
            // any later duplicate submission is a rejected re-relay (status 0,
            // gas 0). Never let such a duplicate overwrite the real execution.
            if st == Some(1) && !r.status {
                return Ok(false);
            }
            // Otherwise (reorg, or success replacing a failed placement): fall
            // through and reindex. The stale block's counters are not rewound —
            // acceptable for the PoC tail.
        }

        tx.execute(
            "INSERT INTO blocks (height, block_hash, timestamp) VALUES (?1, ?2, ?3)
             ON CONFLICT(height) DO NOTHING",
            params![
                r.block_height as i64,
                r.block_hash.as_slice(),
                block_timestamp as i64
            ],
        )?;
        let (tx_count, log_count): (i64, i64) = tx.query_row(
            "SELECT tx_count, log_count FROM blocks WHERE height = ?1",
            params![r.block_height as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let tx_index = tx_count as u64;
        let log_base = log_count as u64;

        tx.execute(
            "INSERT INTO txs (eth_hash, koinos_tx_id, from_addr, to_addr, nonce, value,
                              input, gas_limit, gas_price, raw_tx, tx_type, chain_id,
                              sig_r, sig_s, sig_v,
                              block_height, block_hash, tx_index, status, gas_used,
                              contract_address)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21)
             ON CONFLICT(eth_hash) DO UPDATE SET
                 koinos_tx_id = excluded.koinos_tx_id,
                 block_height = excluded.block_height,
                 block_hash = excluded.block_hash,
                 tx_index = excluded.tx_index,
                 status = excluded.status,
                 gas_used = excluded.gas_used,
                 contract_address = excluded.contract_address",
            params![
                eth_hash.as_slice(),
                (!meta.koinos_tx_id.is_empty()).then_some(meta.koinos_tx_id.as_slice()),
                meta.from.as_slice(),
                meta.to.as_ref().map(|a| a.as_slice()),
                meta.nonce as i64,
                meta.value.as_slice(),
                meta.input.as_slice(),
                meta.gas_limit as i64,
                meta.gas_price.as_slice(),
                meta.raw_tx.as_slice(),
                meta.tx_type as i64,
                meta.chain_id.map(|c| c as i64),
                meta.r.as_slice(),
                meta.s.as_slice(),
                meta.v as i64,
                r.block_height as i64,
                r.block_hash.as_slice(),
                tx_index as i64,
                r.status as i64,
                r.gas_used as i64,
                r.contract_address.as_deref(),
            ],
        )?;
        Self::replace_logs(&tx, eth_hash, r, tx_index, log_base)?;
        tx.execute(
            "UPDATE blocks SET tx_count = tx_count + 1, log_count = log_count + ?2
             WHERE height = ?1",
            params![r.block_height as i64, r.logs.len() as i64],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Highest block height with indexed logs (0 when empty) — the WS logs
    /// tail starts here so restarts don't replay history at subscribers.
    pub fn max_indexed_height(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let h: i64 = conn.query_row(
            "SELECT COALESCE(MAX(block_height), 0) FROM logs",
            [],
            |row| row.get(0),
        )?;
        Ok(h.max(0) as u64)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn get_block_by_height(&self, height: u64) -> Result<Option<BlockRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT height, block_hash, timestamp, tx_count, log_count
             FROM blocks WHERE height = ?1",
            params![height as i64],
            row_to_block,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_block_by_hash(&self, hash: &[u8]) -> Result<Option<BlockRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT height, block_hash, timestamp, tx_count, log_count
             FROM blocks WHERE block_hash = ?1",
            params![hash],
            row_to_block,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Eth hashes of a block's EVM txs, ordered by tx_index (provisionally
    /// settled rows — tx_index NULL — sort last).
    pub fn block_tx_hashes(&self, height: u64) -> Result<Vec<[u8; 32]>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT eth_hash FROM txs WHERE block_height = ?1
             ORDER BY tx_index IS NULL, tx_index, rowid",
        )?;
        let rows = stmt
            .query_map(params![height as i64], |row| Ok(blob32(row.get(0)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Sum of indexed gas_used across a block's EVM txs.
    pub fn block_gas_used(&self, height: u64) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COALESCE(SUM(gas_used), 0) FROM txs WHERE block_height = ?1",
            params![height as i64],
            |row| row.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// All logs in a block (for the block-level bloom), ordered by log_index.
    pub fn block_logs(&self, height: u64) -> Result<Vec<LogRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT log_index, address, topic0, topic1, topic2, topic3, data
             FROM logs WHERE block_height = ?1 ORDER BY log_index",
        )?;
        let rows = stmt
            .query_map(params![height as i64], |row| {
                let mut topics = Vec::new();
                for i in 2..6 {
                    if let Some(t) = row.get::<_, Option<Vec<u8>>>(i)? {
                        topics.push(t);
                    }
                }
                Ok(LogRow {
                    log_index: row.get::<_, i64>(0)? as u64,
                    address: row.get(1)?,
                    topics,
                    data: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Receipt + logs for a tx, if its receipt has been persisted.
    pub fn get_receipt(&self, eth_hash: &[u8; 32]) -> Result<Option<ReceiptRow>> {
        let conn = self.conn.lock().unwrap();
        let head = conn
            .query_row(
                "SELECT block_height, block_hash, status, gas_used, contract_address, tx_index
                 FROM txs WHERE eth_hash = ?1 AND status IS NOT NULL",
                params![eth_hash.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, i64>(3)? as u64,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, Option<i64>>(5)?.map(|t| t as u64),
                    ))
                },
            )
            .optional()?;
        let Some((block_height, block_hash, status, gas_used, contract_address, tx_index)) = head
        else {
            return Ok(None);
        };
        let mut stmt = conn.prepare(
            "SELECT log_index, address, topic0, topic1, topic2, topic3, data
             FROM logs WHERE eth_hash = ?1 ORDER BY log_index",
        )?;
        let logs = stmt
            .query_map(params![eth_hash.as_slice()], |row| {
                let mut topics = Vec::new();
                for i in 2..6 {
                    if let Some(t) = row.get::<_, Option<Vec<u8>>>(i)? {
                        topics.push(t);
                    }
                }
                Ok(LogRow {
                    log_index: row.get::<_, i64>(0)? as u64,
                    address: row.get(1)?,
                    topics,
                    data: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(ReceiptRow {
            block_height,
            block_hash,
            tx_index,
            status,
            gas_used,
            contract_address,
            logs,
        }))
    }

    /// Submitted txs whose receipt hasn't been observed yet (for the poller).
    /// Least-attempted first (newest first within a tie) and capped at
    /// POLL_ATTEMPTS_CAP, so a backlog of never-included zombies can't
    /// permanently occupy the LIMIT window and starve newer txs.
    pub fn pending_txs(&self, limit: usize) -> Result<Vec<([u8; 32], Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT eth_hash, koinos_tx_id FROM txs
             WHERE status IS NULL AND koinos_tx_id IS NOT NULL AND poll_attempts < ?2
             ORDER BY poll_attempts ASC, rowid DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64, POLL_ATTEMPTS_CAP], |row| {
                Ok((blob32(row.get(0)?), row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record a failed settle attempt for a pending tx (see pending_txs).
    pub fn bump_poll_attempts(&self, eth_hash: &[u8; 32]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE txs SET poll_attempts = poll_attempts + 1 WHERE eth_hash = ?1",
            params![eth_hash.as_slice()],
        )?;
        Ok(())
    }

    /// Has this tx settled (receipt persisted)? Used by the durable dedupe in
    /// handle_send_raw_tx: only a SETTLED row short-circuits a resubmission —
    /// a status-NULL row may be a mempool-dropped tx that the sender must be
    /// able to resubmit.
    pub fn is_settled(&self, eth_hash: &[u8; 32]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM txs WHERE eth_hash = ?1 AND status IS NOT NULL",
            params![eth_hash.as_slice()],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// eth_getLogs query. Returns up to `limit` rows ordered by (block, log_index);
    /// the caller passes limit = max_results + 1 to detect overflow.
    pub fn query_logs(&self, f: &LogFilter, limit: usize) -> Result<Vec<LogQueryRow>> {
        let mut sql = String::from(
            "SELECT eth_hash, block_height, block_hash, tx_index, log_index,
                    address, topic0, topic1, topic2, topic3, data
             FROM logs WHERE ",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(bh) = &f.block_hash {
            sql.push_str("block_hash = ?");
            args.push(Box::new(bh.clone()));
        } else {
            sql.push_str("block_height >= ? AND block_height <= ?");
            // Clamp instead of `as i64`: u64 values above i64::MAX would
            // sign-flip negative and silently match nothing.
            args.push(Box::new(f.from_block.min(i64::MAX as u64) as i64));
            args.push(Box::new(f.to_block.min(i64::MAX as u64) as i64));
        }
        if !f.addresses.is_empty() {
            sql.push_str(" AND address IN (");
            push_placeholders(&mut sql, f.addresses.len());
            sql.push(')');
            for a in &f.addresses {
                args.push(Box::new(a.clone()));
            }
        }
        for (i, alternatives) in f.topics.iter().enumerate() {
            if !alternatives.is_empty() {
                sql.push_str(&format!(" AND topic{} IN (", i));
                push_placeholders(&mut sql, alternatives.len());
                sql.push(')');
                for t in alternatives {
                    args.push(Box::new(t.clone()));
                }
            }
        }
        sql.push_str(" ORDER BY block_height, eth_hash, log_index LIMIT ?");
        args.push(Box::new(limit as i64));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args.iter().map(|b| b.as_ref())), |row| {
                let mut topics = Vec::new();
                for i in 6..10 {
                    if let Some(t) = row.get::<_, Option<Vec<u8>>>(i)? {
                        topics.push(t);
                    }
                }
                Ok(LogQueryRow {
                    eth_hash: row.get(0)?,
                    block_height: row.get::<_, i64>(1)? as u64,
                    block_hash: row.get(2)?,
                    tx_index: row.get::<_, i64>(3)? as u64,
                    log: LogRow {
                        log_index: row.get::<_, i64>(4)? as u64,
                        address: row.get(5)?,
                        topics,
                        data: row.get(10)?,
                    },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

fn push_placeholders(sql: &mut String, n: usize) {
    for i in 0..n {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
}

fn row_to_block(row: &rusqlite::Row<'_>) -> rusqlite::Result<BlockRow> {
    Ok(BlockRow {
        height: row.get::<_, i64>(0)? as u64,
        block_hash: row.get(1)?,
        timestamp: row.get::<_, i64>(2)? as u64,
        tx_count: row.get::<_, i64>(3)? as u64,
        log_count: row.get::<_, i64>(4)? as u64,
    })
}

fn blob20(v: Vec<u8>) -> [u8; 20] {
    let mut out = [0u8; 20];
    let n = v.len().min(20);
    out[..n].copy_from_slice(&v[..n]);
    out
}

fn blob32(v: Vec<u8>) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = v.len().min(32);
    out[..n].copy_from_slice(&v[..n]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(koinos_id: bool) -> TxMeta {
        TxMeta {
            koinos_tx_id: if koinos_id { vec![9u8; 32] } else { Vec::new() },
            from: [1; 20],
            to: Some([2; 20]),
            nonce: 7,
            value: [3; 32],
            input: vec![0xab, 0xcd],
            gas_limit: 21_000,
            gas_price: [4; 32],
            raw_tx: vec![0xde, 0xad],
            tx_type: 2,
            chain_id: Some(42069),
            r: [5; 32],
            s: [6; 32],
            v: 1,
        }
    }

    fn receipt() -> ReceiptRow {
        ReceiptRow {
            block_height: 1234,
            block_hash: vec![0xbb; 32],
            tx_index: None,
            status: true,
            gas_used: 50_000,
            contract_address: None,
            logs: vec![
                LogRow {
                    log_index: 0,
                    address: vec![0xaa; 20],
                    topics: vec![vec![0x11; 32], vec![0x22; 32]],
                    data: vec![1, 2, 3],
                },
                LogRow {
                    log_index: 1,
                    address: vec![0xcc; 20],
                    topics: vec![vec![0x11; 32]],
                    data: vec![],
                },
            ],
        }
    }

    #[test]
    fn tx_meta_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let hash = [7u8; 32];
        db.upsert_tx(&hash, &meta(true)).unwrap();
        let m = db.get_tx_meta(&hash).unwrap().unwrap();
        assert_eq!(m.koinos_tx_id, vec![9u8; 32]);
        assert_eq!(m.from, [1; 20]);
        assert_eq!(m.to, Some([2; 20]));
        assert_eq!(m.nonce, 7);
        assert_eq!(m.gas_price, [4; 32]);
        assert_eq!(m.raw_tx, vec![0xde, 0xad]);
        assert_eq!(m.chain_id, Some(42069));
        assert!(db.get_tx_meta(&[8u8; 32]).unwrap().is_none());
    }

    #[test]
    fn upsert_does_not_wipe_receipt() {
        let db = Db::open_in_memory().unwrap();
        let hash = [7u8; 32];
        db.upsert_tx(&hash, &meta(true)).unwrap();
        db.set_receipt(&hash, &receipt()).unwrap();
        // Re-relay of the same tx (e.g. after memory eviction) must keep the receipt.
        db.upsert_tx(&hash, &meta(true)).unwrap();
        let r = db.get_receipt(&hash).unwrap().unwrap();
        assert_eq!(r.block_height, 1234);
        assert_eq!(r.logs.len(), 2);
        assert_eq!(r.logs[0].topics.len(), 2);
    }

    #[test]
    fn pending_then_settled() {
        let db = Db::open_in_memory().unwrap();
        let hash = [7u8; 32];
        db.upsert_tx(&hash, &meta(true)).unwrap();
        let pending = db.pending_txs(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, hash);
        db.set_receipt(&hash, &receipt()).unwrap();
        assert!(db.pending_txs(10).unwrap().is_empty());
    }

    #[test]
    fn poller_prefers_least_attempted_and_caps_zombies() {
        let db = Db::open_in_memory().unwrap();
        let zombie = [1u8; 32];
        let fresh = [2u8; 32];
        db.upsert_tx(&zombie, &meta(true)).unwrap();
        db.upsert_tx(&fresh, &meta(true)).unwrap();
        db.bump_poll_attempts(&zombie).unwrap();
        // Least-attempted first: fresh outranks the zombie even though it's newer.
        let pending = db.pending_txs(1).unwrap();
        assert_eq!(pending[0].0, fresh);
        // Beyond the cap, the zombie drops out of the poll window entirely.
        for _ in 0..POLL_ATTEMPTS_CAP {
            db.bump_poll_attempts(&zombie).unwrap();
        }
        let pending = db.pending_txs(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, fresh);
    }

    #[test]
    fn is_settled_only_after_receipt() {
        let db = Db::open_in_memory().unwrap();
        let hash = [7u8; 32];
        db.upsert_tx(&hash, &meta(true)).unwrap();
        // Pending row must NOT block resubmission.
        assert!(!db.is_settled(&hash).unwrap());
        db.set_receipt(&hash, &receipt()).unwrap();
        assert!(db.is_settled(&hash).unwrap());
    }

    #[test]
    fn index_tx_assigns_block_global_indexes_idempotently() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let ra = receipt(); // 2 logs
        let rb = receipt(); // 2 logs

        // First tx in block 1234: tx_index 0, log indexes 0,1.
        assert!(db.index_tx(&a, &meta(true), &ra, 999).unwrap());
        // Second tx: tx_index 1, log indexes 2,3.
        assert!(db.index_tx(&b, &meta(true), &rb, 999).unwrap());

        let got_a = db.get_receipt(&a).unwrap().unwrap();
        let got_b = db.get_receipt(&b).unwrap().unwrap();
        assert_eq!(got_a.tx_index, Some(0));
        assert_eq!(got_b.tx_index, Some(1));
        assert_eq!(
            got_b.logs.iter().map(|l| l.log_index).collect::<Vec<_>>(),
            vec![2, 3]
        );

        // Re-pass (reversible-window tail): no double counting.
        assert!(!db.index_tx(&a, &meta(true), &ra, 999).unwrap());
        let blk = db.get_block_by_height(1234).unwrap().unwrap();
        assert_eq!(blk.tx_count, 2);
        assert_eq!(blk.log_count, 4);
        assert_eq!(db.block_tx_hashes(1234).unwrap(), vec![a, b]);
        assert_eq!(db.block_gas_used(1234).unwrap(), 100_000);
        assert_eq!(db.block_logs(1234).unwrap().len(), 4);

        // Provisional settle first, indexer overwrites with real indexes.
        let c = [3u8; 32];
        db.upsert_tx(&c, &meta(true)).unwrap();
        db.set_receipt(&c, &ra).unwrap();
        assert_eq!(db.get_receipt(&c).unwrap().unwrap().tx_index, None);
        assert!(db.index_tx(&c, &meta(true), &ra, 999).unwrap());
        assert_eq!(db.get_receipt(&c).unwrap().unwrap().tx_index, Some(2));
    }

    #[test]
    fn failed_duplicate_never_overwrites_success() {
        let db = Db::open_in_memory().unwrap();
        let hash = [7u8; 32];
        let ok = receipt(); // status: true, block 1234
        assert!(db.index_tx(&hash, &meta(true), &ok, 999).unwrap());

        // The same raw tx re-submitted later (different Koinos tx/block) is
        // rejected by the engine nonce check → status false. Must not clobber.
        let mut dup = receipt();
        dup.status = false;
        dup.gas_used = 0;
        dup.block_height = 9999;
        dup.block_hash = vec![0xee; 32];
        dup.logs.clear();
        assert!(!db.index_tx(&hash, &meta(true), &dup, 999).unwrap());

        let r = db.get_receipt(&hash).unwrap().unwrap();
        assert!(r.status);
        assert_eq!(r.block_height, 1234);
        assert_eq!(r.logs.len(), 2);
    }

    #[test]
    fn meta_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get_meta("indexer_cursor").unwrap().is_none());
        db.set_meta("indexer_cursor", "41").unwrap();
        db.set_meta("indexer_cursor", "42").unwrap();
        assert_eq!(
            db.get_meta("indexer_cursor").unwrap().as_deref(),
            Some("42")
        );
    }

    #[test]
    fn set_receipt_requires_parent_row() {
        let db = Db::open_in_memory().unwrap();
        // No txs row → must error, and must NOT leave orphan logs behind.
        assert!(db.set_receipt(&[9u8; 32], &receipt()).is_err());
        let f = LogFilter {
            from_block: 0,
            to_block: 9999,
            ..Default::default()
        };
        assert!(db.query_logs(&f, 100).unwrap().is_empty());
    }

    #[test]
    fn query_logs_filters() {
        let db = Db::open_in_memory().unwrap();
        let hash = [7u8; 32];
        db.upsert_tx(&hash, &meta(true)).unwrap();
        db.set_receipt(&hash, &receipt()).unwrap();

        // address filter
        let f = LogFilter {
            from_block: 0,
            to_block: 9999,
            addresses: vec![vec![0xaa; 20]],
            ..Default::default()
        };
        let rows = db.query_logs(&f, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].log.data, vec![1, 2, 3]);

        // topic0 filter matches both logs
        let f = LogFilter {
            from_block: 0,
            to_block: 9999,
            topics: [vec![vec![0x11; 32]], vec![], vec![], vec![]],
            ..Default::default()
        };
        assert_eq!(db.query_logs(&f, 100).unwrap().len(), 2);

        // topic1 filter matches only the first
        let f = LogFilter {
            from_block: 0,
            to_block: 9999,
            topics: [vec![], vec![vec![0x22; 32]], vec![], vec![]],
            ..Default::default()
        };
        assert_eq!(db.query_logs(&f, 100).unwrap().len(), 1);

        // out-of-range block window
        let f = LogFilter {
            from_block: 0,
            to_block: 100,
            ..Default::default()
        };
        assert!(db.query_logs(&f, 100).unwrap().is_empty());

        // block_hash filter
        let f = LogFilter {
            block_hash: Some(vec![0xbb; 32]),
            ..Default::default()
        };
        assert_eq!(db.query_logs(&f, 100).unwrap().len(), 2);
    }
}
