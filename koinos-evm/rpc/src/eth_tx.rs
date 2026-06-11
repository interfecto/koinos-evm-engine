//! Decode a signed Ethereum raw tx far enough to extract `from`, `to`, `nonce`, `value`, etc.
//!
//! We don't validate the signature here — the engine does that on submit_raw_tx. We just
//! decode the RLP envelope so we can answer eth_getTransactionByHash with the right fields.

use anyhow::{Result, anyhow};
use sha3::{Digest, Keccak256};

#[derive(Debug, Clone)]
pub struct DecodedTx {
    pub tx_type: u8,
    pub chain_id: Option<u64>,
    pub nonce: u64,
    pub to: Option<[u8; 20]>,
    pub value: [u8; 32],
    pub data: Vec<u8>,
    pub gas_limit: u64,
    /// The price the sender committed to pay per gas, 32-byte BE:
    /// legacy `gas_price`, or EIP-1559 `max_fee_per_gas`. The engine charges 0
    /// regardless (zero-fee policy) — this is used for the relay's admission floor
    /// (MIN_GAS_PRICE_WEI) and for eth_getTransactionByHash fidelity.
    pub gas_price: [u8; 32],
    pub from: [u8; 20],
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub v: u64,
}

pub fn decode_and_recover(raw: &[u8]) -> Result<DecodedTx> {
    if raw.is_empty() {
        return Err(anyhow!("empty raw_tx"));
    }
    let first = raw[0];
    if first >= 0xc0 {
        decode_legacy(raw)
    } else if first == 0x02 {
        decode_eip1559(&raw[1..])
    } else {
        Err(anyhow!("unsupported tx type: 0x{:02x}", first))
    }
}

// ── RLP primitives (minimal) ────────────────────────────────────────────

struct RlpReader<'a> {
    items: Vec<&'a [u8]>,
}

impl<'a> RlpReader<'a> {
    fn from_list(buf: &'a [u8]) -> Result<Self> {
        let (list, payload_len, payload_start) = rlp_header(buf)?;
        if !list {
            return Err(anyhow!("expected RLP list"));
        }
        let payload_end = payload_start
            .checked_add(payload_len)
            .ok_or_else(|| anyhow!("RLP list length overflow"))?;
        if payload_end > buf.len() {
            return Err(anyhow!("RLP list payload truncated"));
        }
        let payload = &buf[payload_start..payload_end];
        let mut items = Vec::new();
        let mut pos = 0;
        while pos < payload.len() {
            let item_start = pos;
            let (_is_list, item_payload_len, item_payload_start) = rlp_header(&payload[pos..])?;
            let total_len = item_payload_start
                .checked_add(item_payload_len)
                .ok_or_else(|| anyhow!("RLP item length overflow"))?;
            let item_end = item_start
                .checked_add(total_len)
                .ok_or_else(|| anyhow!("RLP item length overflow"))?;
            if item_end > payload.len() {
                return Err(anyhow!("RLP item truncated"));
            }
            items.push(&payload[item_start..item_end]);
            pos = item_end;
        }
        Ok(Self { items })
    }

    fn item(&self, idx: usize) -> Result<&'a [u8]> {
        self.items
            .get(idx)
            .copied()
            .ok_or_else(|| anyhow!("missing RLP item at index {}", idx))
    }
}

