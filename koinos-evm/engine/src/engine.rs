//! EVM execution engine - ties together revm, the Koinos database, and precompiles.

use alloc::sync::Arc;
use alloc::vec::Vec;

use revm::precompile::{Precompile, PrecompileWithAddress};
use revm::primitives::{
    specification::SpecId, Address, BlockEnv, Bytes, CfgEnv, CfgEnvWithHandlerCfg, EVMError,
    ExecutionResult, Log, Output, ResultAndState, TxEnv, TxKind, U256,
};
use revm::EvmBuilder;

use crate::database::{KoinosDatabase, KoinosDbError};
use crate::koinos::sys;
use crate::precompiles;
use crate::proto;
use crate::tx;

/// Chain ID for the Koinos EVM engine. Used by both CfgEnv and to verify
/// inbound raw transactions are for THIS chain (replay protection).
/// Configurable in a future phase via a config-space entry.
const ENGINE_CHAIN_ID: u64 = 42069;

// ── Request/response protobuf encoding ───────────────────────────────────
//
// Execute/CallView args:
//   message EvmCallArgs {
//     bytes caller = 1;      // 20 bytes
//     bytes to = 2;          // 20 bytes (empty = CREATE)
//     bytes value = 3;       // 32 bytes, big-endian U256
//     bytes data = 4;        // calldata
//     uint64 gas_limit = 5;
//   }

struct EvmCallArgs {
    caller: [u8; 20],
    to: Option<[u8; 20]>,
    value: U256,
    data: Vec<u8>,
    gas_limit: u64,
}

fn parse_call_args(args: &[u8]) -> EvmCallArgs {
    let mut caller = [0u8; 20];
    let mut to: Option<[u8; 20]> = None;
    let mut value = U256::ZERO;
    let mut data = Vec::new();
    let mut gas_limit: u64 = 30_000_000; // default gas limit

    for (field_num, field_val) in proto::FieldIter::new(args) {
        match field_num {
            1 => {
                if let Some(bytes) = proto::get_bytes(&field_val) {
                    if bytes.len() >= 20 {
                        caller.copy_from_slice(&bytes[..20]);
                    }
                }
            }
            2 => {
                if let Some(bytes) = proto::get_bytes(&field_val) {
                    if bytes.len() >= 20 {
                        let mut addr = [0u8; 20];
                        addr.copy_from_slice(&bytes[..20]);
                        to = Some(addr);
                    }
                }
            }
            3 => {
                if let Some(bytes) = proto::get_bytes(&field_val) {
                    if bytes.len() == 32 {
                        value = U256::from_be_slice(bytes);
                    }
                }
            }
            4 => {
                if let Some(bytes) = proto::get_bytes(&field_val) {
                    data = bytes.to_vec();
                }
            }
            5 => {
                if let Some(v) = proto::get_varint(&field_val) {
                    gas_limit = v;
                }
            }
            _ => {}
        }
    }

    EvmCallArgs {
        caller,
        to,
        value,
        data,
        gas_limit,
    }
}

// ── Execution result encoding ────────────────────────────────────────────
//
// message EvmResult {
//   bool success = 1;
//   bytes output = 2;       // return data or revert reason
//   uint64 gas_used = 3;
//   bytes contract_address = 4;  // set on CREATE
// }

fn encode_evm_result(
    success: bool,
    output: &[u8],
    gas_used: u64,
    contract_address: Option<&[u8; 20]>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(output.len() + 40);
    proto::encode_bool_field(&mut buf, 1, success);
    proto::encode_bytes_field(&mut buf, 2, output);
    proto::encode_varint_field(&mut buf, 3, gas_used);
    if let Some(addr) = contract_address {
        proto::encode_bytes_field(&mut buf, 4, addr);
    }
    buf
}

// ── Block environment ────────────────────────────────────────────────────

fn build_block_env() -> BlockEnv {
    let head = sys::get_head_info();
    let mut env = BlockEnv::default();
    env.number = U256::from(head.height);
    env.timestamp = U256::from(head.head_block_time / 1000); // ms to seconds
    env.basefee = U256::ZERO; // Koinos is feeless
    env.gas_limit = U256::from(30_000_000u64);
    env.coinbase = Address::ZERO;
    env.difficulty = U256::ZERO;
    env
}

