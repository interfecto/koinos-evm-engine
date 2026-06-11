//! Shared application state + configuration.

use crate::koinos::KoinosClient;
use anyhow::Context;
use base64::Engine;
use secp256k1::SecretKey;
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Configuration sourced from environment variables.
pub struct Config {
    pub listen_addr: String,
    /// Interval (seconds) for the periodic operator-nonce reconcile task.
    /// 0 disables periodic reconciliation (error-triggered resync still applies).
    pub nonce_reconcile_secs: u64,
    /// Max entries in the in-memory tx_meta map (FIFO-evicted beyond this).
    pub tx_meta_max: usize,
    /// TTL (seconds) for tx_meta entries; expired entries are swept on insert.
    pub tx_meta_ttl_secs: u64,
    /// Max entries in the per-sender pending-nonce map.
    pub pending_nonce_max: usize,
    /// TTL (seconds) for pending-nonce entries. After this long without a new submit
    /// from a sender, eth_getTransactionCount("pending") falls back to the on-chain
    /// nonce — this is the receipt/TTL reset that heals stale too-high entries.
    pub pending_nonce_ttl_secs: u64,
    /// Comma-separated CORS origin allowlist; "*" restores the old fully-permissive
    /// behavior (only sensible behind loopback). Browsers enforce this; non-browser
    /// clients (cast, curl) are unaffected.
    pub cors_allowed_origins: String,
    /// Per-client-IP rate limit: sustained requests/second (token-bucket refill rate).
    /// 0 disables rate limiting.
    pub rate_limit_rps: u64,
    /// Per-client-IP burst capacity (token-bucket size).
    pub rate_limit_burst: u64,
    /// Behind a reverse proxy on THIS host (e.g. nginx terminating TLS), the TCP
    /// peer is always loopback, so per-IP limiting collapses to one shared bucket.
    /// When true, requests arriving FROM LOOPBACK take the client IP from
    /// X-Real-IP (set by the proxy) or the last X-Forwarded-For hop instead.
    /// Never enable without a trusted proxy in front — the headers are
    /// client-forgeable on direct connections (which is why non-loopback peers
    /// always keep their TCP address regardless of this flag).
    pub trust_proxy_headers: bool,
    /// Max number of requests in one JSON-RPC batch.
    pub rpc_max_batch: usize,
    /// Max HTTP request body size in bytes.
    pub rpc_max_body_bytes: usize,
    /// Admission floor: reject eth_sendRawTransaction whose committed gas price
    /// (legacy gas_price / 1559 max_fee_per_gas) is below this many wei. The engine
    /// still charges 0 — this is an anti-spam gate, not a fee market. 0 disables.
    /// eth_gasPrice / eth_feeHistory advertise this floor so wallets auto-comply.
    pub min_gas_price_wei: u128,
    /// SQLite path for the durable tx/receipt/log store.
    pub db_path: String,
    /// eth_getLogs: max block range per query (-32005 beyond, so clients chunk).
    pub getlogs_max_block_range: u64,
    /// eth_getLogs: max result rows per query (-32005 beyond).
    pub getlogs_max_results: usize,
    /// Interval (seconds) for the background receipt poller that settles pending
    /// txs into the durable store without waiting for a client to poll. 0 disables.
    pub receipt_poll_secs: u64,
    /// Interval (seconds) for the account-history backfill indexer (ROADMAP §2):
    /// backfills the FULL engine history into the store, then tails the head.
    /// 0 disables (the provisional receipt poller still settles relayed txs).
    pub indexer_poll_secs: u64,
    /// Interval (seconds) for the WebSocket pollers (newHeads + indexed-logs
    /// tail). 0 disables the push feeds (the WS endpoint still answers RPC).
    pub ws_poll_secs: u64,
    /// account_history page size per indexer request.
    pub indexer_page_size: usize,
    /// eth_estimateGas fallback: when the estimation view hits the node's
    /// read-compute limit (-1013) — which even small writes do on a default
    /// public node, because revm-in-WASM is interpreter-on-interpreter — return
    /// this gas value instead of erroring. Over-estimating is free on this chain
    /// (users pay zero gas; relay mana is independent of the EVM gas figure).
    /// 0 disables the fallback (the -32005 error propagates).
    pub estimate_gas_fallback: u64,
    pub koinos_rpc_url: String,
    pub koinos_rest_url: String,
    /// Human base58check form (e.g. "1E8igxy..."). Used by chain.read_contract / get_account_nonce
    /// AND chain.submit_transaction (in the JSON `payer` / `contract_id` fields).
    pub engine_contract_addr_b58check: String,
    /// Raw base58 form of the 20-byte hash160 (e.g. "31QYFBy..."). Currently unused but kept for debug.
    pub engine_contract_addr_b58: String,
    /// Raw 20-byte hash160 of the engine contract address. Kept for debug.
    #[allow(dead_code)]
    pub engine_contract_id: Vec<u8>,
    /// Full 25-byte payload (version + hash160 + checksum) — what Koinos's JSON deserializer
    /// produces for an ADDRESS/CONTRACT_ID field. Our hand-encoded protobuf headers must use
    /// these bytes so that `sha256(header)` matches Koinos's internal computation.
    pub engine_contract_addr_full: Vec<u8>,
    /// Koinos chain ID (base64-encoded multihash), source of `koinos_chain_id_bytes`.
    /// Kept for debug/logging.
    #[allow(dead_code)]
    pub koinos_chain_id_b64: String,
    /// Raw bytes of the chain ID multihash (decoded from `koinos_chain_id_b64`).
    pub koinos_chain_id_bytes: Vec<u8>,
    /// EVM chain ID (uint), matches `ENGINE_CHAIN_ID` in the engine.
    pub evm_chain_id: u64,
    /// Proxy operator's secp256k1 private key — pays Koinos mana for relayed txs.
    pub operator_key: SecretKey,
    /// Proxy operator's Koinos address (raw base58 — currently unused, kept for debug).
    #[allow(dead_code)]
    pub operator_addr_b58: String,
    /// Proxy operator's human-readable base58check form (used everywhere in JSON).
    pub operator_addr_b58check: String,
    /// 20-byte hash160 of operator's pubkey. Kept for debug.
    #[allow(dead_code)]
    pub operator_addr_bytes: Vec<u8>,
    /// Full 25-byte payload (version + hash160 + checksum) — used in hand-encoded proto headers.
    pub operator_addr_full: Vec<u8>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        // Default bind 127.0.0.1: prevents accidentally exposing the proxy + operator key
        // to the LAN. Operators who want public access can set LISTEN_ADDR=0.0.0.0:8545 explicitly.
        let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8545".into());
        let nonce_reconcile_secs: u64 = std::env::var("NONCE_RECONCILE_SECS")
            .unwrap_or_else(|_| "30".into())
            .parse()
            .context("NONCE_RECONCILE_SECS must be a number (seconds, 0 = disabled)")?;
        let tx_meta_max: usize = std::env::var("TX_META_MAX")
            .unwrap_or_else(|_| "10000".into())
            .parse()
            .context("TX_META_MAX must be a number")?;
        let tx_meta_ttl_secs: u64 = std::env::var("TX_META_TTL_SECS")
            .unwrap_or_else(|_| "3600".into())
            .parse()
            .context("TX_META_TTL_SECS must be a number (seconds)")?;
        let pending_nonce_max: usize = std::env::var("PENDING_NONCE_MAX")
            .unwrap_or_else(|_| "10000".into())
            .parse()
            .context("PENDING_NONCE_MAX must be a number")?;
        let pending_nonce_ttl_secs: u64 = std::env::var("PENDING_NONCE_TTL_SECS")
            .unwrap_or_else(|_| "600".into())
            .parse()
            .context("PENDING_NONCE_TTL_SECS must be a number (seconds)")?;
        // Default allowlist covers the bundled UIs / common local dev ports. Set "*"
        // to restore fully-permissive CORS (matches the old behavior).
        let cors_allowed_origins = std::env::var("CORS_ALLOWED_ORIGINS").unwrap_or_else(|_| {
            "http://localhost:8080,http://127.0.0.1:8080,http://localhost:3000,http://127.0.0.1:3000"
                .into()
        });
        // Generous defaults: on a loopback dev setup MetaMask, the bundled UIs,
        // and deploy scripts all share the single 127.0.0.1 bucket, and ethers v6
        // coalesces concurrent calls into batches of up to 100 (charged at their
        // length) — a tight burst would 429 normal dev traffic.
        let rate_limit_rps: u64 = std::env::var("RATE_LIMIT_RPS")
            .unwrap_or_else(|_| "50".into())
            .parse()
            .context("RATE_LIMIT_RPS must be a number (requests/second, 0 = disabled)")?;
        let rate_limit_burst: u64 = std::env::var("RATE_LIMIT_BURST")
            .unwrap_or_else(|_| "500".into())
            .parse()
            .context("RATE_LIMIT_BURST must be a number")?;
        let trust_proxy_headers = matches!(
            std::env::var("TRUST_PROXY_HEADERS")
                .unwrap_or_else(|_| "0".into())
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        );
        let rpc_max_batch: usize = std::env::var("RPC_MAX_BATCH")
            .unwrap_or_else(|_| "100".into())
            .parse()
            .context("RPC_MAX_BATCH must be a number")?;
        let rpc_max_body_bytes: usize = std::env::var("RPC_MAX_BODY_BYTES")
            .unwrap_or_else(|_| "1048576".into())
            .parse()
            .context("RPC_MAX_BODY_BYTES must be a number (bytes)")?;
        let min_gas_price_wei: u128 = std::env::var("MIN_GAS_PRICE_WEI")
            .unwrap_or_else(|_| "0".into())
            .parse()
            .context("MIN_GAS_PRICE_WEI must be a number (wei, 0 = disabled)")?;
        let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "./koinos-evm-rpc.sqlite".into());
        let getlogs_max_block_range: u64 = std::env::var("GETLOGS_MAX_BLOCK_RANGE")
            .unwrap_or_else(|_| "10000".into())
            .parse()
            .context("GETLOGS_MAX_BLOCK_RANGE must be a number (blocks)")?;
        let getlogs_max_results: usize = std::env::var("GETLOGS_MAX_RESULTS")
            .unwrap_or_else(|_| "10000".into())
            .parse()
            .context("GETLOGS_MAX_RESULTS must be a number")?;
        let receipt_poll_secs: u64 = std::env::var("RECEIPT_POLL_SECS")
            .unwrap_or_else(|_| "3".into())
            .parse()
            .context("RECEIPT_POLL_SECS must be a number (seconds, 0 = disabled)")?;
        let indexer_poll_secs: u64 = std::env::var("INDEXER_POLL_SECS")
            .unwrap_or_else(|_| "3".into())
            .parse()
            .context("INDEXER_POLL_SECS must be a number (seconds, 0 = disabled)")?;
        let indexer_page_size: usize = std::env::var("INDEXER_PAGE_SIZE")
            .unwrap_or_else(|_| "50".into())
            .parse()
            .context("INDEXER_PAGE_SIZE must be a number")?;
        let ws_poll_secs: u64 = std::env::var("WS_POLL_SECS")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .context("WS_POLL_SECS must be a number (seconds, 0 = disabled)")?;
        // 5M covers everything exercised so far (ERC-20 ops ~51k, Uniswap swaps
        // ~150-300k, NFPM mint ~500k, pool CREATE2 ~4-5M) with room, and is well
        // under the 30M block gas limit.
        let estimate_gas_fallback: u64 = std::env::var("ESTIMATE_GAS_FALLBACK")
            .unwrap_or_else(|_| "5000000".into())
            .parse()
            .context("ESTIMATE_GAS_FALLBACK must be a number (gas, 0 = disabled)")?;
        let koinos_rpc_url = std::env::var("KOINOS_RPC_URL")
            .unwrap_or_else(|_| "https://testnet.koinosfoundation.org/jsonrpc".into());
        let koinos_rest_url = std::env::var("KOINOS_REST_URL")
            .unwrap_or_else(|_| "https://testnet.koinosfoundation.org/v1".into());
        let engine_contract_addr_b58 = std::env::var("ENGINE_CONTRACT")
            .unwrap_or_else(|_| "1E8igxyDU3hjbqvcoWXGFG2pRR5xLcAaoE".into());
        let koinos_chain_id_b64 = std::env::var("KOINOS_CHAIN_ID")
            .unwrap_or_else(|_| "EiAIKVvm6-V2qmsmUvPJy09vCCLbtn9lHFpwrJbcTIEWRQ==".into());
        let evm_chain_id: u64 = std::env::var("EVM_CHAIN_ID")
            .unwrap_or_else(|_| "42069".into())
            .parse()
            .context("EVM_CHAIN_ID must be a number")?;

        // Operator key: hex private key
        let operator_key_hex = std::env::var("OPERATOR_PRIVKEY_HEX")
            .context("OPERATOR_PRIVKEY_HEX env var required (32-byte hex secp256k1 private key)")?;
        let key_bytes = hex::decode(operator_key_hex.trim_start_matches("0x"))
            .context("OPERATOR_PRIVKEY_HEX must be valid hex")?;
        let operator_key = SecretKey::from_slice(&key_bytes)
            .context("OPERATOR_PRIVKEY_HEX must be a valid 32-byte secp256k1 secret")?;

        // Derive operator Koinos address from key
        let secp = secp256k1::Secp256k1::new();
        let pubkey = secp256k1::PublicKey::from_secret_key(&secp, &operator_key);
        let pubkey_compressed = pubkey.serialize();
        let operator_addr_bytes = koinos_address_from_pubkey_compressed(&pubkey_compressed);
        // Koinos JSON-RPC uses RAW base58 (no checksum) for ADDRESS/CONTRACT_ID. We keep BOTH:
        //   `operator_addr_b58` is the raw form used in JSON-RPC requests.
        //   The base58check human-readable form is logged at startup for operator UX.
        let operator_addr_b58 = bs58_no_check(&operator_addr_bytes);
        let operator_addr_b58check = base58check_encode(0x00, &operator_addr_bytes);

        let engine_contract_id = base58check_decode(&engine_contract_addr_b58)?;
        let engine_contract_addr_b58check = engine_contract_addr_b58.clone();
        // FULL 25-byte payload (what Koinos's JSON deserializer returns for ADDRESS/CONTRACT_ID)
        let engine_contract_addr_full = bs58_decode(&engine_contract_addr_b58check)?;
        let engine_contract_addr_b58 = bs58_no_check(&engine_contract_id);
        let operator_addr_full = bs58_decode(&operator_addr_b58check)?;
        tracing::info!(operator_human = %operator_addr_b58check, "operator address");

        // Decode chain_id from URL-safe base64 (with optional padding)
        let koinos_chain_id_bytes = base64::engine::general_purpose::URL_SAFE
            .decode(&koinos_chain_id_b64)
            .or_else(|_| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&koinos_chain_id_b64)
            })
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&koinos_chain_id_b64))
            .context("KOINOS_CHAIN_ID must be valid base64 of the chain_id multihash")?;

        Ok(Self {
            listen_addr,
            nonce_reconcile_secs,
            tx_meta_max,
            tx_meta_ttl_secs,
            pending_nonce_max,
            pending_nonce_ttl_secs,
            cors_allowed_origins,
            rate_limit_rps,
            rate_limit_burst,
            trust_proxy_headers,
            rpc_max_batch,
            rpc_max_body_bytes,
            min_gas_price_wei,
            db_path,
            getlogs_max_block_range,
            getlogs_max_results,
            receipt_poll_secs,
            indexer_poll_secs,
            indexer_page_size,
            ws_poll_secs,
            estimate_gas_fallback,
            koinos_rpc_url,
            koinos_rest_url,
            engine_contract_addr_b58check,
            engine_contract_addr_b58,
            engine_contract_id,
            engine_contract_addr_full,
            koinos_chain_id_b64,
            koinos_chain_id_bytes,
            evm_chain_id,
            operator_key,
            operator_addr_full,
            operator_addr_b58check,
            operator_addr_b58,
            operator_addr_bytes,
        })
    }
}