/// Returns (is_list, payload_len, payload_start_offset).
fn rlp_header(buf: &[u8]) -> Result<(bool, usize, usize)> {
    if buf.is_empty() {
        return Err(anyhow!("RLP buf empty"));
    }
    let b = buf[0];
    if b <= 0x7f {
        Ok((false, 1, 0))
    } else if b <= 0xb7 {
        Ok((false, (b - 0x80) as usize, 1))
    } else if b <= 0xbf {
        let len_of_len = (b - 0xb7) as usize;
        if buf.len() < 1 + len_of_len {
            return Err(anyhow!("RLP len-of-len truncated"));
        }
        let mut len = 0usize;
        for i in 0..len_of_len {
            len = (len << 8) | buf[1 + i] as usize;
        }
        Ok((false, len, 1 + len_of_len))
    } else if b <= 0xf7 {
        Ok((true, (b - 0xc0) as usize, 1))
    } else {
        let len_of_len = (b - 0xf7) as usize;
        if buf.len() < 1 + len_of_len {
            return Err(anyhow!("RLP list len-of-len truncated"));
        }
        let mut len = 0usize;
        for i in 0..len_of_len {
            len = (len << 8) | buf[1 + i] as usize;
        }
        Ok((true, len, 1 + len_of_len))
    }
}

fn rlp_decode_bytes(buf: &[u8]) -> Result<&[u8]> {
    let (is_list, payload_len, payload_start) = rlp_header(buf)?;
    if is_list {
        return Err(anyhow!("expected bytes, got list"));
    }
    // Single-byte case: when b <= 0x7f, the byte IS the value
    if buf[0] <= 0x7f {
        return Ok(&buf[..1]);
    }
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or_else(|| anyhow!("RLP bytes length overflow"))?;
    if payload_end > buf.len() {
        return Err(anyhow!("RLP bytes truncated"));
    }
    Ok(&buf[payload_start..payload_end])
}

fn decode_u64(item: &[u8]) -> Result<u64> {
    let bytes = rlp_decode_bytes(item)?;
    if bytes.len() > 8 {
        return Err(anyhow!("integer > 8 bytes"));
    }
    let mut out = 0u64;
    for &x in bytes {
        out = (out << 8) | x as u64;
    }
    Ok(out)
}

fn decode_u256_be(item: &[u8]) -> Result<[u8; 32]> {
    let b = rlp_decode_bytes(item)?;
    if b.len() > 32 {
        return Err(anyhow!("value > 32 bytes"));
    }
    let mut out = [0u8; 32];
    out[32 - b.len()..].copy_from_slice(b);
    Ok(out)
}

fn decode_address(item: &[u8]) -> Result<Option<[u8; 20]>> {
    let b = rlp_decode_bytes(item)?;
    if b.is_empty() {
        Ok(None)
    } else if b.len() == 20 {
        let mut a = [0u8; 20];
        a.copy_from_slice(b);
        Ok(Some(a))
    } else {
        Err(anyhow!("expected 0 or 20 byte address, got {}", b.len()))
    }
}

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

// ── Legacy (incl. EIP-155) ──────────────────────────────────────────────

fn decode_legacy(raw: &[u8]) -> Result<DecodedTx> {
    let reader = RlpReader::from_list(raw)?;
    let nonce = decode_u64(reader.item(0)?)?;
    let gas_price = decode_u256_be(reader.item(1)?)?;
    let gas_limit = decode_u64(reader.item(2)?)?;
    let to = decode_address(reader.item(3)?)?;
    let value = decode_u256_be(reader.item(4)?)?;
    let data = rlp_decode_bytes(reader.item(5)?)?.to_vec();
    let v = decode_u64(reader.item(6)?)?;
    let r = decode_u256_be(reader.item(7)?)?;
    let s = decode_u256_be(reader.item(8)?)?;

    let (chain_id, y_parity) = if v == 27 || v == 28 {
        (None, (v - 27) as u8)
    } else if v >= 35 {
        let cid = (v - 35) / 2;
        let yp = ((v - 35) % 2) as u8;
        (Some(cid), yp)
    } else {
        return Err(anyhow!("invalid v: {}", v));
    };

    // Reconstruct the signing payload bytes for the original message hash.
    // For EIP-155: rlp([n, gp, gl, to, v, d, chain_id, 0, 0])
    // For pre-155: rlp([n, gp, gl, to, v, d])
    let signing_hash = if let Some(cid) = chain_id {
        build_legacy_eip155_signing_hash(raw, cid)?
    } else {
        build_legacy_pre155_signing_hash(raw)?
    };

    let from = recover_sender(&signing_hash, &r, &s, y_parity)?;

    Ok(DecodedTx {
        tx_type: 0,
        chain_id,
        nonce,
        to,
        value,
        data,
        gas_limit,
        gas_price,
        from,
        r,
        s,
        v,
    })
}