fn build_cfg_env() -> CfgEnv {
    let mut cfg = CfgEnv::default();
    cfg.chain_id = ENGINE_CHAIN_ID;
    cfg
}

// ── EVM execution ────────────────────────────────────────────────────────

fn execute_evm(
    args: &EvmCallArgs,
    commit: bool,
) -> Result<ExecutionResult, EVMError<KoinosDbError>> {
    let db = KoinosDatabase::new();

    let tx_env = TxEnv {
        caller: Address::from_slice(&args.caller),
        gas_limit: args.gas_limit,
        gas_price: U256::ZERO,
        transact_to: match args.to {
            Some(addr) => TxKind::Call(Address::from_slice(&addr)),
            None => TxKind::Create,
        },
        value: args.value,
        data: Bytes::copy_from_slice(&args.data),
        ..Default::default()
    };

    let cfg_with_handler = CfgEnvWithHandlerCfg::new_with_spec_id(
        build_cfg_env(),
        SpecId::CANCUN,
    );

    let mut evm = EvmBuilder::default()
        .with_db(db)
        .with_block_env(build_block_env())
        .with_cfg_env_with_handler_cfg(cfg_with_handler)
        .with_tx_env(tx_env)
        // Override default precompile registry with Koinos-syscall-backed implementations
        // for addresses 0x01-0x04 (ecRecover, SHA-256, RIPEMD-160, identity).
        // These delegate to native chain syscalls, avoiding double-interpretation overhead.
        .append_handler_register(|handler| {
            let prev = handler.pre_execution.load_precompiles();
            handler.pre_execution.load_precompiles = Arc::new(move || {
                let mut p = prev.clone();
                p.extend([
                    PrecompileWithAddress(
                        precompiles::ECRECOVER_ADDR,
                        Precompile::Standard(precompiles::ec_recover),
                    ),
                    PrecompileWithAddress(
                        precompiles::SHA256_ADDR,
                        Precompile::Standard(precompiles::sha256_run),
                    ),
                    PrecompileWithAddress(
                        precompiles::RIPEMD160_ADDR,
                        Precompile::Standard(precompiles::ripemd160_run),
                    ),
                    PrecompileWithAddress(
                        precompiles::IDENTITY_ADDR,
                        Precompile::Standard(precompiles::identity_run),
                    ),
                ]);
                p
            });
        })
        .build();

    if commit {
        let result = evm.transact_commit()?;
        Ok(result)
    } else {
        let ResultAndState { result, state: _ } = evm.transact()?;
        Ok(result)
    }
}

/// Route revm-emitted EVM logs to Koinos events so they appear in receipts
/// and can be reconstructed by eth_getLogs at the JSON-RPC layer.
///
/// Wire format per log (protobuf-style):
///   EvmLog { bytes address = 1; repeated bytes topics = 2; bytes data = 3; }
///
/// Koinos event:
///   name      = "evm.log"
///   data      = EvmLog protobuf bytes
///   impacted  = [address, topic_0, topic_1, ...] — enables on-chain indexing
fn emit_logs(logs: &[Log]) {
    for log in logs {
        let topics = log.topics();

        let mut data = Vec::with_capacity(64 + log.data.data.len() + 36 * topics.len());
        proto::encode_bytes_field(&mut data, 1, log.address.as_slice());
        for topic in topics {
            proto::encode_bytes_field(&mut data, 2, topic.as_slice());
        }
        proto::encode_bytes_field(&mut data, 3, log.data.data.as_ref());

        let mut impacted: Vec<&[u8]> = Vec::with_capacity(1 + topics.len());
        impacted.push(log.address.as_slice());
        for topic in topics {
            impacted.push(topic.as_slice());
        }

        sys::event("evm.log", &data, &impacted);
    }
}