/// Metadata captured at eth_sendRawTransaction time, used by F5 receipt/tx lookups.
#[derive(Clone)]
pub struct TxMeta {
    /// Koinos tx id (raw 32-byte hash, no multihash prefix). Used to fetch the receipt.
    pub koinos_tx_id: Vec<u8>,
    /// Recovered EVM sender from the raw_tx (`from` field in eth_getTransactionByHash response).
    pub from: [u8; 20],
    /// Decoded `to` field (None for CREATE).
    pub to: Option<[u8; 20]>,
    /// Tx nonce.
    pub nonce: u64,
    /// Tx value (32-byte BE).
    pub value: [u8; 32],
    /// Tx data / input.
    pub input: Vec<u8>,
    /// Tx gas_limit.
    pub gas_limit: u64,
    /// Sender-committed gas price (32-byte BE): legacy gas_price or 1559 max_fee_per_gas.
    pub gas_price: [u8; 32],
    /// Raw signed bytes — persisted by the durable tx store for restart-safe lookups.
    pub raw_tx: Vec<u8>,
    /// Tx type (0 = legacy, 1 = EIP-2930, 2 = EIP-1559).
    pub tx_type: u8,
    /// Chain ID parsed from tx (for EIP-155+ txs).
    pub chain_id: Option<u64>,
    /// Signature components (preserved so eth_getTransactionByHash returns real values).
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u64,
}

