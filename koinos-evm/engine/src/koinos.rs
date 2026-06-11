//! Koinos system call FFI bridge.
//!
//! Wraps the two host imports (`invoke_thunk`, `invoke_system_call`) and provides
//! typed wrappers for the system calls needed by the EVM engine.

use alloc::vec;
use alloc::vec::Vec;

use crate::proto;
use crate::state::ObjectSpace;

// ── System call IDs ──────────────────────────────────────────────────────
// Currently-unused IDs carry `allow(dead_code)`: they are kept to document the
// Koinos syscall table. IDs only reachable from EVM code paths are dead without
// the `evm` feature.
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const SC_GET_HEAD_INFO: u32 = 1;
#[allow(dead_code)]
const SC_GET_TRANSACTION: u32 = 102;
#[allow(dead_code)]
const SC_GET_BLOCK: u32 = 104;
#[allow(dead_code)]
const SC_GET_LAST_IRREVERSIBLE_BLOCK: u32 = 106;
#[allow(dead_code)]
const SC_GET_ACCOUNT_RC: u32 = 201;
const SC_PUT_OBJECT: u32 = 301;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const SC_REMOVE_OBJECT: u32 = 302;
const SC_GET_OBJECT: u32 = 303;
#[allow(dead_code)]
const SC_GET_NEXT_OBJECT: u32 = 304;
const SC_LOG: u32 = 401;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const SC_EVENT: u32 = 402;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const SC_HASH: u32 = 501;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const SC_RECOVER_PUBLIC_KEY: u32 = 502;
#[allow(dead_code)]
const SC_CALL: u32 = 601;
const SC_EXIT: u32 = 602;
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const SC_GET_ARGUMENTS: u32 = 603;
const SC_GET_CONTRACT_ID: u32 = 604;
#[allow(dead_code)]
const SC_GET_CALLER: u32 = 605;
#[allow(dead_code)]
const SC_CHECK_AUTHORITY: u32 = 606;

// ── Hash multicodec values ───────────────────────────────────────────────
// Unused codecs kept to document the multicodec table; the used ones are only
// reachable from EVM code paths (tx.rs / precompiles.rs).
#[allow(dead_code)]
pub const HASH_SHA1: u64 = 0x11;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
pub const HASH_SHA2_256: u64 = 0x12;
#[allow(dead_code)]
pub const HASH_SHA2_512: u64 = 0x13;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
pub const HASH_KECCAK_256: u64 = 0x1b;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
pub const HASH_RIPEMD_160: u64 = 0x1053;

// ── DSA types ────────────────────────────────────────────────────────────
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
pub const DSA_ECDSA_SECP256K1: u64 = 0;

// ── Host imports ─────────────────────────────────────────────────────────
// The import module is pinned to "env" (what the Koinos Fizzy host resolves and
// what older rustc emitted implicitly). Newer rustc (≥1.9x) no longer auto-imports
// undefined wasm symbols without this attribute and fails at link time instead.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn invoke_system_call(
        sid: u32,
        ret_ptr: *mut u8,
        ret_len: u32,
        arg_ptr: *const u8,
        arg_len: u32,
        bytes_written: *mut u32,
    ) -> i32;
}

/// Non-wasm stub so host unit tests (`--features host-crypto`) and host clippy
/// runs can link. Koinos syscalls only exist inside the chain's WASM VM; any
/// call from a host test is a bug, so fail loudly.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn invoke_system_call(
    sid: u32,
    _ret_ptr: *mut u8,
    _ret_len: u32,
    _arg_ptr: *const u8,
    _arg_len: u32,
    _bytes_written: *mut u32,
) -> i32 {
    panic!("Koinos syscall {sid} invoked on a non-wasm target (no chain host available)");
}

// ── Return buffer ────────────────────────────────────────────────────────
const RET_BUF_SIZE: usize = 1024 * 512; // 512KB return buffer

