# Architecture

Three layers: the **engine** (an EVM as one Koinos contract), the **proxy** (Ethereum JSON-RPC ↔ Koinos),
and the **apps** (unmodified Solidity). The pattern is the same one Aurora uses on NEAR: *one contract
is the EVM*, and a relayer pays the host-chain resource for users.

## 1. The engine — `koinos-evm/engine/`

[`revm`](https://github.com/bluealloy/revm) compiled to **MVP WebAssembly** (the `wasm32v1-none` target,
`wasm-opt -Oz --mvp-features`, verified opcode-clean) and deployed as a single ~407 KB Koinos contract.

| File | Responsibility |
|---|---|
| `lib.rs` | `_start` entry point; dispatch by Koinos `entry_point` id |
| `engine.rs` | `handle_submit_raw_tx` (the committing tx path), `handle_call_view` (read-only), `emit_logs`, `emit_evm_result_event` |
| `tx.rs` | RLP decode of legacy / EIP-155 / EIP-1559 txs + sender recovery (enforces low-s, chain id) |
| `database.rs` | revm `Database`/`DatabaseCommit` backed by Koinos `get_object`/`put_object` |
| `precompiles.rs` | `ecRecover`, SHA-256, RIPEMD-160, identity — delegating to native Koinos crypto syscalls |
| `state.rs` | object-space layout (accounts / code / storage / config / nonces) |
| `koinos.rs` | syscall FFI; `call_system_must` aborts the tx if a state syscall fails |
| `proto.rs` | minimal `no_std` protobuf codec |

**Entry points.** The only state-changing path enabled in production is `submit_raw_tx` (entry point 7):
the sender is recovered from the signature via `ecrecover` — never taken from caller-spoofable arguments.
The spoofable `execute`/`deploy_code` paths are feature-gated **off** by default. Reads use light entry
points (`get_account`, `get_code`, `get_storage_at`, `call_view`).

**State.** All EVM accounts, code, and storage live in the engine contract's Koinos KV object space
(`database.rs` ↔ `state.rs`). Upgrading the engine WASM does **not** touch this state — it has been
upgraded in place with all deployed Uniswap state preserved.

**Receipts.** Koinos doesn't persist EVM return data in its indexed receipt, so the engine emits an
`evm.result` event (`success`, `gas_used`, `contract_address`) and `evm.log` events; the proxy
reconstructs Ethereum receipts/logs from those. (The `success` field is written *explicitly* — proto3
default-skipping would otherwise drop `success=false` and make failures look successful; see
[STATUS.md](STATUS.md).)

## 2. The proxy — `koinos-evm/rpc/`

A ~1500-LOC Rust JSON-RPC server (axum). It accepts standard Ethereum JSON-RPC, and for writes it
decodes the user's signed raw tx, re-wraps it as a Koinos `call_contract(engine, entry_point=7, raw_tx)`,
signs that with the **operator** key, and submits it — paying Koinos mana so the EVM user pays nothing.

| File | Responsibility |
|---|---|
| `main.rs` | HTTP server + permissive CORS |
| `rpc.rs` | method dispatch + receipt/tx/block reconstruction |
| `eth_tx.rs` / `eth_codec.rs` | decode + recover the user's raw Eth tx; hex/quantity helpers |
| `koinos_tx.rs` | build + sign the relayed Koinos tx (the tricky base58check / protobuf-header encoding) |
| `koinos.rs` | Koinos chain RPC client (`read_contract`, `submit_transaction`) |
| `state.rs` | config (env), in-memory `tx_meta`, operator-nonce gate, per-sender pending-nonce map |

**Implemented methods (18):** `eth_chainId`, `net_version`, `web3_clientVersion`, `eth_blockNumber`,
`eth_gasPrice`, `eth_maxPriorityFeePerGas`, `eth_feeHistory`, `eth_getBalance`,
`eth_getTransactionCount`, `eth_getCode`, `eth_getStorageAt`, `eth_call`, `eth_estimateGas`,
`eth_sendRawTransaction`, `eth_getTransactionReceipt`, `eth_getTransactionByHash`,
`eth_getBlockByNumber`, `eth_getBlockByHash`.

**Config (env):** `OPERATOR_PRIVKEY_HEX` (required), `ENGINE_CONTRACT`, `LISTEN_ADDR`,
`RC_LIMIT_MANA`, `KOINOS_RPC_URL`/`KOINOS_REST_URL` (default to the foundation testnet),
`EVM_CHAIN_ID` (42069), `KOINOS_CHAIN_ID`.

## 3. The apps — `scripts/forge/` + `scripts/shell/`

Unmodified mainnet Solidity, built with Foundry under several profiles (`uniswap` for V2 at
`evm_version=istanbul`/`runs=999999`; `v3`/`v3p`/`v3pn` for V3 core/periphery/NFPM under solc 0.7.6),
then deployed through the proxy by the `scripts/shell/deploy_*.sh` scripts. The deploy scripts are
self-asserting: every expected value is captured once from an `anvil` reference running byte-identical
artifacts and baked in as a hard on-chain assertion. See [TESTING.md](TESTING.md).

## Known semantic differences from Ethereum

Intentional / intrinsic (documented, fine for Uniswap; relevant for some other protocols):

- `tx.gasprice`, `block.basefee`, `block.coinbase`, `block.difficulty` are **0**; `BLOCKHASH` returns 0.
- EVM users pay **no gas** — the operator pays Koinos mana (zero-fee policy).
- Koinos block time ≈ 3 s (vs 12 s) → timelocks fire ~4× faster; ~60-block finality lag.
- Heavy read-only views can exceed the Koinos node's `read-compute-bandwidth-limit` (a per-node config,
  default 10M) and revert `-1013`; see [STATUS.md](STATUS.md) (this is the single biggest usability gap).
- EIP-2930 access lists, EIP-4844 blobs, and EIP-7702 are rejected by the tx parser.