/// Bounded tx-metadata store: FIFO + TTL eviction so memory stays bounded under
/// sustained load (the map is attacker-growable on an exposed endpoint — one entry
/// per relayed tx). Entries are swept on insert: expired entries first, then the
/// oldest entries beyond the size cap.
///
/// CAVEAT: once an entry is evicted, a resubmission of the identical raw_tx passes
/// the dedupe check in handle_send_raw_tx and is re-relayed (the engine then rejects
/// it on the EVM nonce check, but the relay still pays the Koinos mana). The default
/// cap/TTL make this a non-issue for dev; durable persistence (ROADMAP §2) is the
/// real fix.
pub struct TxMetaStore {
    map: HashMap<[u8; 32], (TxMeta, Instant)>,
    /// Insertion order for FIFO eviction. May contain stale hashes (removed or
    /// re-inserted entries); eviction skips entries whose Instant doesn't match.
    order: VecDeque<([u8; 32], Instant)>,
    cap: usize,
    ttl: Duration,
}

impl TxMetaStore {
    pub fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
            ttl,
        }
    }

    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        self.map.contains_key(hash)
    }

    /// TTL governs eviction only — a not-yet-swept expired entry is still returned
    /// (better than answering null for a receipt we do know about).
    pub fn get(&self, hash: &[u8; 32]) -> Option<TxMeta> {
        self.map.get(hash).map(|(m, _)| m.clone())
    }

    pub fn insert(&mut self, hash: [u8; 32], meta: TxMeta) {
        let now = Instant::now();
        self.map.insert(hash, (meta, now));
        self.order.push_back((hash, now));
        self.evict(now);
    }

    /// Patch the koinos_tx_id of an existing entry (set after successful submit).
    pub fn set_koinos_tx_id(&mut self, hash: &[u8; 32], koinos_tx_id: Vec<u8>) {
        if let Some((meta, _)) = self.map.get_mut(hash) {
            meta.koinos_tx_id = koinos_tx_id;
        }
    }

    pub fn remove(&mut self, hash: &[u8; 32]) {
        self.map.remove(hash);
        // The matching order entry is skipped at eviction time (Instant mismatch / absent).
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    fn evict(&mut self, now: Instant) {
        // 1. Expired entries from the front (insertion order ⇒ oldest first).
        while let Some((hash, inserted)) = self.order.front().copied() {
            let map_match = self.map.get(&hash).is_some_and(|(_, t)| *t == inserted);
            if !map_match {
                // Stale order entry (removed or re-inserted) — just drop it.
                self.order.pop_front();
            } else if now.duration_since(inserted) >= self.ttl {
                self.order.pop_front();
                self.map.remove(&hash);
            } else {
                break;
            }
        }
        // 2. Size cap: evict oldest until within bounds.
        while self.map.len() > self.cap {
            match self.order.pop_front() {
                Some((hash, inserted)) => {
                    if self.map.get(&hash).is_some_and(|(_, t)| *t == inserted) {
                        self.map.remove(&hash);
                    }
                }
                None => break, // unreachable: map non-empty ⇒ order non-empty
            }
        }
    }
}