/// Direct invoke of SC_LOG without going through call_system_small.
/// Used by syscall-failure logging to avoid infinite recursion if SC_LOG itself fails.
fn raw_log(msg: &[u8]) {
    let mut args_buf = Vec::with_capacity(msg.len() + 8);
    proto::encode_bytes_field(&mut args_buf, 1, msg);
    let mut ret_buf = [0u8; 64];
    let mut bytes_written: u32 = 0;
    unsafe {
        invoke_system_call(
            SC_LOG,
            ret_buf.as_mut_ptr(),
            64,
            args_buf.as_ptr(),
            args_buf.len() as u32,
            &mut bytes_written,
        );
    }
    // Best-effort: ignore rc to break any potential recursion loop.
}

fn call_system(id: u32, args: &[u8]) -> Vec<u8> {
    let mut ret_buf = vec![0u8; RET_BUF_SIZE];
    let mut bytes_written: u32 = 0;

    let rc = unsafe {
        invoke_system_call(
            id,
            ret_buf.as_mut_ptr(),
            RET_BUF_SIZE as u32,
            args.as_ptr(),
            args.len() as u32,
            &mut bytes_written,
        )
    };

    if rc != 0 {
        // Fail loud: log so failures appear in receipts instead of being swallowed.
        let msg = alloc::format!("syscall {} failed: rc={}", id, rc);
        raw_log(msg.as_bytes());
        return Vec::new();
    }

    ret_buf.truncate(bytes_written as usize);
    ret_buf
}

/// Smaller return buffer for calls that return small data.
fn call_system_small(id: u32, args: &[u8]) -> Vec<u8> {
    let mut ret_buf = vec![0u8; 4096];
    let mut bytes_written: u32 = 0;

    let rc = unsafe {
        invoke_system_call(
            id,
            ret_buf.as_mut_ptr(),
            4096,
            args.as_ptr(),
            args.len() as u32,
            &mut bytes_written,
        )
    };

    if rc != 0 {
        let msg = alloc::format!("syscall {} failed: rc={}", id, rc);
        raw_log(msg.as_bytes());
        return Vec::new();
    }

    ret_buf.truncate(bytes_written as usize);
    ret_buf
}

/// MUST-SUCCEED variant: invokes a state-changing syscall and aborts the entire
/// contract via `exit_error` if rc != 0. Use this for syscalls where silent failure
/// could corrupt state (put_object, remove_object, event). Reverts the transaction
/// rather than producing a successful-looking receipt with missing writes.
fn call_system_must(id: u32, args: &[u8]) -> Vec<u8> {
    let mut ret_buf = vec![0u8; 4096];
    let mut bytes_written: u32 = 0;

    let rc = unsafe {
        invoke_system_call(
            id,
            ret_buf.as_mut_ptr(),
            4096,
            args.as_ptr(),
            args.len() as u32,
            &mut bytes_written,
        )
    };

    if rc != 0 {
        // Log the failure first, then abort the transaction. Once exit_error fires
        // the contract execution ends and the transaction is reverted by the chain.
        let msg = alloc::format!("state syscall {} failed: rc={} (aborting)", id, rc);
        raw_log(msg.as_bytes());

        // Build a brief error result and invoke SC_EXIT directly to avoid recursion
        // through sys::exit_error → call_system_small → ...
        let err_msg = alloc::format!("state syscall {} rc={}", id, rc);
        let mut error_data = Vec::with_capacity(err_msg.len() + 8);
        proto::encode_bytes_field(&mut error_data, 1, err_msg.as_bytes());
        let mut result_msg = Vec::with_capacity(error_data.len() + 8);
        proto::encode_submessage_field(&mut result_msg, 2, &error_data);
        let mut exit_args = Vec::with_capacity(result_msg.len() + 8);
        proto::encode_varint_field(&mut exit_args, 1, 1); // code = 1 (failure)
        proto::encode_submessage_field(&mut exit_args, 2, &result_msg);

        let mut exit_ret = [0u8; 64];
        let mut exit_written: u32 = 0;
        unsafe {
            invoke_system_call(
                SC_EXIT,
                exit_ret.as_mut_ptr(),
                64,
                exit_args.as_ptr(),
                exit_args.len() as u32,
                &mut exit_written,
            );
        }
        // exit should not return; if it does, panic via unreachable
        unsafe { core::hint::unreachable_unchecked() }
    }

    ret_buf.truncate(bytes_written as usize);
    ret_buf
}

