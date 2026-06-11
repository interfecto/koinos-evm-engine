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

**Precompiles — vector-verified on-chain (2026-06-10):** a probe contract
(`scripts/forge/src/PrecompileProbe.sol`, driven by `scripts/shell/verify_precompiles.sh`) ran the
EIP test vectors as committing txs on the live testnet engine and compared byte-for-byte against an
anvil reference: **0x02 SHA-256, 0x03 RIPEMD-160, 0x04 identity, 0x05 modexp (EIP-198 + an
RSA-shaped 64-byte case), 0x06/0x07 bn128 add/mul (EIP-196), and 0x09 blake2f (EIP-152) are all
byte-identical.** (0x01 ecRecover is exercised by every relayed tx's sender recovery.)
**0x08 bn128 pairing — verified (2026-06-11):** all three EIP-197 vectors (single-pair success,
invalid pair, two-pair Miller loop e(P,Q)·e(−P,Q)=1) return byte-identical results via the read
path on a raised-read-limit node, and a **single pairing executes in a committed on-chain tx**
(102,280 gas, status 1). The measured committing-path ceiling: the 3-vector probe tx exceeds the
network's consensus `compute_bandwidth_limit` (287.5M per block) — so multi-pair calls
(Groth16-style verifiers need 3–4 pairings) cannot be included under current testnet resource
limits regardless of node config; raising that is a consensus/governance change, not ours.
**0x0a (KZG point evaluation)** is intentionally absent — confirmed live: any tx touching it
fails fatally (revm's no-c-kzg stub; c-kzg is a C dependency that would break the MVP-WASM build).

### Correctness fixes (2026-06-03), each on-chain-verified
1. **Failed transactions reported success.** The `evm.result` event encoded `success` with a proto3
   encoder that *skips* default values, so `success=false` was dropped → every failed tx (revert / halt /
   bad-nonce / parse error) decoded as `status 0x1`. Fixed by writing field 1 explicitly. Verified: a
   bad-nonce tx now returns receipt `status 0x0`; a valid tx returns `0x1`. (`koinos-evm/verify_step1.sh`)
2. **Back-to-back transactions collided on nonce.** `eth_getTransactionCount` ignored the `"pending"`
   block tag, so a wallet sending two txs in a row reused the nonce and the second was rejected. Fixed
   with a per-sender pending-nonce map. Verified: after one send, `pending` advances; both txs land.
   (`koinos-evm/verify_step2.sh`)

### Correctness fix (2026-06-11), on-chain-verified
3. **Concurrent relayed txs raced on the operator's Koinos nonce.** The Koinos mempool admits an
   account's txs strictly in nonce order, but the relay pipelined submissions in detached tasks —
   so two user txs in quick succession (approve+swap, swap+paint) could reach the node out of
   order and bounce with `invalid transaction nonce` (surfacing in wallets as ethers' opaque
   "could not coalesce error"). Measured live: 4 of 5 concurrent relayed txs rejected. Fixed by
   serializing the build+submit round-trip under the operator-nonce gate (counter advances only on
   success; one resync-retry on nonce drift). Verified: 5 concurrent paints all admitted, with 4
   landing in a single block (5715124-5715136 era), and a full rapid-fire quest run
   (mint→approve→swap→paint inside ~1 min) completes with zero nonce errors. The next admission
   bound is mana, not ordering: each pending tx reserves the full `RC_LIMIT_MANA` until
   irreversibility (~3 min), so burst capacity ≈ available operator mana ÷ `RC_LIMIT_MANA`
   (≈10-16 txs per window at 6e8 with a 100 vKOIN operator).

## 🚫 Not yet — do **not** claim these

| Claim to avoid | Reality |
|---|---|
| "Production-ready / mainnet-ready" | Testnet PoC only. |
| "Decentralized / trustless" | One operator key relays every tx **and** owns/upgrades the engine. |
| "Economically safe / has a bridge / fee market" | None of these exist yet. |
| "Ethereum-equivalent / standard tooling fully works" | True for reads+writes via a raised-read-limit node (live-verified); on a default public node heavy reads still degrade. Native-value paths unsupported; multi-pair pairing txs exceed consensus block compute. |

## Known gaps (the "system wrapper", not the execution core)

1. **Heavy read views hit a per-node compute limit (`-1013`) — SOLVED with a raised-limit node
   (live-verified 2026-06-11).** A local observer node (`testnet-node/`, synced to this testnet,
   `read-compute-bandwidth-limit: 300M`) serves the previously-dead read path: real
   `eth_estimateGas` (1.2×-of-actual figures, fallback never fires), `QuoterV2.quoteExactInputSingle`,
   V2 `getAmountsOut`, NFPM `positions()` — all working through the proxy. On a DEFAULT public node
   the limit (10M) still bites and the proxy degrades gracefully (`ESTIMATE_GAS_FALLBACK`, `-32005`
   errors). This is per-node config, not consensus; anyone can run such a node and get byte-identical
   answers.
2. **Persistence: done for the PoC scope** (live-verified). The durable SQLite store is
   self-populating: the account-history indexer backfills the engine's ENTIRE history on first start
   (fresh empty DB rebuilt all 154 history entries in ~18 s against the live testnet) and tails the
   head with last-irreversible-block-aware cursoring. Receipts/txs/logs survive restarts and cover
   txs relayed by anyone, ever. Remaining edge: a tx dropped entirely in a reorg lingers until
   re-included (rows above LIB are otherwise re-processed each cycle).
3. **`eth_getLogs`: done for the PoC scope** (live-verified over historic Uniswap activity).
   Address/topic/range/blockHash filters, `-32005` range/result caps so clients auto-chunk,
   block-global `logIndex`, real `transactionIndex`, real receipt **and** block `logsBloom`
   (byte-checked against geth's bloom9 on a live receipt). Block bodies are populated
   (`transactions`, summed `gasUsed`), plus `eth_getBlockReceipts` and the per-block count/index
   lookups.
4. **Single hot key.** The operator key pays mana for everyone *and* owns the engine account (so it can
   replace the engine). Cheapest high-value hardening: split the relay key from the upgrade authority and
   move upgrades to a multisig/timelock.
5. **No fee market; admission control is basic.** The operator still subsidizes every tx. The proxy now
   ships a CORS origin allowlist, per-IP rate limiting, batch/body caps, and an optional gas-price
   admission floor (`MIN_GAS_PRICE_WEI`, advertised via `eth_gasPrice`/`feeHistory`) — an anti-spam
   gate, **not** an economic fee market; a determined attacker with many IPs can still drain mana on a
   public endpoint.
6. **No KOIN↔EVM bridge**, no multi-hop routing proof, native-value (`swapExactETHForTokens`)
   paths unsupported. (WebSocket `eth_subscribe` for `newHeads`/`logs` landed and is
   live-verified; `newPendingTransactions`/`syncing` subscriptions are not offered.)

## Honest one-liner

> A proven EVM execution core running real Uniswap V2/V3 on Koinos — a high-credibility proof of concept
> and a foundation-adoption proposal, with a **bounded** path to a full testnet layer and a larger,
> org-backed path to production. The hard part (revm compatibility) is done; what remains is the trust /
> economic / indexing wrapper around it.

## Roadmap

| Milestone | Effort | Notes |
|---|---|---|
| **Raised-read-limit node** (own foundation-testnet node) | infra | Unblocks Quoter / `positions()` / `getAmountsOut` — the real "full functionality" unblock. Aligns with the in-progress single-binary Koinos node ("monolith"): once its block-sync lands, run one binary with the read-limit raised and point the proxy at it. |
| **Persistence + `eth_getLogs`** | ~done | Landed + live-verified: SQLite store, full account-history backfill indexer (LIB-aware), `eth_getLogs`, block-global `logIndex`, real `txIndex`, receipt + block blooms, populated block bodies, `eth_getBlockReceipts`. Remaining edge: reorg-dropped txs linger. |
| **Fee-gated relay + key separation** | small/medium | Admission policy; split relay key from engine-upgrade authority (multisig/timelock). |
| **Bridge + decentralized relay + economics** | large, org-backed | The genuinely hard, capital/trust-intensive part — not solo work. |
