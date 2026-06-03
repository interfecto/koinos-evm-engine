//! Shared application state + configuration.

use crate::koinos::KoinosClient;
use anyhow::Context;
use base64::Engine;
use secp256k1::SecretKey;
use std::str::FromStr;
use std::sync::RwLock;
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Configuration sourced from environment variables.
pub struct Config {
    pub listen_addr: String,
    pub koinos_rpc_url: String,
    pub koinos_rest_url: String,
    /// Human base58check form (e.g. "1E8igxy..."). Used by chain.read_contract / get_account_nonce
    /// AND chain.submit_transaction (in the JSON `payer` / `contract_id` fields).
    pub engine_contract_addr_b58check: String,
    /// Raw base58 form of the 20-byte hash160 (e.g. "31QYFBy..."). Currently unused but kept for debug.
    pub engine_contract_addr_b58: String,
    /// Raw 20-byte hash160 of the engine contract address.
    pub engine_contract_id: Vec<u8>,
    /// Full 25-byte payload (version + hash160 + checksum) — what Koinos's JSON deserializer
    /// produces for an ADDRESS/CONTRACT_ID field. Our hand-encoded protobuf headers must use
    /// these bytes so that `sha256(header)` matches Koinos's internal computation.
    pub engine_contract_addr_full: Vec<u8>,
    /// Koinos chain ID (base64-encoded multihash), needed for txn header.chain_id
    pub koinos_chain_id_b64: String,
    /// Raw bytes of the chain ID multihash (decoded from `koinos_chain_id_b64`).
    pub koinos_chain_id_bytes: Vec<u8>,
    /// EVM chain ID (uint), matches `ENGINE_CHAIN_ID` in the engine.
    pub evm_chain_id: u64,
    /// Proxy operator's secp256k1 private key — pays Koinos mana for relayed txs.
    pub operator_key: SecretKey,
    /// Proxy operator's Koinos address (raw base58 — currently unused).
    pub operator_addr_b58: String,
    /// Proxy operator's human-readable base58check form (used everywhere in JSON).
    pub operator_addr_b58check: String,
    /// 20-byte hash160 of operator's pubkey.
    pub operator_addr_bytes: Vec<u8>,
    /// Full 25-byte payload (version + hash160 + checksum) — used in hand-encoded proto headers.
    pub operator_addr_full: Vec<u8>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        // Default bind 127.0.0.1: prevents accidentally exposing the proxy + operator key
        // to the LAN. Operators who want public access can set LISTEN_ADDR=0.0.0.0:8545 explicitly.
        let listen_addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8545".into());
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
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&koinos_chain_id_b64))
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&koinos_chain_id_b64))
            .context("KOINOS_CHAIN_ID must be valid base64 of the chain_id multihash")?;

        Ok(Self {
            listen_addr,
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
    /// Raw signed bytes (returned in eth_getTransactionByHash if requested).
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

pub struct AppState {
    pub config: Config,
    pub client: KoinosClient,
    /// In-memory map from Ethereum tx hash → metadata.
    /// Populated when we relay a tx via eth_sendRawTransaction.
    /// Used by eth_getTransactionReceipt / eth_getTransactionByHash.
    pub tx_meta: RwLock<HashMap<[u8; 32], TxMeta>>,
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
    /// in handle_get_tx_count once the chain catches up. CAVEAT: a relay that the engine
    /// accepts at the Koinos layer but that never advances the EVM on-chain nonce (e.g. a
    /// manually-submitted future-nonce tx) leaves a too-high entry that never self-heals;
    /// a TTL / receipt-driven reset is a Phase-H hardening item before public exposure.
    /// Fine for the sequential-send (MetaMask) flow this targets.
    pub pending_nonce: RwLock<HashMap<[u8; 20], u64>>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let client = KoinosClient::new(&config.koinos_rpc_url, &config.koinos_rest_url)?;
        Ok(Self {
            config,
            client,
            tx_meta: RwLock::new(HashMap::new()),
            operator_nonce: Mutex::new(None),
            pending_nonce: RwLock::new(HashMap::new()),
        })
    }
}

// ── Koinos address helpers ───────────────────────────────────────────────

fn koinos_address_from_pubkey_compressed(pubkey: &[u8]) -> Vec<u8> {
    // Koinos address = RIPEMD-160(SHA-256(pubkey_compressed))
    use sha2::{Digest, Sha256};
    use ripemd::Ripemd160;
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

const BS58_ALPHABET: &[u8; 58] =
    b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn bs58(bytes: &[u8]) -> String {
    use std::iter::repeat;
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
    out.extend(repeat(b'1').take(zeros));
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
        for j in 0..n_len {
            let cur = (n[j] as u32) * 58 + carry;
            n[j] = (cur & 0xff) as u8;
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