/// Bounded per-sender pending-nonce store with TTL. The TTL doubles as the
/// "receipt-driven reset" hardening: a stale too-high entry (e.g. from a relay the
/// engine accepted at the Koinos layer that never advanced the EVM nonce) expires
/// instead of wedging that sender's eth_getTransactionCount("pending") forever.
pub struct PendingNonceStore {
    map: HashMap<[u8; 20], (u64, Instant)>,
    cap: usize,
    ttl: Duration,
}

impl PendingNonceStore {
    pub fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            map: HashMap::new(),
            cap: cap.max(1),
            ttl,
        }
    }

    /// The tracked next-nonce for a sender, or None if absent/expired.
    pub fn get_unexpired(&self, addr: &[u8; 20]) -> Option<u64> {
        let (nonce, t) = self.map.get(addr)?;
        if t.elapsed() >= self.ttl {
            None
        } else {
            Some(*nonce)
        }
    }

    /// Record `next` as the sender's next nonce if it advances the tracked value
    /// (refreshes the TTL either way).
    pub fn advance(&mut self, addr: [u8; 20], next: u64) {
        let now = Instant::now();
        match self.map.get_mut(&addr) {
            Some((cur, t)) => {
                if next > *cur {
                    *cur = next;
                }
                *t = now;
            }
            None => {
                if self.map.len() >= self.cap {
                    self.evict(now);
                }
                self.map.insert(addr, (next, now));
            }
        }
    }

    pub fn remove(&mut self, addr: &[u8; 20]) {
        self.map.remove(addr);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    fn evict(&mut self, now: Instant) {
        // Sweep expired entries; if still at cap, drop the single oldest entry.
        // O(n) scans, but only on overflow of a 10k-default map — fine for the proxy.
        self.map
            .retain(|_, (_, t)| now.duration_since(*t) < self.ttl);
        if self.map.len() >= self.cap
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| *k)
        {
            self.map.remove(&oldest);
        }
    }
}

