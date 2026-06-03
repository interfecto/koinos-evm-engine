# Testing

Two kinds of checks: **byte-exact differential tests** (Uniswap behaviour vs a reference EVM) and
**on-chain correctness checks** (receipt status, nonce handling).

## Methodology — differential vs a reference EVM

Uniswap V3 is path-dependent (every swap/burn moves price, liquidity, and fee growth), so the suite is
one **ordered** sequence:

1. Run the whole sequence once on a local `anvil` loaded with **byte-identical** compiled artifacts and
   capture every expected value (amounts, ticks, `sqrtPriceX96`, `feeGrowthGlobal`, …).
2. Bake those values into the deploy script as **hard on-chain assertions**.
3. Replay each phase **once** against the Koinos proxy. Anvil is cumulative (re-runnable); Koinos is
   incremental — each phase opens with an on-chain **preflight fingerprint** (tick / `sqrtPrice` /
   liquidity / reserves / `feeGrowthGlobal` / `feeProtocol`) and aborts before any state change if the
   pool isn't at the expected prior state. So a stale/double run can't corrupt the sequence.

Heavy reads (`positions()`, `ticks()`, `observe()`, the `Quoter`) exceed the node read-compute limit, so
correctness for those is established by **balance deltas** + light getters (`slot0`, `liquidity`,
`tickBitmap`, `feeGrowthGlobal`, `balanceOf`) instead of reading the heavy view directly.

### Reproduce

```bash
# Reference run (cumulative, re-runnable) — capture/confirm expected values:
RPC=http://localhost:8546 PHASE=all ./scripts/shell/deploy_uniswap_v3.sh   # against `anvil`

# Live run (incremental) — one phase at a time against the proxy:
RPC=http://localhost:8545 PHASE=l1 ./scripts/shell/deploy_uniswap_v3.sh
#   l1 deploy+mint · l2/l3 swaps+crossing · l4 exact-out/price-limit · l5 burn/collect
#   l6 fees · l7 second fee tier · l8 flash/protocol/oracle
RPC=http://localhost:8545 PHASE=p1 ./scripts/shell/deploy_v3_periphery.sh
#   p1 exactInputSingle · p2 exactOutputSingle+path · p3 NFPM mint · p4 QuoterV2
```

Each phase prints `==` lines and asserts equality with the reference; a mismatch aborts.

## On-chain correctness checks

Two scripts validate the 2026-06-03 fixes (see [STATUS.md](STATUS.md)). Both relay real txs through the
proxy, so they need a **running proxy** (with a funded operator) and `cast`. They use a throwaway EVM
account supplied via `TEST_KEY`.

```bash
# Receipt status: a bad-nonce tx must report 0x0, a valid tx 0x1
TEST_KEY=$(cast wallet new | awk '/Private key/{print $NF}') \
  RPC=http://localhost:8545 ./koinos-evm/verify_step1.sh

# Pending-nonce: after one send, getTransactionCount(pending) advances; back-to-back txs both land
TEST_KEY=$(cast wallet new | awk '/Private key/{print $NF}') \
  RPC=http://localhost:8545 ./koinos-evm/verify_step2.sh
```

Expected: `verify_step1` ends `bad-nonce 0x0 / correct-tx 0x1`; `verify_step2` ends `pending 0x1`,
both statuses `0x1`, final `0x2 / 0x2`.

## Engine build self-check

`koinos-evm/engine/build.sh --evm` runs `wasm-opt -Oz --mvp-features` and then **greps the output for
non-MVP opcodes**, failing the build if any are present (the Koinos Fizzy VM is MVP-only). A clean build
prints `ok: no non-MVP opcodes detected` and a ~407 KB artifact.

## What is *not* covered (be honest)

- Adversarial / fuzz / differential-against-mainnet-state testing — only the scripted happy-path lifecycle.
- Heavy read views are inferred (balance deltas), not asserted directly (read-compute limit).
- No multi-hop routing, no native-value paths. See [STATUS.md](STATUS.md).
