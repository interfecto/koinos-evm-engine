//! State layout definitions for the EVM engine.
//!
//! All EVM state lives within the engine contract's own zone in non-system space.

use alloc::vec::Vec;
use crate::koinos::sys;

/// Koinos object_space descriptor.
pub struct ObjectSpace {
    pub system: bool,
    pub zone: Vec<u8>,
    pub id: u32,
}

// ── Space IDs ────────────────────────────────────────────────────────────

/// EVM account data: eth_addr[20] -> {nonce, balance[32], code_hash[32]}
pub const SPACE_ACCOUNTS: u32 = 0;

/// EVM contract code: code_hash[32] -> raw bytecode
pub const SPACE_CODE: u32 = 1;

/// EVM storage: eth_addr[20] || slot[32] (52 bytes) -> value[32]
pub const SPACE_STORAGE: u32 = 2;

/// Engine config: "config" -> {chain_id, ticks_per_gas, owner}
pub const SPACE_CONFIG: u32 = 3;

// SPACE_NONCES (id=4) removed: EVM nonce lives in AccountInfo (SPACE_ACCOUNTS).
// Keeping a second nonce store would create dual-bookkeeping bugs at raw-tx ingress.

/// Test space for Phase 0 validation
pub const SPACE_TEST: u32 = 100;

// ── Space constructors ───────────────────────────────────────────────────

/// Get the contract's own zone (contract ID).
/// Panics if the contract ID is empty (should never happen in normal execution).
fn contract_zone() -> Vec<u8> {
    let zone = sys::get_contract_id();
    if zone.is_empty() {
        sys::log("FATAL: get_contract_id returned empty");
        sys::exit_error(b"get_contract_id failed");
        unreachable!()
    }
    zone
}

/// Create an object space for a given space ID within the engine's zone.
pub fn engine_space(id: u32) -> ObjectSpace {
    ObjectSpace {
        system: false,
        zone: contract_zone(),
        id,
    }
}

/// Accounts space.
pub fn accounts_space() -> ObjectSpace {
    engine_space(SPACE_ACCOUNTS)
}

/// Code space.
pub fn code_space() -> ObjectSpace {
    engine_space(SPACE_CODE)
}

/// Storage space.
pub fn storage_space() -> ObjectSpace {
    engine_space(SPACE_STORAGE)
}

/// Config space.
pub fn config_space() -> ObjectSpace {
    engine_space(SPACE_CONFIG)
}

/// Test space for Phase 0.
pub fn test_space() -> ObjectSpace {
    engine_space(SPACE_TEST)
}

// ── Storage key helpers ──────────────────────────────────────────────────

/// Build a storage key from an Ethereum address and storage slot.
/// Key format: eth_addr[20] || slot[32] = 52 bytes
pub fn storage_key(address: &[u8; 20], slot: &[u8; 32]) -> [u8; 52] {
    let mut key = [0u8; 52];
    key[..20].copy_from_slice(address);
    key[20..].copy_from_slice(slot);
    key
}
