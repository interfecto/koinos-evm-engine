# Launching real-value DeFi (e.g. Uniswap V3) on Koinos mainnet

> **Status: assessment, not a plan of record.** This captures what stands between the proven testnet
> execution core and a *real-money* deployment. It is deliberately blunt about blockers. None of this
> is implemented; the testnet PoC is exactly that. The recurring theme matches
> [STATUS.md](STATUS.md): the hard part (revm compatibility) is done — what remains is the **trust /
> economic / asset wrapper** around it.

Deploying the contracts is the easy 1%. The real distance is one design decision, two key ceremonies,
a chain-id rebuild, an independent audit, and an economics model — in dependency order below.

---

## 0. The keystone decision: what is KOIN *inside* the EVM?

There is currently **nothing of value to trade inside the engine**. The only engine-state ERC-20s
are faucet/demo tokens. On the Koinos side, real assets do exist: the
[Vortex bridge](https://medium.com/koinosnetwork/announcing-vortex-bridge-8469c37e55cb) (a
Wormhole-style 5-of-7 guardian bridge between Ethereum and Koinos) brings bridged USDT, ETH and
KOIN onto mainnet as Koinos tokens. But a Koinos token is invisible to the EVM layer — so step
zero is a Koinos-token↔EVM wrapper (for KOIN itself and for any bridged asset) — and two facts
reshape it:

- **It is not a cross-chain bridge.** KOIN and the EVM live on the *same* chain, so a deposit is one
  atomic Koinos transaction: a KOIN token-transfer op to a lock contract + a cross-contract call that
  credits the EVM side. No relayers, no light clients, no external validators — weekend-scale contract
  work, not a bridge project.
- **The engine already carries `tx.value` correctly** (revm balance checks are on; confirmed in
  [audit/AUDIT.md](../audit/AUDIT.md)). "No native value" is a **funding** gap, not an engine gap —
  nothing ever *mints* native balance. **If the lock contract mints native KOIN balance** (rather than
  a wrapped ERC-20), three apparent blockers collapse at once: `payable`/native-value paths work,
  MetaMask shows a real balance, and a gas-price floor becomes a usable spam gate. **This single choice
  — native-balance vs. ERC-20-only wrapper — is the highest-leverage decision on the whole list.**

Liquidity footnote: with Vortex-bridged USDT/ETH wrappable into the EVM, real launch pairs
(KOIN/USDT, KOIN/ETH) are possible — not just the mechanically correlated KOIN/VHP. The caveat is
**trust stacking**: an EVM-side vUSDT position is exposed to Tether *plus* the Vortex 5-of-7
guardian set *plus* the engine owner key (§1). Each wrapper layer must be disclosed; depth still
depends on how much capital actually bridges over.

## 1. Keys and the rug vector — **blockers**

Today **one hot key, loaded into an internet-facing process, both relays every tx and owns the engine
account** — meaning it can `upload_contract` a malicious VM and take 100% of TVL. There is no on-chain
owner/governance gate inside the engine itself. Sequence (cheapest first):

1. **Split the relay payer from the engine owner** (zero code): point `OPERATOR_PRIVKEY_HEX` at a
   fresh funded account. The relay only ever calls `call_contract`, never `upload_contract`, so it
   works unchanged — and a stolen relay key then drains only that account's mana, it cannot replace
   the engine.
2. **Move engine-upgrade authority to a cold M-of-N multisig with a published, mandatory timelock** on
   `upload_contract`, so LPs can exit before any upgrade lands. The running proxy never needs this key.
3. **Decide upgradeable-vs-immutable explicitly.** Burning the key gives an immutable VM (no rug, no
   censor-via-upgrade) but also removes the ability to patch a revm/consensus bug. Timelocked
   upgradeability keeps that ability at the cost of a standing trusted authority. Document the tradeoff.
4. **Add a state-schema version tag** to the serialized account/storage records + a post-upgrade
   invariant check (e.g. `sum(balances)` preserved on a fork), so a format-changing upgrade can't
   silently misread every pool's balances.

## 2. The chain-id footgun — **blocker**

The inner EVM tx binds only to `ENGINE_CHAIN_ID` — a hardcoded `42069` (`engine.rs`), mirrored by the
relay's `EVM_CHAIN_ID` and the UIs' `config.js` (**three places that must change together**). If
mainnet reuses `42069`, **every tx ever signed on testnet — approvals, permits, swaps — is replayable
on mainnet** (and EIP-712 permits collide too). Assign a distinct registered EVM chain-id, rebuild the
engine, set the relay + UI, and ideally bind the inner tx to the Koinos network id as well. Cheap to
do, fund-loss-grade if forgotten — because it's the default value.

## 3. Independent audit — **blocker**

The in-repo [AUDIT.md](../audit/AUDIT.md) is a *self-audit*, explicitly calibrated to a loopback
testnet PoC; its own text says the findings escalate the moment value is at stake. The engine is
consensus-critical bytecode (the tx parser/sender-recovery trust boundary, precompile overrides
0x01–0x04, the Koinos↔revm state mapping, account/storage serialization, SELFDESTRUCT handling). An
independent audit of those paths plus the relay submit path, a re-rating of every "on exposure"
finding against real-money severity, and a standing bug bounty are all prerequisites, not nice-to-haves.

