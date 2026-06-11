//! MVP precompiles: ecRecover (0x01), SHA-256 (0x02), RIPEMD-160 (0x03), identity (0x04).
//!
//! These delegate to Koinos native system calls where possible for performance,
//! avoiding double-interpretation overhead for cryptographic operations.

use alloc::vec::Vec;

use revm::precompile::{
    PrecompileError, PrecompileErrors, PrecompileOutput, PrecompileResult, u64_to_address,
};
use revm::primitives::{Address, Bytes};

use crate::koinos::{self, sys};

/// Precompile addresses
pub const ECRECOVER_ADDR: Address = u64_to_address(1);
pub const SHA256_ADDR: Address = u64_to_address(2);
pub const RIPEMD160_ADDR: Address = u64_to_address(3);
pub const IDENTITY_ADDR: Address = u64_to_address(4);

/// Run the ecRecover precompile (address 0x01).
///
/// Recovers an Ethereum address from a message hash and signature.
/// Input: hash[32] || v[32] || r[32] || s[32] = 128 bytes
/// Output: address padded to 32 bytes
pub fn ec_recover(input: &Bytes, gas_limit: u64) -> PrecompileResult {
    const EC_RECOVER_BASE: u64 = 3000;
    if gas_limit < EC_RECOVER_BASE {
        return Err(PrecompileErrors::Error(PrecompileError::OutOfGas));
    }

    // Input must be exactly 128 bytes (or padded with zeros)
    let mut padded = [0u8; 128];
    let len = core::cmp::min(input.len(), 128);
    padded[..len].copy_from_slice(&input[..len]);

    let hash = &padded[0..32];
    let v_bytes = &padded[32..64];
    let r_bytes = &padded[64..96];
    let s_bytes = &padded[96..128];

    // Per EVM spec, `v` is a 32-byte big-endian integer: high 31 bytes MUST be zero,
    // last byte MUST be 27 or 28. revm enforces both. Returning empty (not error) is
    // the spec-correct behavior for any deviation.
    if !v_bytes[..31].iter().all(|&b| b == 0) {
        return Ok(PrecompileOutput::new(EC_RECOVER_BASE, Bytes::new()));
    }
    let v = v_bytes[31];
    if v != 27 && v != 28 {
        return Ok(PrecompileOutput::new(EC_RECOVER_BASE, Bytes::new()));
    }

    // Koinos `recoverable_signature` (Bitcoin compact format):
    //   byte 0      = header = 31 + y_parity  (in [31,33], we use 31|32)
    //   bytes 1..33 = r
    //   bytes 33..65 = s
    let y_parity = v - 27;
    let mut signature = Vec::with_capacity(65);
    signature.push(31u8 + y_parity);
    signature.extend_from_slice(r_bytes);
    signature.extend_from_slice(s_bytes);

    // Build the digest as a multihash: keccak-256 prefix (0x1b, 0x20) + hash[32]
    let mut digest = Vec::with_capacity(34);
    digest.push(0x1b); // keccak-256 multicodec
    digest.push(0x20); // 32 bytes length
    digest.extend_from_slice(hash);

    // Recover uncompressed public key via Koinos system call
    let pubkey = sys::recover_public_key(
        koinos::DSA_ECDSA_SECP256K1,
        &signature,
        &digest,
        false, // uncompressed
    );

    if pubkey.is_empty() || pubkey.len() < 64 {
        return Ok(PrecompileOutput::new(EC_RECOVER_BASE, Bytes::new()));
    }

    // Derive Ethereum address: keccak256(pubkey[1..]) -> last 20 bytes
    // Skip the 0x04 prefix byte for uncompressed key
    let pubkey_bytes = if pubkey[0] == 0x04 {
        &pubkey[1..]
    } else {
        &pubkey[..]
    };

    let hash_result = sys::hash(koinos::HASH_KECCAK_256, pubkey_bytes);
    if hash_result.len() < 34 {
        // multihash result should be 2 byte prefix + 32 bytes hash
        return Ok(PrecompileOutput::new(EC_RECOVER_BASE, Bytes::new()));
    }

    // Skip multihash prefix (2 bytes: codec + length), take last 20 bytes
    let keccak_hash = &hash_result[2..34];
    let mut output = [0u8; 32];
    output[12..32].copy_from_slice(&keccak_hash[12..32]);

    Ok(PrecompileOutput::new(
        EC_RECOVER_BASE,
        Bytes::copy_from_slice(&output),
    ))
}

/// Run the SHA-256 precompile (address 0x02).
pub fn sha256_run(input: &Bytes, gas_limit: u64) -> PrecompileResult {
    let gas = 60 + 12 * (input.len() as u64).div_ceil(32);
    if gas_limit < gas {
        return Err(PrecompileErrors::Error(PrecompileError::OutOfGas));
    }

    let result = sys::hash(koinos::HASH_SHA2_256, input);
    if result.len() >= 34 {
        // Strip multihash prefix (0x12, 0x20)
        Ok(PrecompileOutput::new(
            gas,
            Bytes::copy_from_slice(&result[2..34]),
        ))
    } else {
        Err(PrecompileErrors::Error(PrecompileError::Other(
            "sha256 system call failed".into(),
        )))
    }
}

/// Run the RIPEMD-160 precompile (address 0x03).
pub fn ripemd160_run(input: &Bytes, gas_limit: u64) -> PrecompileResult {
    let gas = 600 + 120 * (input.len() as u64).div_ceil(32);
    if gas_limit < gas {
        return Err(PrecompileErrors::Error(PrecompileError::OutOfGas));
    }

    let result = sys::hash(koinos::HASH_RIPEMD_160, input);
    // RIPEMD-160 multihash prefix: varint(0x1053) + varint(20)
    // The multihash encoding for 0x1053 is 2+ bytes as a varint
    // Result should be prefix + 20 bytes hash
    // We need the last 20 bytes, left-padded to 32 bytes

    // Find the hash data after the multihash prefix
    if result.len() >= 22 {
        // Skip multihash prefix bytes to get the raw 20-byte hash
        let hash_start = result.len() - 20;
        let mut output = [0u8; 32];
        output[12..32].copy_from_slice(&result[hash_start..]);
        Ok(PrecompileOutput::new(gas, Bytes::copy_from_slice(&output)))
    } else {
        Err(PrecompileErrors::Error(PrecompileError::Other(
            "ripemd160 system call failed".into(),
        )))
    }
}

/// Run the identity precompile (address 0x04).
pub fn identity_run(input: &Bytes, gas_limit: u64) -> PrecompileResult {
    let gas = 15 + 3 * (input.len() as u64).div_ceil(32);
    if gas_limit < gas {
        return Err(PrecompileErrors::Error(PrecompileError::OutOfGas));
    }

    Ok(PrecompileOutput::new(gas, input.clone()))
}
