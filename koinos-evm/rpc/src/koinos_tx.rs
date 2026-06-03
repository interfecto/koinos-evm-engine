//! Build, sign, and serialize Koinos transactions that relay an Ethereum raw tx
//! into our engine's `submit_raw_tx` entry point.
//!
//! Wire format (hand-rolled protobuf — see github.com/koinos/koinos-proto):
//!
//! ```text
//! message call_contract_operation {
//!   bytes contract_id = 1;     // 20-byte hash160 (b58 in JSON, raw in proto)
//!   uint32 entry_point = 2;
//!   bytes args = 3;
//! }
//!
//! message operation {
//!   oneof op {
//!     upload_contract upload_contract = 1;
//!     call_contract_operation call_contract = 2;
//!     set_system_call set_system_call = 3;
//!     set_system_contract set_system_contract = 4;
//!   }
//! }
//!
//! message transaction_header {
//!   bytes chain_id = 1;                  // multihash of chain genesis
//!   uint64 rc_limit = 2;
//!   bytes nonce = 3;                     // serialized value_type { uint64_value = 5 }
//!   bytes operation_merkle_root = 4;     // sha2-256 multihash of merkle root
//!   bytes payer = 5;                     // 20-byte hash160
//!   bytes payee = 6;                     // optional, omitted when same as payer
//! }
//!
//! message transaction {
//!   bytes id = 1;                        // sha2-256 multihash of header
//!   transaction_header header = 2;
//!   repeated operation operations = 3;
//!   repeated bytes signatures = 4;       // Koinos compact secp256k1: [31+y_parity, r(32), s(32)]
//! }
//! ```
//!
//! For ONE operation, `operation_merkle_root` = `sha2-256(serialize(operation))` (with multihash framing).

use anyhow::Result;
use secp256k1::{Message, Secp256k1, SecretKey};
use sha2::{Digest, Sha256};
use tracing::debug;

// ── Tiny protobuf encoder (just what we need) ───────────────────────────

fn encode_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn write_uint64_field(out: &mut Vec<u8>, field: u32, value: u64) {
    let tag = (field << 3) | 0;
    encode_varint(out, tag as u64);
    encode_varint(out, value);
}

fn write_bytes_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    let tag = (field << 3) | 2;
    encode_varint(out, tag as u64);
    encode_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn write_submessage_field(out: &mut Vec<u8>, field: u32, sub: &[u8]) {
    write_bytes_field(out, field, sub);
}

// ── SHA2-256 multihash ──────────────────────────────────────────────────

const MULTICODEC_SHA2_256: u8 = 0x12;

/// SHA-256 with multihash framing: [0x12, 0x20, ..32 bytes..] = 34 bytes total.
pub fn sha256_multihash(data: &[u8]) -> Vec<u8> {
    let hash = Sha256::digest(data);
    let mut out = Vec::with_capacity(34);
    out.push(MULTICODEC_SHA2_256);
    out.push(0x20);
    out.extend_from_slice(&hash);
    out
}

/// Just the 32-byte SHA-256 digest, no multihash framing.
pub fn sha256_raw(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

// ── Encoders ────────────────────────────────────────────────────────────

/// Encode a `call_contract_operation` message.
pub fn encode_call_contract_operation(
    contract_id_20: &[u8],
    entry_point: u32,
    args: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    write_bytes_field(&mut out, 1, contract_id_20);
    if entry_point != 0 {
        write_uint64_field(&mut out, 2, entry_point as u64);
    }
    if !args.is_empty() {
        write_bytes_field(&mut out, 3, args);
    }
    out
}

/// Wrap a `call_contract_operation` in an `Operation`. Field 2 of `operation` oneof.
pub fn encode_operation_call_contract(call_op_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_submessage_field(&mut out, 2, call_op_bytes);
    out
}

/// Encode the `nonce` bytes field of `transaction_header`.
/// Koinos uses a `value_type` wrapper with `uint64_value = 5`. Bytes are `0x28 || varint(n)`.
pub fn encode_nonce(n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    write_uint64_field(&mut out, 5, n);
    out
}

/// Encode a `transaction_header` message.
pub struct TransactionHeader<'a> {
    pub chain_id_multihash: &'a [u8], // 34 bytes
    pub rc_limit: u64,
    pub nonce_bytes: &'a [u8],
    pub operation_merkle_root_multihash: &'a [u8], // 34 bytes
    pub payer_20: &'a [u8],
    pub payee_20: Option<&'a [u8]>,
}

pub fn encode_transaction_header(h: &TransactionHeader<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    write_bytes_field(&mut out, 1, h.chain_id_multihash);
    if h.rc_limit != 0 {
        write_uint64_field(&mut out, 2, h.rc_limit);
    }
    write_bytes_field(&mut out, 3, h.nonce_bytes);
    write_bytes_field(&mut out, 4, h.operation_merkle_root_multihash);
    write_bytes_field(&mut out, 5, h.payer_20);
    if let Some(payee) = h.payee_20 {
        write_bytes_field(&mut out, 6, payee);
    }
    out
}