/// Emit an `evm.result` event so the JSON-RPC proxy can reconstruct an Ethereum-style receipt
/// (status + gas_used + contract_address). Koinos does NOT persist contract return bytes in
/// the indexed receipt, so we have to surface this via a dedicated event.
///
/// Wire format (proto):
///   EvmResultEvent { bool success=1, uint64 gas_used=2, bytes contract_address=3 (optional, CREATE) }
///
/// Field 1 (success) is written EXPLICITLY — even when false — instead of via
/// `encode_bool_field`. proto3 encoders (incl. ours, proto.rs) omit default values
/// (`false`/`0`), but the proxy receipt decoder (rpc.rs:708/724) defaults status to
/// `0x1` and only sets `0x0` when it actually SEES field 1 == 0. If we let the encoder
/// drop the field on failure, the event payload is empty and EVERY failed tx
/// (revert / halt / nonce-reject / parse-reject) decodes as success. So we emit the
/// tag+varint directly. Success is byte-identical to the old path (field 1 = varint 1).
fn emit_evm_result_event(success: bool, gas_used: u64, contract_address: Option<&[u8; 20]>) {
    let mut data = Vec::with_capacity(48);
    proto::encode_tag(&mut data, 1, proto::WIRE_VARINT);
    proto::encode_varint(&mut data, if success { 1 } else { 0 });
    proto::encode_varint_field(&mut data, 2, gas_used);
    if let Some(addr) = contract_address {
        proto::encode_bytes_field(&mut data, 3, addr);
    }
    let impacted: [&[u8]; 0] = [];
    sys::event("evm.result", &data, &impacted);
}

/// Reject a relayed (committing) transaction. Emits an `evm.result(success=false)`
/// event BEFORE returning the failure bytes — without it the Koinos call still
/// commits successfully (we return normally, not via exit_error), the receipt carries
/// no `evm.result` event, and the proxy's `eth_getTransactionReceipt` defaults status
/// to 0x1 → a rejected tx (bad nonce / parse error / chain-id mismatch) would report
/// as SUCCESS to MetaMask. gas_used is 0 because no EVM execution occurred.
///
/// Emits a Koinos event, so it must run in a committing context. In production this
/// holds because EP_SUBMIT_RAW_TX is only reached via chain.submit_transaction (the
/// proxy never relays it through read_contract — rpc.rs:363/395). The CONTRACT does not
/// itself forbid a direct chain.read_contract to entry_point 7; if someone did that,
/// sys::event would fail and call_system_must would abort the read harmlessly (no state,
/// no misleading receipt). Do not reuse this helper on the read-only call_view path.
fn reject_submit(msg: &[u8]) -> Vec<u8> {
    emit_evm_result_event(false, 0, None);
    encode_evm_result(false, msg, 0, None)
}

/// `emit_logs_if_committing` controls whether revm logs are forwarded as Koinos events.
/// Must be FALSE for read-only paths (eth_call / `handle_call_view`):
///   1. Koinos's chain.read_contract blocks state-changing syscalls, so sys::event
///      would fail and (with our hardened wrapper) abort the entire call.
///   2. Even on a node that allows it, emitting events from a simulation pollutes
///      receipts with phantom events that never actually occurred.
///
/// When committing, we also emit an `evm.result` event so the JSON-RPC proxy can build
/// Ethereum receipts with proper status/gas_used/contract_address.
fn execution_result_to_response(result: ExecutionResult, emit_logs_if_committing: bool) -> Vec<u8> {
    match result {
        ExecutionResult::Success {
            output,
            gas_used,
            logs,
            ..
        } => {
            if emit_logs_if_committing {
                emit_logs(&logs);
            }

            let (output_bytes, contract_addr) = match output {
                Output::Call(data) => (data.to_vec(), None),
                Output::Create(data, addr) => {
                    let addr_bytes = addr.map(|a| {
                        let mut buf = [0u8; 20];
                        buf.copy_from_slice(a.as_slice());
                        buf
                    });
                    (data.to_vec(), addr_bytes)
                }
            };

            if emit_logs_if_committing {
                emit_evm_result_event(true, gas_used, contract_addr.as_ref());
            }

            encode_evm_result(true, &output_bytes, gas_used, contract_addr.as_ref())
        }
        ExecutionResult::Revert { output, gas_used } => {
            if emit_logs_if_committing {
                emit_evm_result_event(false, gas_used, None);
            }
            encode_evm_result(false, &output, gas_used, None)
        }
        ExecutionResult::Halt { gas_used, .. } => {
            if emit_logs_if_committing {
                emit_evm_result_event(false, gas_used, None);
            }
            encode_evm_result(false, b"execution halted", gas_used, None)
        }
    }
}

