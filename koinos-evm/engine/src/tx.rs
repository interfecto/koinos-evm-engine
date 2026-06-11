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

#[cfg(not(feature = "host-crypto"))]
use crate::koinos::{self, sys};

#[derive(Debug)]
pub enum TxParseError {
    Empty,
    RlpDecode,
    // Payload (the tx type byte) is only read via the derived `Debug` impl
    // (engine.rs formats parse errors with `{:?}`), which dead-code analysis ignores.
    #[allow(dead_code)]
    UnsupportedType(u8),
    WrongFieldCount,
    InvalidSignature,
    // Never constructed here: the chain-id check lives in engine.rs (reject_submit).
    // Kept as API surface for moving that check into the parser.
    #[allow(dead_code)]
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
    /// Never constructed — `parse_legacy` rejects pre-155 outright
    /// (`Pre155Rejected`); the variant documents the classification.
    #[allow(dead_code)]
    LegacyPre155,
    /// EIP-155 legacy. `V = chain_id*2 + 35 + y_parity`.
    Eip155,
    /// EIP-1559 typed envelope (0x02).
    Eip1559,
}

// Fields marked `allow(dead_code)` are parsed and validated but not yet consumed
// by the engine (zero-fee policy ignores fee fields; r/s/y_parity are spent during
// recovery). Kept to document the wire format.
pub struct ParsedTx {
    #[allow(dead_code)]
    pub kind: TxKindParsed,
    pub chain_id: Option<u64>, // None for pre-155
    pub nonce: u64,
    pub gas_limit: u64,
    #[allow(dead_code)]
    pub max_fee_per_gas: u128, // gas_price for legacy
    #[allow(dead_code)]
    pub max_priority_fee_per_gas: u128, // = max_fee for legacy
    pub to: Option<Address>, // None = CREATE
    pub value: U256,
    pub data: Vec<u8>,

    // Signature
    #[allow(dead_code)]
    pub r: U256,
    #[allow(dead_code)]
    pub s: U256,
    #[allow(dead_code)]
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
    } else {
        // Known typed envelopes 0x01 (EIP-2930), 0x03 (EIP-4844) and 0x04 (EIP-7702)
        // are deliberately unsupported (see module docs); anything else is unknown.
        // Both reject identically.
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
    let header = Header {
        list: true,
        payload_length: payload_len,
    };
    header.encode(&mut out);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn build_rlp_list_with_tail(items: &[&[u8]], tail: &[u8]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(|i| i.len()).sum::<usize>() + tail.len();
    let mut out = Vec::with_capacity(payload_len + 9);
    let header = Header {
        list: true,
        payload_length: payload_len,
    };
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
//
// CRYPTO SEAM: `keccak256` and `recover_sender` are the only two functions
// that touch the chain host. The `host-crypto` feature swaps both for pure-Rust
// implementations (tiny-keccak / k256) with identical signatures and error
// mapping so the parser is unit-testable on a native target. The default
// (syscall) bodies are untouched — production WASM codegen is identical.

#[cfg(not(feature = "host-crypto"))]
fn keccak256(data: &[u8]) -> Result<B256, TxParseError> {
    let multihash = sys::hash(koinos::HASH_KECCAK_256, data);
    if multihash.len() < 34 {
        return Err(TxParseError::HashFailure);
    }
    // multihash = [0x1b, 0x20, ..32 bytes..]
    Ok(B256::from_slice(&multihash[2..34]))
}

/// Host-test twin of the syscall `keccak256`. Infallible (tiny-keccak cannot
/// fail), so `HashFailure` is unreachable under `host-crypto`.
#[cfg(feature = "host-crypto")]
fn keccak256(data: &[u8]) -> Result<B256, TxParseError> {
    use tiny_keccak::{Hasher, Keccak};
    let mut hasher = Keccak::v256();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    Ok(B256::from(out))
}

// Unused since EIP-155 hashing switched to the `_with_tail` variant; kept as the
// natural API pairing (a future pre-155 or typed-tx path would use it).
#[allow(dead_code)]
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

#[cfg(not(feature = "host-crypto"))]
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

/// Host-test twin of the syscall `recover_sender`, mirroring its semantics:
/// recovery id = `y_parity` (the syscall header `31 + y_parity` encodes the
/// same 0/1 recid; ids 2/3 — the `x = r + n` cases — are never produced),
/// uncompressed pubkey, address = keccak256(X || Y)[12..32]. Any recovery
/// failure (e.g. `r` not the x-coordinate of a curve point) maps to
/// `RecoveryFailure`, matching the syscall path's empty-result handling.
/// Callers have already enforced r/s range and low-s via `validate_signature`.
#[cfg(feature = "host-crypto")]
fn recover_sender(
    msg_hash: &B256,
    r: &U256,
    s: &U256,
    y_parity: u8,
) -> Result<Address, TxParseError> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(&r.to_be_bytes::<32>());
    sig_bytes[32..].copy_from_slice(&s.to_be_bytes::<32>());
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| TxParseError::RecoveryFailure)?;
    let recovery_id = RecoveryId::from_byte(y_parity).ok_or(TxParseError::RecoveryFailure)?;
    let verifying_key =
        VerifyingKey::recover_from_prehash(msg_hash.as_slice(), &signature, recovery_id)
            .map_err(|_| TxParseError::RecoveryFailure)?;

    // Uncompressed SEC1 point: [0x04 || X(32) || Y(32)] — drop the tag byte.
    let point = verifying_key.to_encoded_point(false);
    let pubkey_xy = &point.as_bytes()[1..];

    // EVM address = keccak256(pubkey_xy)[12..32]
    let hash = keccak256(pubkey_xy)?;
    Ok(Address::from_slice(&hash.as_slice()[12..32]))
}