// ── Public API ───────────────────────────────────────────────────────────

pub mod sys {
    use super::*;

    /// Parsed arguments from get_arguments syscall.
    /// Only used by the wasm `_start` dispatcher; dead on host builds.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub struct Arguments {
        pub entry_point: u32,
        pub arguments: Vec<u8>,
    }

    /// Get the entry point and arguments for the current call.
    /// Uses the large buffer since calldata (EVM contract init code) can exceed 4KB.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn get_arguments() -> Arguments {
        let result = call_system(SC_GET_ARGUMENTS, &[]);

        // Decode get_arguments_result { argument_data value = 1; }
        // argument_data { uint32 entry_point = 1; bytes arguments = 2; }
        let mut entry_point: u32 = 0;
        let mut arguments = Vec::new();

        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1 {
                // submessage: argument_data
                if let Some(submsg) = proto::get_bytes(&field_val) {
                    for (sub_field, sub_val) in proto::FieldIter::new(submsg) {
                        match sub_field {
                            1 => {
                                if let Some(v) = proto::get_varint(&sub_val) {
                                    entry_point = v as u32;
                                }
                            }
                            2 => {
                                if let Some(v) = proto::get_bytes(&sub_val) {
                                    arguments = v.to_vec();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Arguments {
            entry_point,
            arguments,
        }
    }

    /// Exit with success, returning serialized result.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn exit_success(data: &[u8]) {
        // exit_arguments { int32 code = 1; result res = 2; }
        // result { oneof { bytes object = 1; error_data error = 2; } }
        let mut result_msg = Vec::new();
        proto::encode_bytes_field(&mut result_msg, 1, data); // result.object

        let mut args = Vec::new();
        // code = 0 (success), skip since default
        proto::encode_submessage_field(&mut args, 2, &result_msg); // res

        call_system_small(SC_EXIT, &args);
    }

    /// Exit with error. Message must be valid UTF-8 (protobuf string field).
    pub fn exit_error(message: &[u8]) {
        // error_data { string message = 1; repeated bytes data = 2; }
        // Validate UTF-8; fall back to generic message if invalid
        let msg = match core::str::from_utf8(message) {
            Ok(_) => message,
            Err(_) => b"contract error",
        };
        let mut error_data = Vec::new();
        proto::encode_bytes_field(&mut error_data, 1, msg);

        // result { error_data error = 2; }
        let mut result_msg = Vec::new();
        proto::encode_submessage_field(&mut result_msg, 2, &error_data);

        let mut args = Vec::new();
        proto::encode_varint_field(&mut args, 1, 1); // code = 1 (failure)
        proto::encode_submessage_field(&mut args, 2, &result_msg);

        call_system_small(SC_EXIT, &args);
    }

    /// Log a message.
    pub fn log(msg: &str) {
        // log_arguments { string message = 1; }
        let mut args = Vec::new();
        proto::encode_bytes_field(&mut args, 1, msg.as_bytes());
        call_system_small(SC_LOG, &args);
    }

    /// Emit an event. Aborts on syscall failure: events are part of the
    /// committed receipt and silently dropping them breaks indexers (eth_getLogs).
    #[cfg_attr(not(feature = "evm"), allow(dead_code))]
    pub fn event(name: &str, data: &[u8], impacted: &[&[u8]]) {
        // event_arguments { string name = 1; bytes data = 2; repeated bytes impacted = 3; }
        let mut args = Vec::new();
        proto::encode_bytes_field(&mut args, 1, name.as_bytes());
        proto::encode_bytes_field(&mut args, 2, data);
        for addr in impacted {
            proto::encode_bytes_field(&mut args, 3, addr);
        }
        call_system_must(SC_EVENT, &args);
    }

    // ── Database operations ──────────────────────────────────────────

    /// Encode an object_space into protobuf.
    fn encode_object_space(space: &ObjectSpace) -> Vec<u8> {
        let mut buf = Vec::new();
        proto::encode_bool_field(&mut buf, 1, space.system);
        proto::encode_bytes_field(&mut buf, 2, &space.zone);
        proto::encode_varint_field(&mut buf, 3, space.id as u64);
        buf
    }

    /// Store an object in state. Aborts the transaction on syscall failure
    /// to prevent silent state-write loss (e.g. a successful-looking commit
    /// that actually dropped account/code/storage updates).
    pub fn put_object(space: &ObjectSpace, key: &[u8], obj: &[u8]) {
        // put_object_arguments { object_space space = 1; bytes key = 2; bytes obj = 3; }
        let space_bytes = encode_object_space(space);
        let mut args = Vec::new();
        proto::encode_submessage_field(&mut args, 1, &space_bytes);
        proto::encode_bytes_field(&mut args, 2, key);
        proto::encode_bytes_field(&mut args, 3, obj);
        call_system_must(SC_PUT_OBJECT, &args);
    }

    /// Get an object from state. Returns None if not found.
    pub fn get_object(space: &ObjectSpace, key: &[u8]) -> Option<Vec<u8>> {
        // get_object_arguments { object_space space = 1; bytes key = 2; }
        let space_bytes = encode_object_space(space);
        let mut args = Vec::new();
        proto::encode_submessage_field(&mut args, 1, &space_bytes);
        proto::encode_bytes_field(&mut args, 2, key);

        let result = call_system(SC_GET_OBJECT, &args);
        if result.is_empty() {
            return None;
        }

        // get_object_result { database_object value = 1; }
        // database_object { bool exists = 1; bytes value = 2; bytes key = 3; }
        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1
                && let Some(submsg) = proto::get_bytes(&field_val)
            {
                let mut exists = false;
                let mut value = Vec::new();
                for (sub_field, sub_val) in proto::FieldIter::new(submsg) {
                    match sub_field {
                        1 => {
                            if let Some(v) = proto::get_varint(&sub_val) {
                                exists = v != 0;
                            }
                        }
                        2 => {
                            if let Some(v) = proto::get_bytes(&sub_val) {
                                value = v.to_vec();
                            }
                        }
                        _ => {}
                    }
                }
                if exists {
                    return Some(value);
                }
            }
        }
        None
    }

    /// Remove an object from state. Aborts on syscall failure (see `put_object`).
    #[cfg_attr(not(feature = "evm"), allow(dead_code))]
    pub fn remove_object(space: &ObjectSpace, key: &[u8]) {
        // remove_object_arguments { object_space space = 1; bytes key = 2; }
        let space_bytes = encode_object_space(space);
        let mut args = Vec::new();
        proto::encode_submessage_field(&mut args, 1, &space_bytes);
        proto::encode_bytes_field(&mut args, 2, key);
        call_system_must(SC_REMOVE_OBJECT, &args);
    }

    // ── Cryptography ─────────────────────────────────────────────────

    /// Hash data using the specified algorithm. Returns the multihash result.
    #[cfg_attr(not(feature = "evm"), allow(dead_code))]
    pub fn hash(code: u64, data: &[u8]) -> Vec<u8> {
        // hash_arguments { uint64 code = 1; bytes obj = 2; uint64 size = 3; }
        let mut args = Vec::new();
        proto::encode_varint_field(&mut args, 1, code);
        proto::encode_bytes_field(&mut args, 2, data);
        // size = 0 (default, hash all data)

        let result = call_system_small(SC_HASH, &args);

        // hash_result { bytes value = 1; }
        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1
                && let Some(v) = proto::get_bytes(&field_val)
            {
                return v.to_vec();
            }
        }
        Vec::new()
    }

    /// Recover a public key from a signature and digest.
    #[cfg_attr(not(feature = "evm"), allow(dead_code))]
    pub fn recover_public_key(
        dsa_type: u64,
        signature: &[u8],
        digest: &[u8],
        compressed: bool,
    ) -> Vec<u8> {
        // recover_public_key_arguments { dsa type = 1; bytes signature = 2; bytes digest = 3; bool compressed = 4; }
        let mut args = Vec::new();
        proto::encode_varint_field(&mut args, 1, dsa_type);
        proto::encode_bytes_field(&mut args, 2, signature);
        proto::encode_bytes_field(&mut args, 3, digest);
        proto::encode_bool_field(&mut args, 4, compressed);

        let result = call_system_small(SC_RECOVER_PUBLIC_KEY, &args);

        // recover_public_key_result { bytes value = 1; }
        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1
                && let Some(v) = proto::get_bytes(&field_val)
            {
                return v.to_vec();
            }
        }
        Vec::new()
    }

    // ── Contract management ──────────────────────────────────────────

    /// Call another contract.
    /// Not used yet — kept as API surface for future Koinos↔EVM contract bridging.
    #[allow(dead_code)]
    pub fn call_contract(contract_id: &[u8], entry_point: u32, args_data: &[u8]) -> Vec<u8> {
        // call_arguments { bytes contract_id = 1; uint32 entry_point = 2; bytes args = 3; }
        let mut args = Vec::new();
        proto::encode_bytes_field(&mut args, 1, contract_id);
        proto::encode_varint_field(&mut args, 2, entry_point as u64);
        proto::encode_bytes_field(&mut args, 3, args_data);

        let result = call_system(SC_CALL, &args);

        // call_result { bytes value = 1; }
        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1
                && let Some(v) = proto::get_bytes(&field_val)
            {
                return v.to_vec();
            }
        }
        Vec::new()
    }

    /// Get the current contract's ID.
    pub fn get_contract_id() -> Vec<u8> {
        let result = call_system_small(SC_GET_CONTRACT_ID, &[]);

        // get_contract_id_result { bytes value = 1; }
        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1
                && let Some(v) = proto::get_bytes(&field_val)
            {
                return v.to_vec();
            }
        }
        Vec::new()
    }

    /// Get the caller's address.
    /// Not used yet — kept as API surface for future caller-authentication work.
    #[allow(dead_code)]
    pub fn get_caller() -> (Vec<u8>, u32) {
        let result = call_system_small(SC_GET_CALLER, &[]);

        // get_caller_result { caller_data value = 1; }
        // caller_data { bytes caller = 1; privilege caller_privilege = 2; }
        let mut caller = Vec::new();
        let mut privilege: u32 = 1; // default: user_mode

        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1
                && let Some(submsg) = proto::get_bytes(&field_val)
            {
                for (sub_field, sub_val) in proto::FieldIter::new(submsg) {
                    match sub_field {
                        1 => {
                            if let Some(v) = proto::get_bytes(&sub_val) {
                                caller = v.to_vec();
                            }
                        }
                        2 => {
                            if let Some(v) = proto::get_varint(&sub_val) {
                                privilege = v as u32;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        (caller, privilege)
    }

    // ── Block/transaction info ───────────────────────────────────────

    /// Head info from the chain.
    #[cfg_attr(not(feature = "evm"), allow(dead_code))]
    pub struct HeadInfo {
        pub height: u64,
        pub head_block_time: u64,
    }

    /// Get current head block info.
    #[cfg_attr(not(feature = "evm"), allow(dead_code))]
    pub fn get_head_info() -> HeadInfo {
        let result = call_system_small(SC_GET_HEAD_INFO, &[]);

        // get_head_info_result { head_info value = 1; }
        // head_info { block_topology head_topology = 1; uint64 head_block_time = 2; uint64 last_irreversible_block = 3; }
        // block_topology { bytes id = 1; uint64 height = 2; bytes previous = 3; }
        let mut height: u64 = 0;
        let mut head_block_time: u64 = 0;

        for (field_num, field_val) in proto::FieldIter::new(&result) {
            if field_num == 1 {
                // get_head_info_result.value -> head_info submessage
                if let Some(head_info) = proto::get_bytes(&field_val) {
                    for (hi_field, hi_val) in proto::FieldIter::new(head_info) {
                        match hi_field {
                            1 => {
                                // head_topology submessage
                                if let Some(topo) = proto::get_bytes(&hi_val) {
                                    for (t_field, t_val) in proto::FieldIter::new(topo) {
                                        if t_field == 2
                                            && let Some(v) = proto::get_varint(&t_val)
                                        {
                                            height = v;
                                        }
                                    }
                                }
                            }
                            2 => {
                                // head_block_time (field 2, NOT 4)
                                if let Some(v) = proto::get_varint(&hi_val) {
                                    head_block_time = v;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        HeadInfo {
            height,
            head_block_time,
        }
    }
}
