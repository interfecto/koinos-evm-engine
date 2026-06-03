//! Ethereum raw-transaction parsing and signing-hash recovery for `submit_raw_tx`.
//!
//! Supports:
//!   - Legacy (pre-EIP-155): `rlp([n, gp, gl, to, v, d, V, R, S])`
//!   - EIP-155 (legacy w/ chain_id): same envelope, `V = chain_id*2 + 35 + y_parity`
//!   - EIP-1559 (type 0x02): `0x02 || rlp([cid, n, mpfg, mfg, gl, to, v, d, al, y, r, s])`
//!
//! Explicitly rejects:
//!   - EIP-2930 (type 0x01): access lists deferred
//!   - EIP-4844 (type 0x03): blob carriers (not relevant for L1-equivalent execution)
//!   - EIP-7702 (type 0x04): auth lists deferred
//!
//! Signing-hash strategy: re-encode the unsigned-payload bytes by slicing the original
//! item bytes from `PayloadView::List` and wrapping with a fresh RLP list header. Hash
//! via native Koinos `sys::hash(HASH_KECCAK_256)` rather than a pure-Rust implementation.

use alloc::vec::Vec;
use alloy_rlp::{Encodable, Header, PayloadView};
use revm::primitives::{Address, B256, U256};

use crate::koinos::{self, sys};

#[derive(Debug)]
pub enum TxParseError {
    Empty,
    RlpDecode,
    UnsupportedType(u8),
    WrongFieldCount,
    InvalidSignature,
    InvalidChainId,
    AddressLength,
    ScalarTooLarge,
    NonCanonicalInteger,
    TrailingBytes,
    Pre155Rejected,
    AccessListUnsupported,
    InvalidFeeOrdering,
    HashFailure,
    RecoveryFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxKindParsed {
    /// Pre-EIP-155 legacy. `V` is 27 or 28; no chain_id replay protection.
    LegacyPre155,
    /// EIP-155 legacy. `V = chain_id*2 + 35 + y_parity`.
    Eip155,
    /// EIP-1559 typed envelope (0x02).
    Eip1559,
}

pub struct ParsedTx {
    pub kind: TxKindParsed,
    pub chain_id: Option<u64>, // None for pre-155
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,           // gas_price for legacy
    pub max_priority_fee_per_gas: u128,  // = max_fee for legacy
    pub to: Option<Address>,             // None = CREATE
    pub value: U256,
    pub data: Vec<u8>,

    // Signature
    pub r: U256,
    pub s: U256,
    pub y_parity: u8, // 0 or 1