fn build_legacy_pre155_signing_hash(raw: &[u8]) -> Result<[u8; 32]> {
    let reader = RlpReader::from_list(raw)?;
    let items: Vec<&[u8]> = (0..6).map(|i| reader.item(i)).collect::<Result<Vec<_>>>()?;
    let rlp = wrap_rlp_list(&items);
    Ok(keccak256(&rlp))
}

fn build_legacy_eip155_signing_hash(raw: &[u8], chain_id: u64) -> Result<[u8; 32]> {
    let reader = RlpReader::from_list(raw)?;
    let mut items: Vec<&[u8]> = (0..6).map(|i| reader.item(i)).collect::<Result<Vec<_>>>()?;
    let chain_id_bytes = rlp_encode_u64(chain_id);
    let zero_bytes = vec![0x80u8]; // rlp(0)
    items.push(&chain_id_bytes);
    items.push(&zero_bytes);
    items.push(&zero_bytes);
    let rlp = wrap_rlp_list(&items);
    Ok(keccak256(&rlp))
}

// ── EIP-1559 ────────────────────────────────────────────────────────────

fn decode_eip1559(payload: &[u8]) -> Result<DecodedTx> {
    let reader = RlpReader::from_list(payload)?;
    let chain_id = decode_u64(reader.item(0)?)?;
    let nonce = decode_u64(reader.item(1)?)?;
    let _max_priority = rlp_decode_bytes(reader.item(2)?)?;
    let max_fee = decode_u256_be(reader.item(3)?)?;
    let gas_limit = decode_u64(reader.item(4)?)?;
    let to = decode_address(reader.item(5)?)?;
    let value = decode_u256_be(reader.item(6)?)?;
    let data = rlp_decode_bytes(reader.item(7)?)?.to_vec();
    // items[8] = access_list (not used for recovery, but part of signing hash)
    let y_parity = decode_u64(reader.item(9)?)? as u8;
    let r = decode_u256_be(reader.item(10)?)?;
    let s = decode_u256_be(reader.item(11)?)?;

    // Signing hash = keccak256(0x02 || rlp([items[0..9]]))
    let items: Vec<&[u8]> = (0..9).map(|i| reader.item(i)).collect::<Result<Vec<_>>>()?;
    let mut rlp = wrap_rlp_list(&items);
    rlp.insert(0, 0x02);
    let signing_hash = keccak256(&rlp);

    let from = recover_sender(&signing_hash, &r, &s, y_parity)?;

    Ok(DecodedTx {
        tx_type: 2,
        chain_id: Some(chain_id),
        nonce,
        to,
        value,
        data,
        gas_limit,
        gas_price: max_fee,
        from,
        r,
        s,
        v: y_parity as u64,
    })
}

// ── Signer recovery via secp256k1 ───────────────────────────────────────

fn recover_sender(
    msg_hash: &[u8; 32],
    r: &[u8; 32],
    s: &[u8; 32],
    y_parity: u8,
) -> Result<[u8; 20]> {
    use secp256k1::{Message, Secp256k1, ecdsa::RecoverableSignature, ecdsa::RecoveryId};
    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(r);
    sig_bytes[32..].copy_from_slice(s);
    let rec_id = RecoveryId::try_from(y_parity as i32)
        .map_err(|_| anyhow!("invalid recovery id: {}", y_parity))?;
    let sig = RecoverableSignature::from_compact(&sig_bytes, rec_id)?;
    let msg = Message::from_digest(*msg_hash);
    let secp = Secp256k1::verification_only();
    let pubkey = secp.recover_ecdsa(&msg, &sig)?;
    let serialized = pubkey.serialize_uncompressed();
    let hash = keccak256(&serialized[1..]); // strip 0x04 prefix
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash[12..32]);
    Ok(addr)
}

