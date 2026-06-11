//! Helpers for encoding/decoding the small protobuf messages our engine speaks.
//!
//! These mirror the wire formats in `koinos-evm/engine/src/engine.rs`:
//!   - Engine handlers take raw bytes for some entry points (get_account: 20 bytes; get_storage_at: 52 bytes)
//!   - submit_raw_tx wraps the raw_tx in protobuf `{ bytes raw_tx = 1; }`
//!   - Account result: `{ uint64 nonce=1, bytes balance=2 (32 BE), bytes code_hash=3 (32) }`
//!   - EvmResult: `{ bool success=1, bytes output=2, uint64 gas_used=3, bytes contract_address=4 }`

use anyhow::{Result, anyhow};

// ── Varint ──────────────────────────────────────────────────────────────

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            return Err(anyhow!("varint truncated"));
        }
        let b = buf[*pos];
        *pos += 1;
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(val);
        }
        shift += 7;
        if shift >= 64 {
            return Err(anyhow!("varint too long"));
        }
    }
}

fn write_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Write field N with wire type 2 (length-delimited bytes) into `out`.
pub fn write_bytes_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    let tag = (field << 3) | 2;
    write_varint(out, tag as u64);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Iterator over (field_num, wire_type, payload_slice) tuples.
pub struct ProtoIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

pub struct ProtoField<'a> {
    pub field: u32,
    pub wtype: u8,
    pub payload: &'a [u8],
    /// For varint fields, the decoded value.
    pub varint: u64,
}

impl<'a> ProtoIter<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl<'a> Iterator for ProtoIter<'a> {
    type Item = Result<ProtoField<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = match read_varint(self.buf, &mut self.pos) {
            Ok(t) => t,
            Err(e) => return Some(Err(e)),
        };
        let field = (tag >> 3) as u32;
        let wtype = (tag & 7) as u8;
        match wtype {
            0 => match read_varint(self.buf, &mut self.pos) {
                Ok(v) => Some(Ok(ProtoField {
                    field,
                    wtype,
                    payload: &[],
                    varint: v,
                })),
                Err(e) => Some(Err(e)),
            },
            2 => {
                let len = match read_varint(self.buf, &mut self.pos) {
                    Ok(l) => l as usize,
                    Err(e) => return Some(Err(e)),
                };
                if self.pos + len > self.buf.len() {
                    return Some(Err(anyhow!("length-delimited field overflows buf")));
                }
                let payload = &self.buf[self.pos..self.pos + len];
                self.pos += len;
                Some(Ok(ProtoField {
                    field,
                    wtype,
                    payload,
                    varint: 0,
                }))
            }
            _ => Some(Err(anyhow!("unsupported wire type {}", wtype))),
        }
    }
}

// ── EvmAccount decoding (response from get_account) ─────────────────────

#[derive(Debug, Default)]
pub struct EvmAccount {
    pub nonce: u64,
    pub balance: [u8; 32],
    pub code_hash: [u8; 32],
}

/// Decode the engine's `get_account` response. Returns `Ok(None)` for empty input
/// (engine returns empty bytes for non-existent accounts).
pub fn decode_account(bytes: &[u8]) -> Result<Option<EvmAccount>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let mut acc = EvmAccount::default();
    for field in ProtoIter::new(bytes) {
        let f = field?;
        match (f.field, f.wtype) {
            (1, 0) => acc.nonce = f.varint,
            (2, 2) => {
                if f.payload.len() == 32 {
                    acc.balance.copy_from_slice(f.payload);
                }
            }
            (3, 2) if f.payload.len() == 32 => {
                acc.code_hash.copy_from_slice(f.payload);
            }
            _ => {}
        }
    }
    Ok(Some(acc))
}

// ── EvmResult decoding (response from execute / call_view / submit_raw_tx) ──

#[derive(Debug, Default)]
pub struct EvmResult {
    pub success: bool,
    pub output: Vec<u8>,
    pub gas_used: u64,
    pub contract_address: Option<[u8; 20]>,
}

pub fn decode_evm_result(bytes: &[u8]) -> Result<EvmResult> {
    let mut r = EvmResult::default();
    for field in ProtoIter::new(bytes) {
        let f = field?;
        match (f.field, f.wtype) {
            (1, 0) => r.success = f.varint != 0,
            (2, 2) => r.output = f.payload.to_vec(),
            (3, 0) => r.gas_used = f.varint,
            (4, 2) if f.payload.len() == 20 => {
                let mut a = [0u8; 20];
                a.copy_from_slice(f.payload);
                r.contract_address = Some(a);
            }
            _ => {}
        }
    }
    Ok(r)
}

// ── Args builders for engine entry points ───────────────────────────────

/// `get_account` args: just the raw 20-byte address.
pub fn build_get_account_args(addr: &[u8; 20]) -> Vec<u8> {
    addr.to_vec()
}

/// `get_storage_at` args: 20-byte addr || 32-byte slot.
pub fn build_get_storage_at_args(addr: &[u8; 20], slot: &[u8; 32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(52);
    v.extend_from_slice(addr);
    v.extend_from_slice(slot);
    v
}

/// `get_code` args: just the raw 20-byte address.
pub fn build_get_code_args(addr: &[u8; 20]) -> Vec<u8> {
    addr.to_vec()
}

/// `call_view` args: protobuf with optional caller, to, value, data, gas_limit.
pub struct CallViewArgs<'a> {
    pub caller: Option<&'a [u8; 20]>,
    pub to: Option<&'a [u8; 20]>,
    pub value: Option<&'a [u8; 32]>,
    pub data: &'a [u8],
    pub gas_limit: u64,
}

pub fn build_call_view_args(args: &CallViewArgs<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(c) = args.caller {
        write_bytes_field(&mut out, 1, c);
    }
    if let Some(t) = args.to {
        write_bytes_field(&mut out, 2, t);
    }
    if let Some(v) = args.value {
        write_bytes_field(&mut out, 3, v);
    }
    if !args.data.is_empty() {
        write_bytes_field(&mut out, 4, args.data);
    }
    if args.gas_limit > 0 {
        let tag = 5u32 << 3;
        write_varint(&mut out, tag as u64);
        write_varint(&mut out, args.gas_limit);
    }
    out
}

/// `submit_raw_tx` args: protobuf with field 1 = raw_tx bytes.
pub fn build_submit_raw_tx_args(raw_tx: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_bytes_field(&mut out, 1, raw_tx);
    out
}
