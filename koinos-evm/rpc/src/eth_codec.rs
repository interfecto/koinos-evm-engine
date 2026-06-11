//! Ethereum-style hex codec helpers.
//!
//! Conventions:
//!   - "Quantity" values (block numbers, balances, gas) → `"0x<hex>"` with no leading zeros (except `"0x0"`)
//!   - "Data" values (addresses, hashes, bytecode, calldata) → `"0x<hex>"` with even-length, lowercase

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

/// Parse `"0x<hex>"` quantity → u64. Accepts `"0x0"`, `"0x1a"`, etc.
pub fn parse_quantity(s: &str) -> Result<u64> {
    let s = s
        .strip_prefix("0x")
        .context("expected 0x-prefixed quantity")?;
    if s.is_empty() {
        return Err(anyhow!("empty quantity"));
    }
    u64::from_str_radix(s, 16).map_err(Into::into)
}

/// Parse `"0x<hex>"` quantity → 32-byte big-endian U256. Supports values up to 256 bits.
pub fn parse_u256_be(s: &str) -> Result<[u8; 32]> {
    let s = s
        .strip_prefix("0x")
        .context("expected 0x-prefixed quantity")?;
    if s.is_empty() {
        return Err(anyhow!("empty quantity"));
    }
    if s.len() > 64 {
        return Err(anyhow!("quantity exceeds 256 bits"));
    }
    let mut padded = String::with_capacity(64);
    for _ in 0..(64 - s.len()) {
        padded.push('0');
    }
    padded.push_str(s);
    let bytes = hex::decode(&padded)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Parse `"0x<hex>"` data → bytes.
pub fn parse_data(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").context("expected 0x-prefixed data")?;
    if s.len() % 2 != 0 {
        return Err(anyhow!("hex data length must be even"));
    }
    hex::decode(s).map_err(Into::into)
}

/// Parse a 20-byte address `"0x<40 hex>"` → 20 bytes.
pub fn parse_address(s: &str) -> Result<[u8; 20]> {
    let v = parse_data(s)?;
    if v.len() != 20 {
        return Err(anyhow!("address must be 20 bytes, got {}", v.len()));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Parse a 32-byte hash/topic/slot.
pub fn parse_b32(s: &str) -> Result<[u8; 32]> {
    let v = parse_data(s)?;
    if v.len() != 32 {
        return Err(anyhow!("expected 32 bytes, got {}", v.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Format u64 as `"0x<hex>"` quantity (no leading zeros, lowercase).
pub fn quantity(n: u64) -> Value {
    Value::String(format!("0x{:x}", n))
}

/// Format u128 as `"0x<hex>"` quantity (no leading zeros, lowercase).
pub fn quantity_u128(n: u128) -> Value {
    Value::String(format!("0x{:x}", n))
}

/// Format u256 (32 bytes) as `"0x<hex>"` quantity. Strips leading zeros except keep "0x0".
pub fn quantity_from_be32(bytes: &[u8; 32]) -> Value {
    let hex = hex::encode(bytes);
    let trimmed = hex.trim_start_matches('0');
    if trimmed.is_empty() {
        Value::String("0x0".into())
    } else {
        Value::String(format!("0x{}", trimmed))
    }
}

/// Format bytes as `"0x<hex>"` data.
pub fn data(bytes: &[u8]) -> Value {
    Value::String(format!("0x{}", hex::encode(bytes)))
}

/// Is a 32-byte BE quantity >= a u128 floor? (Anything with a set bit in the top
/// 16 bytes exceeds any u128.)
pub fn u256_be_ge_u128(v: &[u8; 32], floor: u128) -> bool {
    if v[..16].iter().any(|&b| b != 0) {
        return true;
    }
    u128::from_be_bytes(v[16..].try_into().expect("16-byte slice")) >= floor
}

/// Extract param at index — returns helpful error.
pub fn param(params: &Value, idx: usize) -> Result<&Value> {
    match params {
        Value::Array(arr) => arr
            .get(idx)
            .ok_or_else(|| anyhow!("missing param at index {}", idx)),
        _ => Err(anyhow!("params must be an array")),
    }
}

pub fn param_str(params: &Value, idx: usize) -> Result<&str> {
    param(params, idx)?
        .as_str()
        .ok_or_else(|| anyhow!("param {} must be a string", idx))
}