pub struct AppState {
    pub config: Config,
    pub client: KoinosClient,
    /// Bounded in-memory map from Ethereum tx hash → metadata.
    /// Populated when we relay a tx via eth_sendRawTransaction.
    /// Used by eth_getTransactionReceipt / eth_getTransactionByHash.
    pub tx_meta: RwLock<TxMetaStore>,
    /// Async-safe operator nonce gate. Serializes eth_sendRawTransaction calls so
    /// concurrent requests don't race on the operator's Koinos nonce.
    /// Holds the NEXT nonce to use. Initialized to None; populated lazily from chain.
    pub operator_nonce: Mutex<Option<u64>>,
    /// Per-EVM-sender highest pending nonce (the NEXT nonce that sender should use),
    /// updated on each successful eth_sendRawTransaction relay. `eth_getTransactionCount`
    /// with the "pending" block tag returns max(on-chain nonce, this) so a wallet sending
    /// back-to-back txs doesn't reuse a nonce while the first is still in flight — the
    /// engine requires strictly sequential nonces and would otherwise reject the 2nd tx.
    /// In-memory only: on restart it falls back to the on-chain nonce (a brief pending-gap
    /// window, self-healed as soon as the in-flight txs commit). Stale entries are dropped
    /// in handle_get_tx_count once the chain catches up, and expire via TTL otherwise.
    pub pending_nonce: RwLock<PendingNonceStore>,
    /// Unix seconds of the last SUCCESSFUL relayed submit (0 = never). The periodic
    /// nonce reconciler only steps the cached operator nonce DOWN to the chain value
    /// after a quiet period without successful submits — a cached value legitimately
    /// runs ahead of chain while our txs sit in the mempool, and stepping down too
    /// eagerly would re-issue a nonce that is still in flight.
    pub last_submit_ok: std::sync::atomic::AtomicU64,
    /// Per-client-IP token-bucket rate limiter (RATE_LIMIT_RPS / RATE_LIMIT_BURST).
    pub rate_limiter: crate::limit::RateLimiter,
    /// Durable SQLite store: relayed txs + observed receipts/logs (restart-safe
    /// lookups + eth_getLogs). The in-memory tx_meta store remains the hot path.
    pub db: crate::db::Db,
    /// Broadcast channel from the WS pollers to every live WebSocket connection
    /// (newHeads + newly indexed logs). Send errors mean "no subscribers".
    pub ws_events: tokio::sync::broadcast::Sender<std::sync::Arc<crate::ws::WsEvent>>,
    /// Number of relayed submits currently in flight (reserved nonce, network
    /// round-trip not yet settled). Stepping the cached operator nonce DOWN is
    /// only safe when this is zero — the chain's committed nonce is blind to
    /// mempool txs, so a downward reset while submits are in flight would
    /// re-issue their nonces.
    pub inflight_submits: std::sync::atomic::AtomicU64,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let client = KoinosClient::new(&config.koinos_rpc_url, &config.koinos_rest_url)?;
        let tx_meta = TxMetaStore::new(
            config.tx_meta_max,
            Duration::from_secs(config.tx_meta_ttl_secs),
        );
        let pending_nonce = PendingNonceStore::new(
            config.pending_nonce_max,
            Duration::from_secs(config.pending_nonce_ttl_secs),
        );
        let rate_limiter =
            crate::limit::RateLimiter::new(config.rate_limit_rps, config.rate_limit_burst);
        let db = crate::db::Db::open(&config.db_path)?;
        let (ws_events, _) = tokio::sync::broadcast::channel(256);
        Ok(Self {
            config,
            client,
            tx_meta: RwLock::new(tx_meta),
            operator_nonce: Mutex::new(None),
            pending_nonce: RwLock::new(pending_nonce),
            last_submit_ok: std::sync::atomic::AtomicU64::new(0),
            rate_limiter,
            db,
            ws_events,
            inflight_submits: std::sync::atomic::AtomicU64::new(0),
        })
    }
}

