//! revm Database trait implementation backed by Koinos object_space storage.

use alloc::vec::Vec;
use core::fmt;

use revm::primitives::{AccountInfo, Address, B256, Bytecode, KECCAK_EMPTY, U256};
use revm::{Database, DatabaseCommit};

use crate::koinos::sys;
use crate::proto;
use crate::state;

/// Error type for Koinos database operations.
#[derive(Debug, Clone)]
pub enum KoinosDbError {
    /// Failed to read from state. Never constructed today: failing read syscalls
    /// log + return empty (treated as "not found"). Kept as error API surface.
    #[allow(dead_code)]
    ReadError,
    /// Failed to write to state. Never constructed today: failing write syscalls
    /// abort the transaction via `call_system_must`. Kept as error API surface.
    #[allow(dead_code)]
    WriteError,
    /// Data corruption.
    InvalidData,
}

impl fmt::Display for KoinosDbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadError => write!(f, "koinos read error"),
            Self::WriteError => write!(f, "koinos write error"),
            Self::InvalidData => write!(f, "invalid data in state"),
        }
    }
}

/// Koinos-backed database for revm.
///
/// Reads EVM state from Koinos object_space and tracks pending writes.
/// Changes are committed back to Koinos state via `commit()`.
pub struct KoinosDatabase;

impl KoinosDatabase {
    pub fn new() -> Self {
        Self
    }
}

// ── Account serialization ────────────────────────────────────────────────
//
// Account record in space 0, keyed by eth_addr[20]:
//   message EvmAccount {
//     uint64 nonce = 1;
//     bytes balance = 2;     // 32 bytes, big-endian U256
//     bytes code_hash = 3;   // 32 bytes, B256
//   }

fn serialize_account(info: &AccountInfo) -> Vec<u8> {
    let mut buf = Vec::with_capacity(80);
    proto::encode_varint_field(&mut buf, 1, info.nonce);
    let balance_bytes = info.balance.to_be_bytes::<32>();
    proto::encode_bytes_field(&mut buf, 2, &balance_bytes);
    proto::encode_bytes_field(&mut buf, 3, info.code_hash.as_slice());
    buf
}

fn deserialize_account(data: &[u8]) -> Option<AccountInfo> {
    let mut nonce: u64 = 0;
    let mut balance = U256::ZERO;
    let mut code_hash = KECCAK_EMPTY;

    for (field_num, field_val) in proto::FieldIter::new(data) {
        match field_num {
            1 => {
                if let Some(v) = proto::get_varint(&field_val) {
                    nonce = v;
                }
            }
            2 => {
                if let Some(bytes) = proto::get_bytes(&field_val)
                    && bytes.len() == 32
                {
                    balance = U256::from_be_slice(bytes);
                }
            }
            3 => {
                if let Some(bytes) = proto::get_bytes(&field_val)
                    && bytes.len() == 32
                {
                    code_hash = B256::from_slice(bytes);
                }
            }
            _ => {}
        }
    }

    Some(AccountInfo {
        nonce,
        balance,
        code_hash,
        code: None, // loaded lazily via code_by_hash
    })
}

impl Database for KoinosDatabase {
    type Error = KoinosDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let space = state::accounts_space();
        match sys::get_object(&space, address.as_slice()) {
            Some(data) => {
                let info = deserialize_account(&data).ok_or(KoinosDbError::InvalidData)?;
                Ok(Some(info))
            }
            None => Ok(None),
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if code_hash == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }

        let space = state::code_space();
        match sys::get_object(&space, code_hash.as_slice()) {
            Some(data) => Ok(Bytecode::new_raw(data.into())),
            None => Ok(Bytecode::default()),
        }
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let slot_bytes = index.to_be_bytes::<32>();
        let addr_bytes: &[u8; 20] = address.as_ref();
        let key = state::storage_key(addr_bytes, &slot_bytes);

        let space = state::storage_space();
        match sys::get_object(&space, &key) {
            Some(data) => {
                if data.len() == 32 {
                    Ok(U256::from_be_slice(&data))
                } else {
                    Ok(U256::ZERO)
                }
            }
            None => Ok(U256::ZERO),
        }
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        // KNOWN INCOMPAT (Phase A): EVM BLOCKHASH opcode returns 0x00...00 for all heights.
        //
        // Koinos `get_block_field` only reads the currently-executing block, not historical.
        // Implementing a proper EVM BLOCKHASH (last 256 block hashes) requires either:
        //   (a) a ring buffer maintained via pre_block_callback (system contract override), or
        //   (b) an oracle contract written by the operator.
        //
        // For Aave/Uni v2/v3/v4: BLOCKHASH is not on the critical path. Defer to Phase D+.
        // Any contract relying on BLOCKHASH for randomness or oracle ordering will see zeros.
        Ok(B256::ZERO)
    }
}

impl DatabaseCommit for KoinosDatabase {
    fn commit(&mut self, changes: revm::primitives::HashMap<Address, revm::primitives::Account>) {
        let accounts_space = state::accounts_space();
        let code_space = state::code_space();
        let storage_space = state::storage_space();

        for (address, account) in changes {
            // Skip untouched accounts
            if !account.is_touched() {
                continue;
            }

            // Handle account destruction (selfdestruct)
            if account.is_selfdestructed() {
                sys::remove_object(&accounts_space, address.as_slice());
                // Note: storage is not cleared here for simplicity.
                // Full selfdestruct cleanup would iterate storage keys.
                continue;
            }

            // Write account info
            let account_data = serialize_account(&account.info);
            sys::put_object(&accounts_space, address.as_slice(), &account_data);

            // Write code if it exists and is new
            if let Some(ref code) = account.info.code
                && !code.is_empty()
                && account.info.code_hash != KECCAK_EMPTY
            {
                sys::put_object(
                    &code_space,
                    account.info.code_hash.as_slice(),
                    code.original_bytes().as_ref(),
                );
            }

            // Write storage changes
            for (slot, value) in account.storage {
                let slot_bytes = slot.to_be_bytes::<32>();
                let addr_bytes: &[u8; 20] = address.as_ref();
                let key = state::storage_key(addr_bytes, &slot_bytes);

                if value.present_value().is_zero() {
                    // Clear storage slot
                    sys::remove_object(&storage_space, &key);
                } else {
                    let val_bytes = value.present_value().to_be_bytes::<32>();
                    sys::put_object(&storage_space, &key, &val_bytes);
                }
            }
        }
    }
}