// ── Entry point handlers ─────────────────────────────────────────────────

/// Execute a state-changing EVM transaction.
pub fn handle_execute(args: &[u8]) -> Vec<u8> {
    let call_args = parse_call_args(args);

    match execute_evm(&call_args, true) {
        Ok(result) => execution_result_to_response(result, true),
        Err(e) => {
            let msg = alloc::format!("EVM error: {:?}", e);
            sys::log(&msg);
            encode_evm_result(false, msg.as_bytes(), 0, None)
        }
    }
}

/// Execute a read-only EVM call (eth_call equivalent).
/// Logs are NOT emitted (see `execution_result_to_response`); read_contract context
/// disallows state-changing syscalls anyway.
pub fn handle_call_view(args: &[u8]) -> Vec<u8> {
    let call_args = parse_call_args(args);

    match execute_evm(&call_args, false) {
        Ok(result) => execution_result_to_response(result, false),
        Err(e) => {
            let msg = alloc::format!("EVM error: {:?}", e);
            sys::log(&msg);
            encode_evm_result(false, msg.as_bytes(), 0, None)
        }
    }
}

/// Deploy EVM contract bytecode (CREATE).
pub fn handle_deploy_code(args: &[u8]) -> Vec<u8> {
    // Deploy is just an execute with to=None
    let mut call_args = parse_call_args(args);
    call_args.to = None; // Force CREATE mode
    // The data field contains the init code

    match execute_evm(&call_args, true) {
        Ok(result) => execution_result_to_response(result, true),
        Err(e) => {
            let msg = alloc::format!("EVM deploy error: {:?}", e);
            sys::log(&msg);
            encode_evm_result(false, msg.as_bytes(), 0, None)
        }
    }
}

/// Get EVM account info.
pub fn handle_get_account(args: &[u8]) -> Vec<u8> {
    // args: just the 20-byte address
    if args.len() < 20 {
        return Vec::new();
    }

    let mut db = KoinosDatabase::new();
    let addr = Address::from_slice(&args[..20]);

    match revm::Database::basic(&mut db, addr) {
        Ok(Some(info)) => {
            let mut buf = Vec::new();
            proto::encode_varint_field(&mut buf, 1, info.nonce);
            let balance_bytes = info.balance.to_be_bytes::<32>();
            proto::encode_bytes_field(&mut buf, 2, &balance_bytes);
            proto::encode_bytes_field(&mut buf, 3, info.code_hash.as_slice());
            buf
        }
        Ok(None) => Vec::new(),
        Err(_) => Vec::new(),
    }
}

/// Get EVM storage value at a slot.
pub fn handle_get_storage_at(args: &[u8]) -> Vec<u8> {
    // args: address[20] || slot[32]
    if args.len() < 52 {
        return Vec::new();
    }

    let mut db = KoinosDatabase::new();
    let addr = Address::from_slice(&args[..20]);
    let slot = U256::from_be_slice(&args[20..52]);

    match revm::Database::storage(&mut db, addr, slot) {
        Ok(value) => value.to_be_bytes::<32>().to_vec(),
        Err(_) => [0u8; 32].to_vec(),
    }
}