// ── Koinos address helpers ───────────────────────────────────────────────

fn koinos_address_from_pubkey_compressed(pubkey: &[u8]) -> Vec<u8> {
    // Koinos address = RIPEMD-160(SHA-256(pubkey_compressed))
    use ripemd::Ripemd160;
    use sha2::{Digest, Sha256};
    let sha = Sha256::digest(pubkey);
    let rip = Ripemd160::digest(sha);
    rip.to_vec()
}

fn base58check_encode(version: u8, payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut buf = Vec::with_capacity(1 + payload.len() + 4);
    buf.push(version);
    buf.extend_from_slice(payload);
    let check = Sha256::digest(Sha256::digest(&buf));
    buf.extend_from_slice(&check[..4]);
    bs58(&buf)
}

/// Raw base58 encode (NO checksum, NO version byte) — what Koinos JSON-RPC expects
/// for `(btype) = ADDRESS` and `(btype) = CONTRACT_ID` bytes fields.
pub fn bs58_no_check(payload: &[u8]) -> String {
    bs58(payload)
}

fn base58check_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let raw = bs58_decode(s)?;
    if raw.len() < 5 {
        anyhow::bail!("base58check string too short");
    }
    let (payload, check) = raw.split_at(raw.len() - 4);
    let expected = Sha256::digest(Sha256::digest(payload));
    if &expected[..4] != check {
        anyhow::bail!("base58check checksum mismatch");
    }
    Ok(payload[1..].to_vec()) // strip version byte → 20-byte hash160
}