// ── Host unit tests (WS-A1) ─────────────────────────────────────────────
//
// Run with:
//   cargo test --target aarch64-apple-darwin --features host-crypto
// (the explicit host target overrides the wasm32v1-none default from
// .cargo/config.toml; `host-crypto` swaps the syscall crypto for tiny-keccak
// + k256 so the parser runs without a chain host).

#[cfg(all(test, feature = "host-crypto"))]
mod tests {
    use super::*;
    use alloc::vec;

    // ── Golden vectors ───────────────────────────────────────────────

    /// EIP-155 spec example transaction (chain id 1), from the EIP text.
    /// nonce 9, gasprice 20 gwei, gas 21000, to 0x3535…35, value 1 ether,
    /// empty data, v = 37 (chain 1, y_parity 0).
    const EIP155_SPEC_RAW: &str = "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83";
    const EIP155_SPEC_SENDER: &str = "9d8a62f656a8d1615c1294fd71e9cfb3e4855a4f";

    /// Generated offline with foundry `cast` (throwaway key, never funded):
    ///   cast wallet new
    ///     → address     0x9d9a9C891Cb86136A56394d215ec2De0eC44F252
    ///     → private key 0x87a162d9fd3b0e6730a713937393b2f3e6299d14a15402679a642463b4680aaa
    ///   cast mktx --private-key <pk> --chain 42069 --nonce 0 --gas-limit 21000 \
    ///     --gas-price 1 --value 1 --legacy 0x1111111111111111111111111111111111111111
    /// v = 0x148ce = 84174 = 42069*2 + 35 + 1 → chain 42069, y_parity 1.
    const CAST_LEGACY_RAW: &str = "f86280018252089411111111111111111111111111111111111111110180830148cea0e93452df983161572885d66e3496133be9eb8752254ba9ed2703cadd98b7fcbaa07c431cb342891e0fdc832c4d31a5585d4a5181a92b136034cedebbb047731cc6";

    /// Same key, EIP-1559 with empty access list:
    ///   cast mktx --private-key <pk> --chain 42069 --nonce 1 --gas-limit 21000 \
    ///     --gas-price 2 --priority-gas-price 1 --value 1 \
    ///     0x1111111111111111111111111111111111111111
    const CAST_1559_RAW: &str = "02f86482a4550101028252089411111111111111111111111111111111111111110180c001a085241832afad022300a084ba17874a17f0557b94a5cede609763b396d7fd0390a040c223c96fca2e5b28d0cdcba2fe69cd0b3e817c5e61433e9c9aaf18834d1394";

    const CAST_SENDER: &str = "9d9a9c891cb86136a56394d215ec2de0ec44f252";
    const CAST_TO: &str = "1111111111111111111111111111111111111111";

