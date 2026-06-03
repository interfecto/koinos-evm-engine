# Status — what's proven, what isn't

This project is a **proof of concept**: a rigorously-tested EVM *execution core*, plus a thin relay and
demo UIs. It is **not** a production network. This page is deliberately calibrated so nobody over-claims.

## ✅ Proven (byte-exact vs a reference EVM, live on the foundation testnet)

"Byte-exact" means: run against an `anvil` reference loaded with byte-identical compiled artifacts, and
every exercised result matched **to the wei / to the last bit**, on-chain.

- **Uniswap V2** — CREATE2 pair creation, first-mint + `MINIMUM_LIQUIDITY` lock, bidirectional
  constant-product swaps, `removeLiquidity` / k-preservation.
- **Uniswap V3 core** — `createPool` (two fee tiers, canonical `POOL_INIT_CODE_HASH`), `initialize`,
  concentrated-liquidity mint, in-range + **bidirectional tick-crossing** swaps, exact-output swaps,
  price-limit (partial-fill) swaps, `burn`/`collect`, swap-fee accrual (`feeGrowthGlobal`) + per-position
  fee collection, a second fee tier (tickSpacing 10), flash swaps, protocol fees, oracle cardinality.
- **Uniswap V3 periphery** — the *real* `@uniswap/v3-periphery` v1.3.0: `SwapRouter`
  (`exactInputSingle`/`exactOutputSingle`/`exactInput` path), `NonfungiblePositionManager` (NFT-wrapped
  positions), `QuoterV2` (deploy + correct output on the reference).
- **End-to-end via MetaMask** — connect, mint faucet tokens, swap, manage V3 positions, all zero-gas.
- **Engine upgradeability** — the engine WASM was upgraded in place with **all** deployed Uniswap state
  preserved.

Reproduce with `scripts/shell/deploy_uniswap_v3.sh` (phases `l1`–`l8`) and `deploy_v3_periphery.sh`
(phases `p1`–`p4`). See [TESTING.md](TESTING.md).

### Correctness fixes (2026-06-03), each on-chain-verified
1. **Failed transactions reported success.** The `evm.result` event encoded `success` with a proto3
   encoder that *skips* default values, so `success=false` was dropped → every failed tx (revert / halt /
   bad-nonce / parse error) decoded as `status 0x1`. Fixed by writing field 1 explicitly. Verified: a
   bad-nonce tx now returns receipt `status 0x0`; a valid tx returns `0x1`. (`koinos-evm/verify_step1.sh`)
2. **Back-to-back transactions collided on nonce.** `eth_getTransactionCount` ignored the `"pending"`
   block tag, so a wallet sending two txs in a row reused the nonce and the second was rejected. Fixed
   with a per-sender pending-nonce map. Verified: after one send, `pending` advances; both txs land.
   (`koinos-evm/verify_step2.sh`)

## 🚫 Not yet — do **not** claim these

| Claim to avoid | Reality |
|---|---|
| "Production-ready / mainnet-ready" | Testnet PoC only. |
| "Decentralized / trustless" | One operator key relays every tx **and** owns/upgrades the engine. |
| "Economically safe / has a bridge / fee market" | None of these exist yet. |
| "Ethereum-equivalent / standard tooling fully works" | Heavy reads, `eth_getLogs`, and persistence are missing (below). |

## Known gaps (the "system wrapper", not the execution core)

1. **Heavy read views hit a per-node compute limit (`-1013`).** `Quoter`, V3 `positions()`/`ticks()`,
   `getAmountsOut`, oracle `observe()` exceed the Koinos node's `read-compute-bandwidth-limit` (default
   10M). This is a **per-node config flag, not consensus** — the fix is to run a node with the limit
   raised and point the proxy at it. The demo UIs work around it (client-side quotes; positions tracked
   from receipt events). This is the **single biggest blocker** to "standard Ethereum tooling just works."
2. **No persistence.** The proxy keeps tx metadata in memory; a restart breaks receipt/`getTransactionByHash`
   lookups for prior txs. Needs a durable (e.g. SQLite) store.
3. **No `eth_getLogs`.** Not implemented; `logsBloom` is zero. Breaks log indexers / The Graph / wallet history.
4. **Single hot key.** The operator key pays mana for everyone *and* owns the engine account (so it can
   replace the engine). Cheapest high-value hardening: split the relay key from the upgrade authority and
   move upgrades to a multisig/timelock.
5. **No fee market / admission control.** The operator subsidizes every tx; a public endpoint is
   grief-able. `eth_gasPrice`/`feeHistory` are stubs.
6. **No KOIN↔EVM bridge**, no WebSocket subscriptions, no multi-hop routing proof, native-value
   (`swapExactETHForTokens`) paths unsupported.

## Honest one-liner

> A proven EVM execution core running real Uniswap V2/V3 on Koinos — a high-credibility proof of concept
> and a foundation-adoption proposal, with a **bounded** path to a full testnet layer and a larger,
> org-backed path to production. The hard part (revm compatibility) is done; what remains is the trust /
> economic / indexing wrapper around it.

## Roadmap

| Milestone | Effort | Notes |
|---|---|---|
| **Raised-read-limit node** (own foundation-testnet node) | infra | Unblocks Quoter / `positions()` / `getAmountsOut` — the real "full functionality" unblock. Aligns with the in-progress single-binary Koinos node ("monolith"): once its block-sync lands, run one binary with the read-limit raised and point the proxy at it. |
| **Persistence + `eth_getLogs`** | medium | SQLite-backed tx/receipt/log index; real `logsBloom`; restart-safe. |
| **Fee-gated relay + key separation** | small/medium | Admission policy; split relay key from engine-upgrade authority (multisig/timelock). |
| **Bridge + decentralized relay + economics** | large, org-backed | The genuinely hard, capital/trust-intensive part — not solo work. |
