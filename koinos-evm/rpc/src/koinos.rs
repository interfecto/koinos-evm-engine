//! Minimal Koinos chain RPC client (read_contract + submit_transaction).
//!
//! Talks JSON-RPC to a Koinos node. Doesn't pull in the full koinos-proto-cpp /
//! koinos-types stack — we only need a few message types (operation, transaction,
//! call_contract_operation). Signed transactions are crafted manually with
//! a hand-rolled protobuf encoder.

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};

/// Decode base64 leniently — accepts both std and URL-safe alphabets, with/without padding.
/// Koinos JSON often uses URL-safe-without-padding.
pub fn decode_b64_lax(s: &str) -> Result<Vec<u8>> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    // Try URL-safe no-pad first (Koinos default), then standard with pad
    if let Ok(v) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')) {
        return Ok(v);
    }
    if let Ok(v) = base64::engine::general_purpose::STANDARD.decode(s) {
        return Ok(v);
    }
    anyhow::bail!("invalid base64: {:?}", s)
}

/// Koinos JSON-RPC expects URL-safe base64 WITH `=` padding for `bytes` fields.
/// Std base64 chars `+` / `/` are rejected, but padding is required.
/// Empirically: URL_SAFE_NO_PAD fails when raw length isn't a multiple of 3
/// (the parser requires the encoded string length to be a multiple of 4).
pub fn encode_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE.encode(bytes)
}

pub struct KoinosClient {
    http: Client,
    rpc_url: String,
    rest_url: String,
}

impl KoinosClient {
    pub fn new(rpc_url: &str, rest_url: &str) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            rpc_url: rpc_url.to_string(),
            rest_url: rest_url.to_string(),
        })
    }

    /// Call a JSON-RPC method on the Koinos node.
    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        });
        let resp: Value = self
            .http
            .post(&self.rpc_url)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("koinos rpc error: {}", err);
        }
        resp.get("result")
            .cloned()
            .context("koinos rpc response missing 'result'")
    }

    /// GET a REST endpoint (e.g. /transaction/<id>, /block/<id>).
    pub async fn rest(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.rest_url, path);
        let resp: Value = self.http.get(&url).send().await?.json().await?;
        Ok(resp)
    }

    /// `chain.get_head_info` — returns current block height + irreversible block.
    pub async fn get_head_info(&self) -> Result<Value> {
        self.rpc("chain.get_head_info", json!({})).await
    }

    /// `chain.read_contract` — read-only contract invocation (no mana cost, no state change).
    /// `contract_id` is the **human base58check** form (e.g. "1E8igxy..."). Koinos's JSON-RPC
    /// strips the version+checksum to recover the 20-byte hash160. Sending the raw form
    /// here would look up a DIFFERENT account.
    /// `args` is URL-safe base64 WITH padding.
    pub async fn read_contract(
        &self,
        contract_id_b58: &str,
        entry_point: u32,
        args_b64: &str,
    ) -> Result<Vec<u8>> {
        let resp = self
            .rpc(
                "chain.read_contract",
                json!({
                    "contract_id": contract_id_b58,
                    "entry_point": entry_point,
                    "args": args_b64
                }),
            )
            .await?;
        // Response shape: { "result": "<base64 bytes>", "logs": [...] }
        let result_b64 = resp
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Koinos uses URL-safe base64 (no padding) in JSON
        decode_b64_lax(result_b64)
    }

    /// `chain.submit_transaction` — submit a signed Koinos transaction.
    /// `tx_object` is the full JSON object representation (per Koinos protobuf-to-JSON
    /// conventions: `bytes` fields with `(btype)` decoded to base58/hex/etc, uint64 as strings,
    /// nested messages as JSON objects).
    pub async fn submit_transaction(&self, tx_object: Value, broadcast: bool) -> Result<Value> {
        self.rpc(
            "chain.submit_transaction",
            json!({ "transaction": tx_object, "broadcast": broadcast }),
        )
        .await
    }

    /// `chain.get_account_nonce` — get the current nonce for an account.
    pub async fn get_account_nonce(&self, account_b64: &str) -> Result<Value> {
        self.rpc(
            "chain.get_account_nonce",
            json!({ "account": account_b64 }),
        )
        .await
    }

    /// `chain.get_account_rc` — get the account's mana balance.
    pub async fn get_account_rc(&self, account_b64: &str) -> Result<Value> {
        self.rpc(
            "chain.get_account_rc",
            json!({ "account": account_b64 }),
        )
        .await
    }
}