## 4. Economics — **blocker** (the part most skip)

Good news the analysis surfaced: **mana regenerates (~5 days to full), so the relay "subsidy" is
cost-of-capital on locked KOIN plus DoS-insurance, not a per-tx burn.** The clean arithmetic:

- **Sustained throughput** ≈ `(operator KOIN / 5 days) / 2.5 KOIN-mana-per-swap`. So ~1,000 swaps/day
  ≈ a **~12,500 KOIN standing float**; throughput is literally purchasable capital.
- **Burst** is the binding constraint and it's small: each pending tx reserves the *full*
  `RC_LIMIT_MANA` until ~3-min irreversibility → **~6 concurrent slots per 100 KOIN** of operator mana.

Two non-obvious traps:

- **LP exits fail before swaps do.** An NFPM mint measured ~530M rc — within ~12% of today's 6e8 RC
  limit. Under mainnet dynamic resource pricing, position *management* (deposit AND withdraw) is the
  fragile operation, which is the worst thing to have fail in a market move.
- **The "anyone can self-submit" escape hatch structurally fails for the users it should protect.**
  Koinos mana capacity equals KOIN balance, so an LP who has bridged their KOIN into the pool has no
  mana left to self-submit a withdrawal with — the censorship/MEV mitigation evaporates exactly when
  it's needed.

Implication: you need a real fee/float model (covering capital lockup + DoS, not per-tx gas), and a
published, enforceable ordering policy (the single relay is a single sequencer; classic gas-auction MEV
is absent only because all ordering power sits with that trusted party). Bound launch TVL to what the
trust setup actually covers.

## 5. Infra, assets, operations — **blockers + serious**

- **Mainnet consensus limits are unverified.** Block compute limit, single-tx fit, network-bandwidth
  budget, and mana pricing are all testnet-only numbers — must be re-measured on mainnet before sizing.
- **Observer-node fleet:** heavy `eth_call` views (`Quoter`, `positions()`) need a raised
  `read-compute-bandwidth-limit` node, and the relay's whole restart/DR story depends on a node
  retaining the engine address's **full `account_history`** — both are per-node, non-default config.
  Real-scale public RPC wants several such nodes + HA, against a write path that is *one* relay
  serialized on the operator-nonce gate (a throughput ceiling and a single point of failure).
- **No state snapshot / no state root.** All EVM state lives in one contract's KV space; there is no
  export tool and no Merkle state root to verify against (`eth_getProof` is deferred for exactly this
  reason). That undermines DR, light-client trust, and cross-checking.
- **Finality UX:** receipts in ~3s but reversible for ~3 min, plus a known indexer reorg edge that can
  show a trader a "successful" swap that didn't ultimately land. Needs explicit confirmation UX.
- **No tracing APIs** (`debug_trace*`/`trace_*`) → no Etherscan-class explorer; traders have no
  independent venue to verify txs/positions (the in-repo explorer + `ktx` viewer help, but aren't that).
- **No metrics/alerting/CI** on the proxy (tracing logs only); **no mainnet deployment runbook**
  (DEPLOYMENT.md is testnet-only); the shipped front-ends are all hardwired to testnet config.
- **Periphery WETH9 slot:** the V3 periphery's immutable WETH9 constructor arg needs a decision —
  deploy a canonical wKOIN-as-WETH9 even though, absent the native-mint choice in §0, payable paths
  stay dead.

## 6. Legal / brand — **get professional advice (this section is not legal advice)**

- **Name, not code.** Uniswap v3-core is GPL-2.0-or-later now (the vendored license permits use); the
  exposure is the **"Uniswap" trademark** — you cannot launch as "Uniswap on Koinos" without
  clearance. Pick your own product name.
- **Operator posture.** A single operator who hosts the swap UI, sequences trades, *and* sponsors fees
  for real-value assets is a regulatory posture that is jurisdiction-dependent and needs a professional
  opinion — flagged, not advised here.
- **Disclosure & reputation.** Unaudited engine, upgradeable VM, single relay — serious projects
  disclose these plainly. A "Uniswap on Koinos" that breaks with real money on it is reputational risk
  for the Koinos foundation too, which argues for a foundation-backed, not solo, launch.

---

## Sequenced summary

```
0. Decide the wrapper: lock contract mints NATIVE KOIN balance (unblocks payable, balances, fee floor)
1. Key split (zero code) → cold M-of-N multisig + timelock on engine upgrades → state-schema version tag
2. New registered EVM chain-id → rebuild engine + relay + UI (kills testnet↔mainnet replay)
3. Independent audit of engine + relay + bug bounty
4. Fee / float model (capital + DoS, not per-tx) + published ordering policy + TVL cap
5. Observer-node fleet (raised read limit + full account_history) + HA + monitoring + DR/snapshot + runbook
6. Legal: product name (not "Uniswap") + operator/regulatory opinion + disclosures
```

Honest framing: this is a focused **multi-month, foundation-grade** effort with an external audit on
the critical path — not a solo weekend launch. The execution core that everyone fears is the hard part,
and it's done; everything above is the known (if substantial) wrapper work around it.