const BS58_ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn bs58(bytes: &[u8]) -> String {
    let mut zeros = 0;
    for &b in bytes {
        if b == 0 {
            zeros += 1;
        } else {
            break;
        }
    }
    let mut n: Vec<u8> = bytes.to_vec();
    let mut out: Vec<u8> = Vec::new();
    let mut start = zeros;
    while start < n.len() {
        let mut rem = 0u32;
        for byte in &mut n[start..] {
            let cur = rem * 256 + *byte as u32;
            *byte = (cur / 58) as u8;
            rem = cur % 58;
        }
        out.push(BS58_ALPHABET[rem as usize]);
        if n[start] == 0 {
            start += 1;
        }
    }
    out.extend(std::iter::repeat_n(b'1', zeros));
    out.reverse();
    String::from_utf8(out).unwrap()
}

pub fn bs58_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    let mut zeros = 0;
    for c in s.chars() {
        if c == '1' {
            zeros += 1;
        } else {
            break;
        }
    }
    let mut n = vec![0u8; s.len()];
    let mut n_len = 0;
    for c in s.chars() {
        let i = BS58_ALPHABET
            .iter()
            .position(|&x| x == c as u8)
            .ok_or_else(|| anyhow::anyhow!("invalid base58 character: {:?}", c))?;
        let mut carry = i as u32;
        for byte in n.iter_mut().take(n_len) {
            let cur = (*byte as u32) * 58 + carry;
            *byte = (cur & 0xff) as u8;
            carry = cur >> 8;
        }
        while carry > 0 {
            n[n_len] = (carry & 0xff) as u8;
            n_len += 1;
            carry >>= 8;
        }
    }
    n.truncate(n_len);
    let mut out = vec![0u8; zeros];
    out.extend(n.iter().rev());
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_meta() -> TxMeta {
        TxMeta {
            koinos_tx_id: vec![0; 32],
            from: [1; 20],
            to: None,
            nonce: 0,
            value: [0; 32],
            input: Vec::new(),
            gas_limit: 21_000,
            gas_price: [0; 32],
            raw_tx: Vec::new(),
            tx_type: 2,
            chain_id: Some(42069),
            r: [0; 32],
            s: [0; 32],
            v: 0,
        }
    }

    fn hash(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn tx_meta_store_caps_size_fifo() {
        let mut store = TxMetaStore::new(3, Duration::from_secs(3600));
        for i in 0..5u8 {
            store.insert(hash(i), dummy_meta());
        }
        assert_eq!(store.len(), 3);
        // Oldest two evicted, newest three retained.
        assert!(!store.contains(&hash(0)));
        assert!(!store.contains(&hash(1)));
        assert!(store.contains(&hash(2)));
        assert!(store.contains(&hash(4)));
    }

    #[test]
    fn tx_meta_store_ttl_evicts_on_insert() {
        let mut store = TxMetaStore::new(100, Duration::from_secs(0)); // everything expires instantly
        store.insert(hash(1), dummy_meta());
        store.insert(hash(2), dummy_meta());
        // Each insert sweeps expired entries from the front; only the just-inserted
        // entry can survive (and is itself swept by the next insert).
        assert!(store.len() <= 1);
        assert!(!store.contains(&hash(1)));
    }

    #[test]
    fn tx_meta_store_remove_then_evict_skips_stale_order() {
        let mut store = TxMetaStore::new(2, Duration::from_secs(3600));
        store.insert(hash(1), dummy_meta());
        store.remove(&hash(1));
        store.insert(hash(2), dummy_meta());
        store.insert(hash(3), dummy_meta());
        store.insert(hash(4), dummy_meta()); // overflow: must evict hash(2), not choke on stale hash(1)
        assert_eq!(store.len(), 2);
        assert!(!store.contains(&hash(2)));
        assert!(store.contains(&hash(3)));
        assert!(store.contains(&hash(4)));
    }

    #[test]
    fn tx_meta_store_set_koinos_tx_id() {
        let mut store = TxMetaStore::new(10, Duration::from_secs(3600));
        store.insert(hash(1), dummy_meta());
        store.set_koinos_tx_id(&hash(1), vec![7; 32]);
        assert_eq!(store.get(&hash(1)).unwrap().koinos_tx_id, vec![7; 32]);
    }

    #[test]
    fn pending_nonce_advance_keeps_max_and_caps() {
        let mut store = PendingNonceStore::new(2, Duration::from_secs(3600));
        store.advance([1; 20], 5);
        store.advance([1; 20], 3); // lower value must not regress the tracked nonce
        assert_eq!(store.get_unexpired(&[1; 20]), Some(5));

        store.advance([2; 20], 1);
        store.advance([3; 20], 1); // overflow: oldest entry evicted
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn pending_nonce_ttl_expires() {
        let mut store = PendingNonceStore::new(10, Duration::from_secs(0));
        store.advance([1; 20], 5);
        assert_eq!(store.get_unexpired(&[1; 20]), None);
    }
}