// ── RLP encode helpers ──────────────────────────────────────────────────

fn rlp_encode_u64(n: u64) -> Vec<u8> {
    if n == 0 {
        return vec![0x80];
    }
    let be = n.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(8);
    let bytes = &be[start..];
    if bytes.len() == 1 && bytes[0] <= 0x7f {
        vec![bytes[0]]
    } else {
        let mut out = vec![0x80 + bytes.len() as u8];
        out.extend_from_slice(bytes);
        out
    }
}

fn wrap_rlp_list(items: &[&[u8]]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(|i| i.len()).sum();
    let mut out = Vec::with_capacity(payload_len + 9);
    if payload_len <= 55 {
        out.push(0xc0 + payload_len as u8);
    } else {
        let len_bytes = payload_len.to_be_bytes();
        let start = len_bytes.iter().position(|&b| b != 0).unwrap_or(8);
        let lb = &len_bytes[start..];
        out.push(0xf7 + lb.len() as u8);
        out.extend_from_slice(lb);
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The EIP-155 example transaction (chain id 1), signed with private key
    /// 0x4646...46 — sender 0x9d8A62f656a8d1615C1294fd71e9CFb3E4855A4F.
    const EIP155_RAW: &str = "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83";

    #[test]
    fn golden_eip155_decode_and_recover() {
        let raw = hex::decode(EIP155_RAW).unwrap();
        let tx = decode_and_recover(&raw).unwrap();
        assert_eq!(tx.tx_type, 0);
        assert_eq!(tx.chain_id, Some(1));
        assert_eq!(tx.nonce, 9);
        assert_eq!(tx.gas_limit, 21_000);
        assert_eq!(tx.to, Some([0x35; 20]));
        // gas_price = 20 gwei
        let mut gp = [0u8; 32];
        gp[27..].copy_from_slice(&20_000_000_000u64.to_be_bytes()[3..]);
        assert_eq!(tx.gas_price, gp);
        assert_eq!(
            hex::encode(tx.from),
            "9d8a62f656a8d1615c1294fd71e9cfb3e4855a4f"
        );
    }

    #[test]
    fn malformed_inputs_error_instead_of_panicking() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],                                          // empty
            vec![0xde, 0xad, 0xbe, 0xef], // list header claiming 30-byte payload in 4 bytes
            vec![0xc0],                   // empty list (missing items)
            vec![0xf8],                   // len-of-len truncated
            vec![0xfb, 0xff, 0xff, 0xff], // huge list length, truncated
            vec![0x02],                   // 1559 marker with no payload
            vec![0x02, 0xde, 0xad],       // 1559 with garbage payload
            vec![0x01, 0xc0],             // unsupported tx type (2930)
            hex::decode(EIP155_RAW).unwrap()[..30].to_vec(), // truncated valid tx
        ];
        for case in cases {
            assert!(
                decode_and_recover(&case).is_err(),
                "expected error for {:02x?}",
                case
            );
        }
    }

    #[test]
    fn oversized_integer_rejected() {
        // Legacy tx shape with a 9-byte nonce — decode_u64 must reject, not wrap.
        let mut items: Vec<Vec<u8>> = Vec::new();
        let mut nonce = vec![0x89]; // 9-byte string
        nonce.extend_from_slice(&[0xff; 9]);
        items.push(nonce);
        for _ in 0..8 {
            items.push(vec![0x80]); // empty/zero placeholders
        }
        let refs: Vec<&[u8]> = items.iter().map(|v| v.as_slice()).collect();
        let raw = wrap_rlp_list(&refs);
        assert!(decode_and_recover(&raw).is_err());
    }
}