/// Encode a full `transaction` message.
pub fn encode_transaction(
    tx_id_multihash: &[u8],
    header_bytes: &[u8],
    operation_bytes: &[u8],
    signature_bytes: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    write_bytes_field(&mut out, 1, tx_id_multihash);
    write_submessage_field(&mut out, 2, header_bytes);
    write_submessage_field(&mut out, 3, operation_bytes);
    write_bytes_field(&mut out, 4, signature_bytes);
    out
}

// ── Signing ─────────────────────────────────────────────────────────────

/// Sign a 32-byte digest with secp256k1, return Koinos compact format:
/// `[byte 0 = 31 + y_parity] || [r 32 bytes] || [s 32 bytes]` = 65 bytes.
/// Always emits low-s (revm/Ethereum convention; Koinos `is_canonical` also requires low-s).
pub fn sign_digest_compact(secret: &SecretKey, digest: &[u8; 32]) -> [u8; 65] {
    let secp = Secp256k1::signing_only();
    let msg = Message::from_digest(*digest);
    let sig = secp.sign_ecdsa_recoverable(&msg, secret);
    let (rec_id, compact) = sig.serialize_compact();
    let rec_id_byte: u8 = i32::from(rec_id) as u8;
    // Note: secp256k1::sign_ecdsa_recoverable already enforces low-s normalization.
    let header = 31u8 + rec_id_byte;
    let mut out = [0u8; 65];
    out[0] = header;
    out[1..33].copy_from_slice(&compact[..32]);
    out[33..65].copy_from_slice(&compact[32..64]);
    out
}

// ── End-to-end transaction builder ──────────────────────────────────────

pub struct RelayedTx {
    /// Full encoded protobuf transaction (ready to base64-encode for chain.submit_transaction).
    pub transaction_bytes: Vec<u8>,
    /// Multihash of header (tx.id). 34 bytes.
    pub tx_id_multihash: Vec<u8>,
    /// Multihash of operation merkle root (single-op tx → sha256 of op bytes). 34 bytes.
    pub operation_merkle_root: Vec<u8>,
    /// Compact Koinos signature `[31+y_parity, r, s]`. 65 bytes.
    pub signature: Vec<u8>,
    /// The single operation's serialized bytes (for re-rendering as JSON).
    pub operation_bytes: Vec<u8>,
}

/// Build + sign a Koinos transaction that calls our engine's `submit_raw_tx` entry point
/// with the given raw Ethereum tx bytes.
///
/// `payer_20` is the operator's 20-byte hash160 address.
/// `nonce` is the OPERATOR's next-tx nonce (NOT the EVM sender's nonce).
pub fn build_relayed_tx(
    chain_id_multihash: &[u8],
    rc_limit: u64,
    payer_20: &[u8],
    nonce: u64,
    engine_contract_20: &[u8],
    submit_raw_tx_entry_point: u32,
    raw_eth_tx: &[u8],
    operator_secret: &SecretKey,
) -> Result<RelayedTx> {
    // 1. Build the submit_raw_tx args: protobuf { bytes raw_tx = 1 }
    let mut submit_args = Vec::new();
    write_bytes_field(&mut submit_args, 1, raw_eth_tx);

    // 2. Build call_contract_operation
    let call_op_bytes =
        encode_call_contract_operation(engine_contract_20, submit_raw_tx_entry_point, &submit_args);

    // 3. Wrap in Operation
    let op_bytes = encode_operation_call_contract(&call_op_bytes);

    // 4. operation_merkle_root for a single op = sha2-256 multihash of serialized op
    let op_merkle_root = sha256_multihash(&op_bytes);

    // 5. Encode the nonce as value_type { uint64_value = N }
    let nonce_bytes = encode_nonce(nonce);

    // 6. Build the header
    let header = TransactionHeader {
        chain_id_multihash,
        rc_limit,
        nonce_bytes: &nonce_bytes,
        operation_merkle_root_multihash: &op_merkle_root,
        payer_20,
        payee_20: None,
    };
    let header_bytes = encode_transaction_header(&header);

    // 7. tx_id = sha2-256 multihash of header
    let tx_id = sha256_multihash(&header_bytes);

    // 8. Sign tx_id's 32-byte digest (NOT the multihash; the underlying SHA-256 hash)
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&tx_id[2..34]);
    let sig = sign_digest_compact(operator_secret, &digest);

    // 9. Encode the full transaction
    let tx_bytes = encode_transaction(&tx_id, &header_bytes, &op_bytes, &sig);

    debug!(
        op_bytes_len = op_bytes.len(),
        header_len = header_bytes.len(),
        tx_total_len = tx_bytes.len(),
        "built relayed Koinos tx"
    );

    Ok(RelayedTx {
        transaction_bytes: tx_bytes,
        tx_id_multihash: tx_id,
        operation_merkle_root: op_merkle_root,
        signature: sig.to_vec(),
        operation_bytes: op_bytes,
    })
}
