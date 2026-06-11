// no_std/no_main/dlmalloc/panic_handler are the production (wasm32) configuration;
// on non-wasm targets (host unit tests / clippy with `host-crypto`) the crate
// compiles against std so the libtest harness can run. All cfg conditions below
// evaluate exactly as before on wasm32 — the WASM artifact is unchanged.
#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

extern crate alloc;
#[cfg(target_arch = "wasm32")]
extern crate dlmalloc;

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

mod koinos;
mod proto;
mod state;

#[cfg(feature = "evm")]
mod database;
#[cfg(feature = "evm")]
mod engine;
#[cfg(feature = "evm")]
mod precompiles;
// The tx parser also compiles under `host-crypto` (without `evm`) so its
// consensus-critical decode/recover logic is host-testable. Without `evm`
// nothing in the engine references it, hence the dead_code allowance.
#[cfg(any(feature = "evm", feature = "host-crypto"))]
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
mod tx;

use alloc::vec::Vec;
use koinos::sys;

// Entry point IDs. Only referenced from `cfg(feature = "evm")` match arms in
// `_start` (dev_unsafe_caller implies evm), hence dead in featureless builds.
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_EXECUTE: u32 = 0x00000001;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_CALL_VIEW: u32 = 0x00000002;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_DEPLOY_CODE: u32 = 0x00000003;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_GET_ACCOUNT: u32 = 0x00000004;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_GET_STORAGE_AT: u32 = 0x00000005;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_GET_CODE: u32 = 0x00000006;
#[cfg_attr(not(feature = "evm"), allow(dead_code))]
const EP_SUBMIT_RAW_TX: u32 = 0x00000007;

// Simple test entry points (Phase 0)
const EP_STORE_VALUE: u32 = 0x10000001;
const EP_READ_VALUE: u32 = 0x10000002;
const EP_ECHO: u32 = 0x10000003;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let args = sys::get_arguments();

    let result = match args.entry_point {
        EP_STORE_VALUE => handle_store_value(&args.arguments),
        EP_READ_VALUE => handle_read_value(&args.arguments),
        EP_ECHO => handle_echo(&args.arguments),

        // Authenticated paths:
        #[cfg(feature = "evm")]
        EP_SUBMIT_RAW_TX => engine::handle_submit_raw_tx(&args.arguments),
        #[cfg(feature = "evm")]
        EP_CALL_VIEW => engine::handle_call_view(&args.arguments),
        #[cfg(feature = "evm")]
        EP_GET_ACCOUNT => engine::handle_get_account(&args.arguments),
        #[cfg(feature = "evm")]
        EP_GET_STORAGE_AT => engine::handle_get_storage_at(&args.arguments),
        #[cfg(feature = "evm")]
        EP_GET_CODE => engine::handle_get_code(&args.arguments),

        // Spoofing-vulnerable paths (caller taken from protobuf args without auth).
        // Only routed when `dev_unsafe_caller` feature is enabled. Default builds reject these.
        #[cfg(feature = "dev_unsafe_caller")]
        EP_EXECUTE => engine::handle_execute(&args.arguments),
        #[cfg(feature = "dev_unsafe_caller")]
        EP_DEPLOY_CODE => engine::handle_deploy_code(&args.arguments),

        #[cfg(all(feature = "evm", not(feature = "dev_unsafe_caller")))]
        EP_EXECUTE | EP_DEPLOY_CODE => {
            sys::log("execute/deploy_code disabled: use submit_raw_tx");
            sys::exit_error(b"use submit_raw_tx");
            unreachable!()
        }

        _ => {
            sys::log("unknown entry point");
            sys::exit_error(b"unknown entry point");
            unreachable!()
        }
    };

    sys::exit_success(&result);
}

/// Phase 0 test: store a value in object space
fn handle_store_value(args: &[u8]) -> Vec<u8> {
    // Args format: key_len(4 LE) + key + value
    if args.len() < 5 {
        sys::exit_error(b"store_value: args too short");
        unreachable!()
    }
    let key_len = u32::from_le_bytes([args[0], args[1], args[2], args[3]]) as usize;
    let end = match 4usize.checked_add(key_len) {
        Some(e) if e <= args.len() => e,
        _ => {
            sys::log("store_value: invalid key length");
            sys::exit_error(b"invalid key length");
            unreachable!()
        }
    };
    let key = &args[4..end];
    let value = &args[end..];

    let space = state::test_space();
    sys::put_object(&space, key, value);
    sys::log("store_value: success");
    Vec::new()
}

/// Phase 0 test: read a value from object space
fn handle_read_value(args: &[u8]) -> Vec<u8> {
    // Args: raw key bytes
    let space = state::test_space();
    match sys::get_object(&space, args) {
        Some(value) => {
            sys::log("read_value: found");
            value
        }
        None => {
            sys::log("read_value: not found");
            Vec::new()
        }
    }
}

/// Phase 0 test: echo back the arguments
fn handle_echo(args: &[u8]) -> Vec<u8> {
    sys::log("echo: returning args");
    args.to_vec()
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys::log("PANIC");
    sys::exit_error(b"panic");
    // exit_error calls exit which doesn't return, but compiler needs this
    loop {}
}