/// Get EVM contract bytecode.
pub fn handle_get_code(args: &[u8]) -> Vec<u8> {
    // args: 20-byte address
    if args.len() < 20 {
        return Vec::new();
    }

    let mut db = KoinosDatabase::new();
    let addr = Address::from_slice(&args[..20]);

    match revm::Database::basic(&mut db, addr) {
        Ok(Some(info)) => match revm::Database::code_by_hash(&mut db, info.code_hash) {
            Ok(code) => code.original_bytes().to_vec(),
            Err(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Accept a signed Ethereum raw transaction.
///
/// Args layout: protobuf with field 1 = raw_tx bytes
///   message SubmitRawTxArgs { bytes raw_tx = 1; }
///
/// Flow:
///   1. Strip protobuf wrapper to get raw_tx bytes
///   2. tx::parse_raw_tx → ParsedTx (incl. recovered sender)
///   3. Enforce chain_id matches ENGINE_CHAIN_ID
///   4. Enforce nonce matches sender's account nonce
///   5. Build TxEnv with zero gas_price (Koinos mana pays)
///   6. Call execute_evm with commit=true
pub fn handle_submit_raw_tx(args: &[u8]) -> Vec<u8> {
    // Strip protobuf wrapper
    let mut raw_tx_bytes: Vec<u8> = Vec::new();
    for (field_num, field_val) in proto::FieldIter::new(args) {
        if field_num == 1 {
            if let Some(b) = proto::get_bytes(&field_val) {
                raw_tx_bytes = b.to_vec();
            }
        }
    }
    if raw_tx_bytes.is_empty() {
        return reject_submit(b"empty raw_tx");
    }

    // Parse + recover sender
    let parsed = match tx::parse_raw_tx(&raw_tx_bytes) {
        Ok(p) => p,
        Err(e) => {
            let msg = alloc::format!("raw_tx parse error: {:?}", e);
            sys::log(&msg);
            return reject_submit(msg.as_bytes());
        }
    };

    // Chain ID check: parser rejects pre-EIP-155 entirely, so parsed.chain_id is
    // always Some(_). Defensive: only enforce if Some(_) — but mismatch is fatal.
    if let Some(cid) = parsed.chain_id {
        if cid != ENGINE_CHAIN_ID {
            let msg = alloc::format!(
                "chain_id mismatch: tx={} expected={}",
                cid, ENGINE_CHAIN_ID
            );
            sys::log(&msg);
            return reject_submit(msg.as_bytes());
        }
    }

    // Nonce check against on-chain account state
    let mut db = KoinosDatabase::new();
    let expected_nonce = match revm::Database::basic(&mut db, parsed.sender) {
        Ok(Some(info)) => info.nonce,
        Ok(None) => 0,
        Err(_) => {
            return reject_submit(b"db basic read failed");
        }
    };
    if parsed.nonce != expected_nonce {
        let msg = alloc::format!(
            "nonce mismatch: tx={} expected={}",
            parsed.nonce, expected_nonce
        );
        sys::log(&msg);
        return reject_submit(msg.as_bytes());
    }

    // Build TxEnv directly (bypass parse_call_args)
    let tx_env = TxEnv {
        caller: parsed.sender,
        gas_limit: parsed.gas_limit,
        // Zero-fee policy: Koinos mana pays execution. Sender ETH balance need not
        // cover gas_price * gas_limit. Means contracts that read tx.gasprice see 0.
        gas_price: U256::ZERO,
        gas_priority_fee: None,
        transact_to: match parsed.to {
            Some(addr) => TxKind::Call(addr),
            None => TxKind::Create,
        },
        value: parsed.value,
        data: Bytes::copy_from_slice(&parsed.data),
        nonce: Some(parsed.nonce),
        chain_id: parsed.chain_id,
        ..Default::default()
    };

    let cfg_with_handler =
        CfgEnvWithHandlerCfg::new_with_spec_id(build_cfg_env(), SpecId::CANCUN);

    let mut evm = EvmBuilder::default()
        .with_db(KoinosDatabase::new())
        .with_block_env(build_block_env())
        .with_cfg_env_with_handler_cfg(cfg_with_handler)
        .with_tx_env(tx_env)
        .append_handler_register(|handler| {
            let prev = handler.pre_execution.load_precompiles();
            handler.pre_execution.load_precompiles = Arc::new(move || {
                let mut p = prev.clone();
                p.extend([
                    PrecompileWithAddress(
                        precompiles::ECRECOVER_ADDR,
                        Precompile::Standard(precompiles::ec_recover),
                    ),
                    PrecompileWithAddress(
                        precompiles::SHA256_ADDR,
                        Precompile::Standard(precompiles::sha256_run),
                    ),
                    PrecompileWithAddress(
                        precompiles::RIPEMD160_ADDR,
                        Precompile::Standard(precompiles::ripemd160_run),
                    ),
                    PrecompileWithAddress(
                        precompiles::IDENTITY_ADDR,
                        Precompile::Standard(precompiles::identity_run),
                    ),
                ]);
                p
            });
        })
        .build();

    match evm.transact_commit() {
        Ok(result) => execution_result_to_response(result, true),
        Err(e) => {
            let msg = alloc::format!("EVM error: {:?}", e);
            sys::log(&msg);
            reject_submit(msg.as_bytes())
        }
    }
}