    // Recovered sender (set by `recover_sender`)
    pub sender: Address,
}

pub fn parse_raw_tx(raw: &[u8]) -> Result<ParsedTx, TxParseError> {
    if raw.is_empty() {
        return Err(TxParseError::Empty);
    }

    let first = raw[0];
    if first >= 0xc0 {
        parse_legacy(raw)
    } else if first == 0x02 {
        parse_eip1559(&raw[1..])
    } else if first == 0x01 || first == 0x03 || first == 0x04 {
        Err(TxParseError::UnsupportedType(first))
    } else {
        Err(TxParseError::UnsupportedType(first))
    }
}

// ── Legacy (pre-155 and EIP-155) ────────────────────────────────────────

fn parse_legacy(raw: &[u8]) -> Result<ParsedTx, TxParseError> {
    let mut buf = raw;
    let view = Header::decode_raw(&mut buf).map_err(|_| TxParseError::RlpDecode)?;
    // Reject trailing bytes after the RLP list — `valid_tx || garbage` must not parse
    if !buf.is_empty() {
        return Err(TxParseError::TrailingBytes);
    }
    let items = match view {
        PayloadView::List(items) => items,
        _ => return Err(TxParseError::RlpDecode),
    };
    if items.len() != 9 {
        return Err(TxParseError::WrongFieldCount);
    }

    let nonce = decode_u64(items[0])?;
    let gas_price = decode_u128(items[1])?;
    let gas_limit = decode_u64(items[2])?;
    let to = decode_to(items[3])?;
    let value = decode_u256(items[4])?;
    let data = decode_bytes(items[5])?.to_vec();
    let v_raw = decode_u64(items[6])?;
    let r = decode_u256(items[7])?;
    let s = decode_u256(items[8])?;

    // Classify v and derive (kind, chain_id, y_parity).
    // Pre-EIP-155 (v == 27/28) is REJECTED on a chain with chain_id != 0:
    // those signatures have no replay protection and could be lifted from mainnet/other chains.
    let (kind, chain_id, y_parity) = if v_raw == 27 || v_raw == 28 {
        return Err(TxParseError::Pre155Rejected);
    } else if v_raw >= 35 {
        // v = chain_id*2 + 35 + y_parity
        let chain_id = (v_raw - 35) / 2;
        let y_parity = ((v_raw - 35) % 2) as u8;
        (TxKindParsed::Eip155, Some(chain_id), y_parity)
    } else {
        return Err(TxParseError::InvalidSignature);
    };

    validate_signature(&r, &s, y_parity)?;

    // Build the unsigned-payload bytes
    let signing_hash = match kind {
        TxKindParsed::Eip155 => {
            let cid = chain_id.unwrap();
            // Append rlp(chain_id) || rlp(0) || rlp(0) to the first 6 items
            let mut extra: Vec<u8> = Vec::with_capacity(16);
            encode_u64_into(&mut extra, cid);
            encode_u64_into(&mut extra, 0);
            encode_u64_into(&mut extra, 0);
            keccak256_of_rlp_list_with_tail(&items[0..6], &extra)?
        }
        TxKindParsed::LegacyPre155 | TxKindParsed::Eip1559 => unreachable!(),
    };

    let sender = recover_sender(&signing_hash, &r, &s, y_parity)?;

    Ok(ParsedTx {
        kind,
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas: gas_price,
        max_priority_fee_per_gas: gas_price,
        to,
        value,
        data,
        r,
        s,
        y_parity,
        sender,
    })
}

// ── EIP-1559 ────────────────────────────────────────────────────────────

fn parse_eip1559(payload: &[u8]) -> Result<ParsedTx, TxParseError> {
    let mut buf = payload;
    let view = Header::decode_raw(&mut buf).map_err(|_| TxParseError::RlpDecode)?;
    if !buf.is_empty() {
        return Err(TxParseError::TrailingBytes);
    }
    let items = match view {
        PayloadView::List(items) => items,
        _ => return Err(TxParseError::RlpDecode),
    };
    if items.len() != 12 {
        return Err(TxParseError::WrongFieldCount);
    }

    let chain_id = decode_u64(items[0])?;
    let nonce = decode_u64(items[1])?;
    let max_priority = decode_u128(items[2])?;
    let max_fee = decode_u128(items[3])?;
    if max_priority > max_fee {
        return Err(TxParseError::InvalidFeeOrdering);
    }
    let gas_limit = decode_u64(items[4])?;
    let to = decode_to(items[5])?;
    let value = decode_u256(items[6])?;
    let data = decode_bytes(items[7])?.to_vec();
    // items[8] = access_list. We don't propagate AL entries to TxEnv yet
    // (would change intrinsic gas + warm-storage behavior) — reject non-empty for now.
    {
        let mut al_buf = items[8];
        let al_view = Header::decode_raw(&mut al_buf).map_err(|_| TxParseError::RlpDecode)?;
        match al_view {
            PayloadView::List(list) if list.is_empty() => {}
            _ => return Err(TxParseError::AccessListUnsupported),
        }
    }
    let y_parity_raw = decode_u64(items[9])?;
    let r = decode_u256(items[10])?;
    let s = decode_u256(items[11])?;

    if y_parity_raw > 1 {
        return Err(TxParseError::InvalidSignature);
    }
    let y_parity = y_parity_raw as u8;
    validate_signature(&r, &s, y_parity)?;

    // Signing hash: keccak256(0x02 || rlp([items[0..9]]))
    let unsigned_rlp = build_rlp_list(&items[0..9]);
    let mut prehash: Vec<u8> = Vec::with_capacity(1 + unsigned_rlp.len());
    prehash.push(0x02);
    prehash.extend_from_slice(&unsigned_rlp);
    let signing_hash = keccak256(&prehash)?;

    let sender = recover_sender(&signing_hash, &r, &s, y_parity)?;

    Ok(ParsedTx {
        kind: TxKindParsed::Eip1559,
        chain_id: Some(chain_id),
        nonce,
        gas_limit,
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: max_priority,
        to,
        value,
        data,
        r,
        s,
        y_parity,
        sender,
    })
}

// ── RLP helpers ─────────────────────────────────────────────────────────

/// Per Ethereum spec, RLP-encoded scalars must NOT have leading zeros and
/// zero must be encoded as the empty string. `[0x00]` and `[0x00, 0x05]` are
/// non-canonical and rejected.
fn check_canonical(bytes: &[u8]) -> Result<(), TxParseError> {
    if !bytes.is_empty() && bytes[0] == 0 {
        return Err(TxParseError::NonCanonicalInteger);
    }
    Ok(())
}

fn decode_u64(item: &[u8]) -> Result<u64, TxParseError> {
    let bytes = decode_bytes(item)?;
    check_canonical(bytes)?;
    if bytes.len() > 8 {
        return Err(TxParseError::ScalarTooLarge);
    }
    let mut out = 0u64;
    for &b in bytes {
        out = (out << 8) | b as u64;
    }
    Ok(out)
}

fn decode_u128(item: &[u8]) -> Result<u128, TxParseError> {
    let bytes = decode_bytes(item)?;
    check_canonical(bytes)?;
    if bytes.len() > 16 {
        return Err(TxParseError::ScalarTooLarge);
    }
    let mut out = 0u128;
    for &b in bytes {
        out = (out << 8) | b as u128;
    }
    Ok(out)
}

fn decode_u256(item: &[u8]) -> Result<U256, TxParseError> {
    let bytes = decode_bytes(item)?;
    check_canonical(bytes)?;
    if bytes.len() > 32 {
        return Err(TxParseError::ScalarTooLarge);
    }
    Ok(U256::from_be_slice(bytes))
}

fn decode_bytes(item: &[u8]) -> Result<&[u8], TxParseError> {
    let mut buf = item;
    Header::decode_bytes(&mut buf, false).map_err(|_| TxParseError::RlpDecode)
}

fn decode_to(item: &[u8]) -> Result<Option<Address>, TxParseError> {
    let bytes = decode_bytes(item)?;
    if bytes.is_empty() {
        Ok(None)
    } else if bytes.len() == 20 {
        Ok(Some(Address::from_slice(bytes)))
    } else {
        Err(TxParseError::AddressLength)
    }
}

/// Build a fresh RLP list out of pre-encoded item slices (concatenate + wrap header).
fn build_rlp_list(items: &[&[u8]]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(|i| i.len()).sum();
    let mut out = Vec::with_capacity(payload_len + 9);
    let header = Header { list: true, payload_length: payload_len };
    header.encode(&mut out);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn build_rlp_list_with_tail(items: &[&[u8]], tail: &[u8]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(|i| i.len()).sum::<usize>() + tail.len();
    let mut out = Vec::with_capacity(payload_len + 9);
    let header = Header { list: true, payload_length: payload_len };
    header.encode(&mut out);
    for item in items {
        out.extend_from_slice(item);
    }
    out.extend_from_slice(tail);
    out
}

fn encode_u64_into(out: &mut Vec<u8>, v: u64) {
    v.encode(out);
}

// ── Crypto via Koinos syscalls ──────────────────────────────────────────

fn keccak256(data: &[u8]) -> Result<B256, TxParseError> {
    let multihash = sys::hash(koinos::HASH_KECCAK_256, data);
    if multihash.len() < 34 {
        return Err(TxParseError::HashFailure);
    }
    // multihash = [0x1b, 0x20, ..32 bytes..]
    Ok(B256::from_slice(&multihash[2..34]))
}

fn keccak256_of_rlp_list(items: &[&[u8]]) -> Result<B256, TxParseError> {
    let rlp = build_rlp_list(items);
    keccak256(&rlp)
}

fn keccak256_of_rlp_list_with_tail(items: &[&[u8]], tail: &[u8]) -> Result<B256, TxParseError> {
    let rlp = build_rlp_list_with_tail(items, tail);
    keccak256(&rlp)
}

// ── Signature validation ────────────────────────────────────────────────

/// secp256k1 group order N. Used for `r < N` and `s < N` validation.
const SECP256K1_N: U256 = U256::from_be_bytes([
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE,
    0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36, 0x41, 0x41,
]);

/// Half the secp256k1 group order, used for EIP-2 low-s check.
/// n/2 = 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0
const SECP256K1_N_HALF: U256 = U256::from_be_bytes([
    0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B, 0x20, 0xA0,
]);

fn validate_signature(r: &U256, s: &U256, y_parity: u8) -> Result<(), TxParseError> {
    if r.is_zero() || s.is_zero() {
        return Err(TxParseError::InvalidSignature);
    }
    if *r >= SECP256K1_N || *s >= SECP256K1_N {
        // r and s must be in the secp256k1 group order range
        return Err(TxParseError::InvalidSignature);
    }
    if *s > SECP256K1_N_HALF {
        // EIP-2: reject high-s signatures (malleability defense, txs only —
        // the ecrecover precompile still accepts high-s)
        return Err(TxParseError::InvalidSignature);
    }
    if y_parity > 1 {
        return Err(TxParseError::InvalidSignature);
    }
    Ok(())
}

// ── Sender recovery ─────────────────────────────────────────────────────

fn recover_sender(
    msg_hash: &B256,
    r: &U256,
    s: &U256,
    y_parity: u8,
) -> Result<Address, TxParseError> {
    // Koinos `recoverable_signature` (Bitcoin compact, koinos-crypto-cpp): 65 bytes
    //   byte 0      = header = 31 + y_parity  (range [31,33], we use 31|32)
    //   bytes 1..33 = r
    //   bytes 33..65 = s
    let mut signature = Vec::with_capacity(65);
    signature.push(31u8 + y_parity);
    signature.extend_from_slice(&r.to_be_bytes::<32>());
    signature.extend_from_slice(&s.to_be_bytes::<32>());

    // Digest as multihash: keccak-256 prefix (0x1b, 0x20) || hash[32]
    let mut digest = Vec::with_capacity(34);
    digest.push(0x1b);
    digest.push(0x20);
    digest.extend_from_slice(msg_hash.as_slice());

    let pubkey = sys::recover_public_key(
        koinos::DSA_ECDSA_SECP256K1,
        &signature,
        &digest,
        false, // uncompressed: 65 bytes [0x04 || X(32) || Y(32)]
    );

    if pubkey.len() < 64 {
        return Err(TxParseError::RecoveryFailure);
    }
    let pubkey_xy = if pubkey[0] == 0x04 && pubkey.len() == 65 {
        &pubkey[1..]
    } else if pubkey.len() == 64 {
        &pubkey[..]
    } else {
        return Err(TxParseError::RecoveryFailure);
    };

    // EVM address = keccak256(pubkey_xy)[12..32]
    let multihash = sys::hash(koinos::HASH_KECCAK_256, pubkey_xy);
    if multihash.len() < 34 {
        return Err(TxParseError::HashFailure);
    }
    Ok(Address::from_slice(&multihash[2 + 12..2 + 32]))
}