    // ── Helpers (decode-side only, no crypto) ────────────────────────

    fn hex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2), "odd hex length");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn u256(s: &str) -> U256 {
        U256::from_be_slice(&hex(s))
    }

    fn addr(s: &str) -> Address {
        Address::from_slice(&hex(s))
    }

    /// Split an RLP list payload into its (still RLP-encoded) item slices.
    fn list_items(mut payload: &[u8]) -> Vec<Vec<u8>> {
        match Header::decode_raw(&mut payload).expect("valid rlp") {
            PayloadView::List(items) => items.iter().map(|i| i.to_vec()).collect(),
            PayloadView::String(_) => panic!("expected list"),
        }
    }

    /// RLP-encode a byte string item.
    fn rlp_str(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Encodable::encode(data, &mut out);
        out
    }

    /// Rebuild a legacy raw tx with item `idx` replaced by pre-encoded `new_item`.
    fn mutate_legacy(raw: &[u8], idx: usize, new_item: &[u8]) -> Vec<u8> {
        let mut items = list_items(raw);
        items[idx] = new_item.to_vec();
        let refs: Vec<&[u8]> = items.iter().map(|v| v.as_slice()).collect();
        build_rlp_list(&refs)
    }

    /// Rebuild an EIP-1559 raw tx (0x02-prefixed) with item `idx` replaced.
    fn mutate_1559(raw: &[u8], idx: usize, new_item: &[u8]) -> Vec<u8> {
        assert_eq!(raw[0], 0x02);
        let mut items = list_items(&raw[1..]);
        items[idx] = new_item.to_vec();
        let refs: Vec<&[u8]> = items.iter().map(|v| v.as_slice()).collect();
        let mut out = vec![0x02];
        out.extend_from_slice(&build_rlp_list(&refs));
        out
    }

    fn assert_same(a: &ParsedTx, b: &ParsedTx) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.chain_id, b.chain_id);
        assert_eq!(a.nonce, b.nonce);
        assert_eq!(a.gas_limit, b.gas_limit);
        assert_eq!(a.max_fee_per_gas, b.max_fee_per_gas);
        assert_eq!(a.max_priority_fee_per_gas, b.max_priority_fee_per_gas);
        assert_eq!(a.to, b.to);
        assert_eq!(a.value, b.value);
        assert_eq!(a.data, b.data);
        assert_eq!(a.r, b.r);
        assert_eq!(a.s, b.s);
        assert_eq!(a.y_parity, b.y_parity);
        assert_eq!(a.sender, b.sender);
    }

    // Legacy item indices: 0 nonce, 1 gas_price, 2 gas_limit, 3 to, 4 value,
    //                      5 data, 6 v, 7 r, 8 s
    // 1559 item indices:   0 chain_id, 1 nonce, 2 max_priority, 3 max_fee,
    //                      4 gas_limit, 5 to, 6 value, 7 data, 8 access_list,
    //                      9 y_parity, 10 r, 11 s

    // ── Golden positive vectors ──────────────────────────────────────

    #[test]
    fn golden_eip155_spec_vector() {
        let parsed = parse_raw_tx(&hex(EIP155_SPEC_RAW)).expect("spec vector must parse");
        assert_eq!(parsed.kind, TxKindParsed::Eip155);
        assert_eq!(parsed.chain_id, Some(1));
        assert_eq!(parsed.nonce, 9);
        assert_eq!(parsed.gas_limit, 21000);
        assert_eq!(parsed.max_fee_per_gas, 20_000_000_000);
        assert_eq!(parsed.max_priority_fee_per_gas, 20_000_000_000);
        assert_eq!(
            parsed.to,
            Some(addr("3535353535353535353535353535353535353535"))
        );
        assert_eq!(parsed.value, U256::from(1_000_000_000_000_000_000u64));
        assert!(parsed.data.is_empty());
        assert_eq!(
            parsed.r,
            u256("28ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276")
        );
        assert_eq!(
            parsed.s,
            u256("67cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83")
        );
        assert_eq!(parsed.y_parity, 0);
        assert_eq!(parsed.sender, addr(EIP155_SPEC_SENDER));
    }

    #[test]
    fn golden_cast_legacy_eip155_chain_42069() {
        let parsed = parse_raw_tx(&hex(CAST_LEGACY_RAW)).expect("cast legacy vector must parse");
        assert_eq!(parsed.kind, TxKindParsed::Eip155);
        assert_eq!(parsed.chain_id, Some(42069));
        assert_eq!(parsed.nonce, 0);
        assert_eq!(parsed.gas_limit, 21000);
        assert_eq!(parsed.max_fee_per_gas, 1);
        assert_eq!(parsed.max_priority_fee_per_gas, 1);
        assert_eq!(parsed.to, Some(addr(CAST_TO)));
        assert_eq!(parsed.value, U256::from(1u64));
        assert!(parsed.data.is_empty());
        assert_eq!(
            parsed.r,
            u256("e93452df983161572885d66e3496133be9eb8752254ba9ed2703cadd98b7fcba")
        );
        assert_eq!(
            parsed.s,
            u256("7c431cb342891e0fdc832c4d31a5585d4a5181a92b136034cedebbb047731cc6")
        );
        assert_eq!(parsed.y_parity, 1);
        assert_eq!(parsed.sender, addr(CAST_SENDER));
    }

    #[test]
    fn golden_cast_eip1559_chain_42069() {
        let parsed = parse_raw_tx(&hex(CAST_1559_RAW)).expect("cast 1559 vector must parse");
        assert_eq!(parsed.kind, TxKindParsed::Eip1559);
        assert_eq!(parsed.chain_id, Some(42069));
        assert_eq!(parsed.nonce, 1);
        assert_eq!(parsed.gas_limit, 21000);
        assert_eq!(parsed.max_fee_per_gas, 2);
        assert_eq!(parsed.max_priority_fee_per_gas, 1);
        assert_eq!(parsed.to, Some(addr(CAST_TO)));
        assert_eq!(parsed.value, U256::from(1u64));
        assert!(parsed.data.is_empty());
        assert_eq!(
            parsed.r,
            u256("85241832afad022300a084ba17874a17f0557b94a5cede609763b396d7fd0390")
        );
        assert_eq!(
            parsed.s,
            u256("40c223c96fca2e5b28d0cdcba2fe69cd0b3e817c5e61433e9c9aaf18834d1394")
        );
        assert_eq!(parsed.y_parity, 1);
        assert_eq!(parsed.sender, addr(CAST_SENDER));
    }

    #[test]
    fn determinism_same_input_same_output() {
        for raw in [EIP155_SPEC_RAW, CAST_LEGACY_RAW, CAST_1559_RAW] {
            let bytes = hex(raw);
            let a = parse_raw_tx(&bytes).unwrap();
            let b = parse_raw_tx(&bytes).unwrap();
            assert_same(&a, &b);
        }
    }

    // ── Rejection tests: every reachable TxParseError variant ────────
    //
    // Not covered (unreachable from parse_raw_tx by construction):
    //   - InvalidChainId: never constructed; engine.rs owns the chain-id check.
    //   - HashFailure: only a short syscall multihash triggers it; the
    //     host-crypto keccak is infallible.

    #[test]
    fn reject_empty() {
        assert!(matches!(parse_raw_tx(&[]), Err(TxParseError::Empty)));
    }

    #[test]
    fn reject_rlp_decode() {
        // List header promises 2 payload bytes, only 1 present.
        assert!(matches!(
            parse_raw_tx(&[0xc2, 0x01]),
            Err(TxParseError::RlpDecode)
        ));
        // Truncated golden vectors (outer header longer than remaining input).
        let legacy = hex(CAST_LEGACY_RAW);
        assert!(matches!(
            parse_raw_tx(&legacy[..legacy.len() - 1]),
            Err(TxParseError::RlpDecode)
        ));
        let t1559 = hex(CAST_1559_RAW);
        assert!(matches!(
            parse_raw_tx(&t1559[..t1559.len() - 1]),
            Err(TxParseError::RlpDecode)
        ));
        // 0x02 envelope whose payload is an RLP string, not a list.
        assert!(matches!(
            parse_raw_tx(&[0x02, 0x83, 0x01, 0x02, 0x03]),
            Err(TxParseError::RlpDecode)
        ));
    }

    #[test]
    fn reject_unsupported_types() {
        // EIP-2930 / EIP-4844 / EIP-7702 typed envelopes + unknown type bytes.
        for (raw, ty) in [
            (vec![0x01u8, 0xc0], 0x01u8), // EIP-2930
            (vec![0x03, 0xc0], 0x03),     // EIP-4844
            (vec![0x04, 0xc0], 0x04),     // EIP-7702
            (vec![0x05, 0xc0], 0x05),     // unknown future type
            (vec![0x80], 0x80),           // top-level RLP string (not a tx)
        ] {
            match parse_raw_tx(&raw) {
                Err(TxParseError::UnsupportedType(b)) => assert_eq!(b, ty),
                other => panic!(
                    "expected UnsupportedType({ty:#x}), got {other:?}",
                    other = other.map(|_| "Ok")
                ),
            }
        }
    }

    #[test]
    fn reject_wrong_field_count() {
        let empty_item: &[u8] = &[0x80];
        // Legacy: 8 and 10 items instead of 9.
        assert!(matches!(
            parse_raw_tx(&build_rlp_list(&[empty_item; 8])),
            Err(TxParseError::WrongFieldCount)
        ));
        assert!(matches!(
            parse_raw_tx(&build_rlp_list(&[empty_item; 10])),
            Err(TxParseError::WrongFieldCount)
        ));
        // EIP-1559: 11 and 13 items instead of 12.
        for n in [11usize, 13] {
            let items: Vec<&[u8]> = core::iter::repeat_n(empty_item, n).collect();
            let mut raw = vec![0x02u8];
            raw.extend_from_slice(&build_rlp_list(&items));
            assert!(matches!(
                parse_raw_tx(&raw),
                Err(TxParseError::WrongFieldCount)
            ));
        }
    }

    #[test]
    fn reject_legacy_invalid_v() {
        let raw = hex(CAST_LEGACY_RAW);
        // v ∈ {0, 26, 29, 30, 34} — neither pre-155 (27/28) nor EIP-155 (≥35).
        for v in [&[0x80][..], &[0x1a], &[0x1d], &[0x1e], &[0x22]] {
            assert!(matches!(
                parse_raw_tx(&mutate_legacy(&raw, 6, v)),
                Err(TxParseError::InvalidSignature)
            ));
        }
    }

    #[test]
    fn reject_pre155() {
        let raw = hex(CAST_LEGACY_RAW);
        for v in [0x1bu8, 0x1c] {
            // v = 27 / 28: unprotected pre-EIP-155 signature
            assert!(matches!(
                parse_raw_tx(&mutate_legacy(&raw, 6, &[v])),
                Err(TxParseError::Pre155Rejected)
            ));
        }
    }

    #[test]
    fn reject_zero_r_or_s() {
        let raw = hex(CAST_LEGACY_RAW);
        // r = 0 (canonical empty-string encoding)
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&raw, 7, &[0x80])),
            Err(TxParseError::InvalidSignature)
        ));
        // s = 0
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&raw, 8, &[0x80])),
            Err(TxParseError::InvalidSignature)
        ));
    }

    #[test]
    fn reject_r_or_s_at_group_order() {
        let raw = hex(CAST_LEGACY_RAW);
        let n_item = rlp_str(&SECP256K1_N.to_be_bytes::<32>());
        // r = N and s = N: out of the [1, N-1] scalar range
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&raw, 7, &n_item)),
            Err(TxParseError::InvalidSignature)
        ));
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&raw, 8, &n_item)),
            Err(TxParseError::InvalidSignature)
        ));
    }

    #[test]
    fn reject_high_s() {
        // Flip s to its high-order twin (N - s) — EIP-2 malleability defense.
        let legacy = hex(CAST_LEGACY_RAW);
        let s = u256("7c431cb342891e0fdc832c4d31a5585d4a5181a92b136034cedebbb047731cc6");
        let high_s = rlp_str(&(SECP256K1_N - s).to_be_bytes::<32>());
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 8, &high_s)),
            Err(TxParseError::InvalidSignature)
        ));

        let t1559 = hex(CAST_1559_RAW);
        let s = u256("40c223c96fca2e5b28d0cdcba2fe69cd0b3e817c5e61433e9c9aaf18834d1394");
        let high_s = rlp_str(&(SECP256K1_N - s).to_be_bytes::<32>());
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&t1559, 11, &high_s)),
            Err(TxParseError::InvalidSignature)
        ));
    }

    #[test]
    fn reject_bad_y_parity_1559() {
        let raw = hex(CAST_1559_RAW);
        // y_parity = 2 (must be 0 or 1)
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&raw, 9, &[0x02])),
            Err(TxParseError::InvalidSignature)
        ));
    }

    #[test]
    fn reject_address_length() {
        let legacy = hex(CAST_LEGACY_RAW);
        for bad_to in [rlp_str(&[0x11; 19]), rlp_str(&[0x11; 21])] {
            assert!(matches!(
                parse_raw_tx(&mutate_legacy(&legacy, 3, &bad_to)),
                Err(TxParseError::AddressLength)
            ));
        }
        let t1559 = hex(CAST_1559_RAW);
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&t1559, 5, &rlp_str(&[0x11; 19]))),
            Err(TxParseError::AddressLength)
        ));
    }

    #[test]
    fn reject_scalar_too_large() {
        let legacy = hex(CAST_LEGACY_RAW);
        // nonce: 9 bytes > u64
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 0, &rlp_str(&[0x01; 9]))),
            Err(TxParseError::ScalarTooLarge)
        ));
        // gas_price: 17 bytes > u128
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 1, &rlp_str(&[0x01; 17]))),
            Err(TxParseError::ScalarTooLarge)
        ));
        // value: 33 bytes > u256
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 4, &rlp_str(&[0x01; 33]))),
            Err(TxParseError::ScalarTooLarge)
        ));
        // r: 33 bytes > u256
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 7, &rlp_str(&[0x01; 33]))),
            Err(TxParseError::ScalarTooLarge)
        ));
    }

    #[test]
    fn reject_non_canonical_integer() {
        let legacy = hex(CAST_LEGACY_RAW);
        // zero must be the empty string, not [0x00]
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 0, &rlp_str(&[0x00]))),
            Err(TxParseError::NonCanonicalInteger)
        ));
        // leading-zero scalar: gas_limit = 0x005208
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&legacy, 2, &rlp_str(&[0x00, 0x52, 0x08]))),
            Err(TxParseError::NonCanonicalInteger)
        ));
        let t1559 = hex(CAST_1559_RAW);
        // chain_id = 0x00a455
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&t1559, 0, &rlp_str(&[0x00, 0xa4, 0x55]))),
            Err(TxParseError::NonCanonicalInteger)
        ));
    }

    #[test]
    fn reject_trailing_bytes() {
        let mut legacy = hex(CAST_LEGACY_RAW);
        legacy.push(0x00);
        assert!(matches!(
            parse_raw_tx(&legacy),
            Err(TxParseError::TrailingBytes)
        ));
        let mut t1559 = hex(CAST_1559_RAW);
        t1559.push(0x00);
        assert!(matches!(
            parse_raw_tx(&t1559),
            Err(TxParseError::TrailingBytes)
        ));
    }

    #[test]
    fn reject_access_list() {
        let raw = hex(CAST_1559_RAW);
        // Non-empty access list (one entry)
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&raw, 8, &[0xc1, 0x01])),
            Err(TxParseError::AccessListUnsupported)
        ));
        // Access list slot holding an RLP string instead of a list
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&raw, 8, &[0x80])),
            Err(TxParseError::AccessListUnsupported)
        ));
    }

    #[test]
    fn reject_invalid_fee_ordering() {
        let raw = hex(CAST_1559_RAW);
        // max_priority_fee_per_gas = 3 > max_fee_per_gas = 2
        assert!(matches!(
            parse_raw_tx(&mutate_1559(&raw, 2, &[0x03])),
            Err(TxParseError::InvalidFeeOrdering)
        ));
    }

    #[test]
    fn reject_recovery_failure() {
        // Replace r with an in-range scalar whose x-coordinate is not on the
        // curve (x^3 + 7 is a non-residue), so point recovery must fail.
        let raw = hex(CAST_LEGACY_RAW);
        let bad_r: &[u8] = &[0x05];
        assert!(matches!(
            parse_raw_tx(&mutate_legacy(&raw, 7, bad_r)),
            Err(TxParseError::RecoveryFailure)
        ));
    }
}
