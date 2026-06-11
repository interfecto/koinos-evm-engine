# Pushing the EVM-Compatibility Layer Forward

**Date:** 2026-06-09 · companion to [AUDIT.md](./AUDIT.md)

The execution core is done and proven. What remains is the **trust / economic / indexing wrapper** around
it — and closing a few EVM-completeness gaps. This roadmap sequences that work from "cheap, solo, ships
this week" to "large, org-backed, not solo work," with concrete file-level steps for each. Each workstream
was designed against the actual code; effort tags are the designers' estimates.

The unifying theme: **convert "runs Uniswap" into "point any Ethereum tool at it and it works."**

---

## Dependency / sequencing map

```
        ┌─────────────────────────────────────────────────────────┐
PHASE 0 │ §4 Key split (0-code) · admission control · real fee-RPC │  ← do first, mostly solo, de-risks all
 cheap  │ §1 Precompile verification + doc fix (already compiled)  │  ← independent, high credibility/effort ratio
        │ §3 Raised-read-limit node (config, no code)              │  ← independent, unblocks the #1 usability gap
        └─────────────────────────────────────────────────────────┘
                                  │
        ┌─────────────────────────▼───────────────────────────────┐
PHASE 1 │ §2 Persistence + eth_getLogs + real logsBloom (SQLite)   │  ← hard dependency for tooling + decentralization
 found.  │ §6 CI skeleton + parser fuzz/property tests             │  ← merge gate; land the skeleton early
        └─────────────────────────┬───────────────────────────────┘
                                  │
        ┌─────────────────────────▼───────────────────────────────┐
PHASE 2 │ §5 Full eth_* tooling: WS eth_subscribe, getBlockReceipts,│
 compat │     populated blocks, spec error objects, native-value    │
        └─────────────────────────┬───────────────────────────────┘
                                  │
        ┌─────────────────────────▼───────────────────────────────┐
PHASE 3+│ §7 KOIN↔EVM bridge · multi-relayer · fee market (org-backed, not solo)
        └──────────────────────────────────────────────────────────┘
```

---

## §1 — Precompiles 0x05–0x0a: verify, wire explicitly, decide KZG  · effort: medium

**The reframe:** these are **not missing**. An audit agent built the engine (`build.sh --evm`) and confirmed
revm's `Precompiles::cancun()` already links modexp (0x05), bn254 add·mul·pairing (0x06–0x08), and blake2f
(0x09) — all MVP-clean inside the existing ~407 KB artifact. The work is **verification + intentional
wiring + an explicit 0x0a decision**, not implementation. Closing this credibly is what turns "runs
Uniswap" into "runs the EVM" — it unblocks RSA/modexp, Groth16/PLONK zk-verifiers (bn254 pairing), and
cross-chain verifiers (blake2f).

1. **Reframe in docs** — `0x05`–`0x09` are present-and-compiled; do *not* reimplement them.
2. **Verify-first (no code change):** Solidity probe contracts that `staticcall` each precompile —
   modexp (`3^2 mod 5` + a 256-byte RSA-shaped exponent), ecAdd/ecMul with EIP-196 vectors, ecPairing with
   the EIP-197 single-pair vector (revm's own `bn128.rs` tests return `0x…01`), blake2f with the EIP-152
   vector. Deploy to the engine *and* anvil; assert byte-identical output.
3. **Add a precompile smoke test** to the deploy/`verify_step*.sh` pattern so 0x05–0x09 are covered like
   Uniswap is.
4. **Make wiring self-documenting** — in `engine.rs`, a comment block stating 0x05–0x09 are intentionally
   inherited from revm's Cancun set and only 0x01–0x04 are overridden (Koinos syscalls). Factor the
   duplicated precompile-registration closure (`engine.rs:177` and `:545`) into one shared helper so the
   two call sites can't drift.
5. **Decide 0x0a explicitly** — recommend OFF, but legible: replace revm's generic fatal stub with a
   `PrecompileWithAddress(0x0a)` returning a clear "KZG point-evaluation unsupported on Koinos EVM" error.
   Do **not** pull `c-kzg` (C + build.rs, breaks MVP-WASM). If ever needed, spike `kzg-rs` behind an
   off-by-default feature and re-measure size (it adds the trusted-setup table — likely pushes over budget).
6. **Harden the pairing `-1013` risk** — ecPairing is the heaviest precompile under the Fizzy interpreter;
   a multi-pair zk-verifier call on a *read* path can exceed the node's 10 M read-compute limit even though
   it succeeds in a committing tx. Add a 2–4-pair vector, measure `compute_bandwidth_used`, document the
   per-call pair ceiling. Same mitigation as §3 (raised-read-limit node) — config, not consensus.
7. **Update docs** — `ARCHITECTURE.md` precompiles row, `README`, `STATUS.md`: 0x01–0x09 supported
   (0x01–0x04 via syscalls, 0x05–0x09 via revm-native), 0x0a intentionally unsupported, pairing read caveat.

*Sequencing:* independent of everything; the empirical baseline is already confirmed. Highest
credibility-per-effort item in the whole roadmap.

---

## §2 — Persistence + eth_getLogs + real logsBloom  · effort: large

Replace the in-memory `tx_meta` (`state.rs:153`) with a durable SQLite store that indexes txs/receipts/logs
by hash, block, address, and topics; backfill from Koinos account history on startup (restart-safe);
implement `eth_getLogs`; compute a real (non-zero) `logsBloom`. These are `STATUS.md` gaps #2 and #3 and the
named "Persistence + eth_getLogs (medium)" milestone. This is the difference between "MetaMask sends a tx"
and "The Graph / explorers / wallet history work."

1. **`db` module** — `rusqlite` (bundled feature, no system libsqlite) behind `tokio::task::spawn_blocking`,
   `PRAGMA journal_mode=WAL; synchronous=NORMAL; foreign_keys=ON`. `DB_PATH` env, default
   `./koinos-evm-rpc.sqlite`.
2. **Schema** — `txs(eth_hash PK, koinos_tx_id, from, to, nonce, value, input, gas_limit, raw_tx, tx_type,
   chain_id, sig_r/s/v, block_height, block_hash, tx_index, status, gas_used, contract_address, seq_num,
   logs_bloom)`; `logs(id PK, eth_hash, block_height, block_hash, tx_index, log_index, address,
   topic0..topic3, data, removed)` — **topics as 4 fixed columns** so the canonical filter is an indexed
   `WHERE`; `blocks(height PK, block_hash, parent_hash, timestamp, tx_count, logs_bloom)`; `meta(key,value)`
   for the backfill cursor + irreversible height. Indexes on `logs(address,block_height)` and
   `logs(topic0,block_height)`.
3. **Canonical EVM-block mapping** — one helper, reused everywhere: `evm_block_number == koinos_block_height`
   (already what execution sees, `engine.rs:128`). `tx_index` = position of the `submit_raw_tx` op within its
   Koinos block (fixes the always-`0x0` index); `log_index` becomes **block-global** per spec.
4. **Real `logsBloom`** — a pure, unit-tested `bloom.rs`: keccak256 the address + each topic, fold 3 bit
   positions (byte pairs masked to 11 bits) into a 2048-bit accumulator; receipt bloom = OR of its logs,
   block bloom = OR of receipts. Replaces the hardcoded zeros at `rpc.rs:657` / `:842`.
5. **Write path** — `handle_send_raw_tx` upserts the `txs` row immediately (receipt fields NULL) so
   `getTransactionByHash` works pre-inclusion; DB is the source of truth.
6. **Indexer task** (the heart of restart-safety) — a background tokio task that pages
   `account_history.get_account_history` on the engine address from a persisted cursor, decodes each
   `raw_tx` (reuse `eth_tx::decode_and_recover`), parses `evm.result`/`evm.log` events (factor the existing
   `rpc.rs` decode into a shared `receipt_decode`), resolves block/tx-index, computes blooms, writes
   txs+logs+blocks in one transaction, advances the cursor; then tails head every ~3 s. Self-populating,
   fully restart-safe.
7. **Reorg/finality** — track `last_irreversible_block` from `get_head_info`; treat heights above it as
   provisional and re-index on a fork-point hash mismatch; rows ≤ irreversible are immutable.
8. **Read path** — receipt/tx handlers query the DB first, fall back to a single live Koinos read only for
   not-yet-indexed txs. Serve real `logs_bloom` and correct `tx_index`/`log_index`.
9. **`eth_getLogs`** — add to dispatch (`rpc.rs:65`). Parse `{fromBlock,toBlock,address,topics,blockHash}`,
   build a parameterized query (`address IN (…)`, positional `topicN IN (…)`), enforce a configurable
   block-range / result cap returning the standard `-32005` so viem/ethers auto-chunk.
10. **Block bodies** — `getBlockByNumber/Hash` read real `blocks.logs_bloom`, populate `transactions`, set
    `gasUsed` = sum of receipt gas. Shared `evm_log_to_json` + `receipt_to_json` to kill duplication.
11. **Tests + ops** — golden `bloom.rs` vectors, topic-filter SQL tests, a backfill-from-fixture integration
    test; document `DB_PATH`, first-start backfill, and a `--reindex` reset in `DEPLOYMENT.md`.

*Sequencing:* land **after** the receipt-decode logic is factored into a shared module. Prerequisite for §5.
Recommended kickoff order: `bloom.rs` + golden tests (pure, no deps) → schema → indexer → getLogs.

---

## §3 — Heavy-read `-1013` unblock  · effort: medium

The single biggest *usability* blocker (`STATUS.md`). Heavy views — `QuoterV2.quoteExactInputSingle`,
`positions()`/`ticks()`, `getAmountsOut`, oracle `observe()` — exceed the node's
`read-compute-bandwidth-limit` (default ~10 M) and revert `-1013`. The UIs hide it with client-side math
(`v3math.js`, `pool.js`), but that's a per-app patch, not EVM equivalence.

- **PRIMARY — raised-limit node (config, zero code).** Stand up our own node syncing the *existing*
  foundation testnet (not a new network) with `read_compute_bandwidth_limit` raised to ~200–500 M (size it
  empirically: measure the worst real view, set ~4–8× headroom). This flag gates only `read_contract`
  metering — **per-node, not consensus** — so the node stays a valid peer. Point the proxy at it via
  `KOINOS_RPC_URL`/`KOINOS_REST_URL`; zero proxy code change. Aligns with the in-progress single-binary
  "monolith" node: when its block-sync lands, run one binary with the limit raised.
- **Harden the now-public compute surface** — a per-call `gas_limit` cap in `handle_eth_call`
  (`rpc.rs:318`), basic rate limiting / request-size cap (`main.rs`), sane timeout (already 30 s). Land
  these **before** exposing the raised-limit node.
- **Honest trust framing** — a raised-limit node is one we operate for *availability*, not *correctness*:
  the engine is deterministic, so any third party can re-run the same view against the same on-chain state
  on their own raised-limit node and get a byte-identical answer. Liveness trust, not correctness trust.
- **FALLBACK — proxy-hosted off-chain revm.** Embed revm in the proxy, backed by a `Database` impl whose
  `storage()`/`basic()`/`code_by_hash()` fetch via the *cheap* existing primitives (`eth_getStorageAt` =
  one syscall/slot, well under 10 M). Same `SpecId::CANCUN` / zero-basefee env as the engine. Pin every read
  to a single head height (snapshot at call start; retry-on-advance) to avoid tearing across 3 s blocks.
  Use it as a guardrail for views that exceed even the raised limit, or if we can't run a node yet.
- **TERTIARY — result cache** keyed by `(to, calldata, caller, value, block-height)` with TTL ≤ one block.
  Optimization only, never a primary fix.

*Recommended rollout:* ship PRIMARY (lowest effort, byte-exact, docs already endorse it) → add node-hardening
caps + short-TTL cache → build the off-chain-revm fallback as insurance. Independent, can ship immediately.

---

## §4 — Relay hardening: key separation + fee-gated admission control  · effort: medium

Today one secp256k1 key both pays mana for every tx **and** owns the engine account that can replace the WASM
(`STATUS.md` gap 4), CORS is `permissive()`, and the EVM sender pays zero gas-price — so any browser anywhere
can submit unlimited subsidized txs (`STATUS.md` gap 5). Prerequisite to letting anyone but the author point
a wallet at it.

1. **STEP 1 — split the key at the account level (zero code, highest value).** Stop using the
   engine-owning account as the relay payer: generate a fresh funded Koinos account, set
   `OPERATOR_PRIVKEY_HEX` to it. The relay only does `call_contract` (never `upload_contract`) and the engine
   does no payer auth, so it works unchanged — and a stolen relay key now drains only that account's mana, it
   **cannot replace the engine.** Document this as the required posture in `DEPLOYMENT.md`.
2. **STEP 2 — move engine-upgrade authority to a multisig/timelock.** Transfer the engine account to a
   Koinos multisig so `upload_contract` needs M-of-N (+ optional timelock). Keep that key fully cold — the
   running proxy never needs it.
3. **STEP 3 — EVM gas-price floor (cheapest code-level abuse gate).** Capture the currently-discarded gas
   price in `eth_tx.rs` (`gas_price` legacy / `max_fee_per_gas` 1559), thread into `DecodedTx`, reject below
   a `MIN_GAS_PRICE_WEI` env floor in `handle_send_raw_tx`; have `eth_gasPrice`/`feeHistory` return the floor
   so wallets auto-populate it. Spam now costs the *sender* a valid-signature constraint while the engine
   still charges 0 ETH — an admission gate, not a real fee market.
4. **STEP 4 — token-bucket rate limiting** (Axum middleware) keyed on the recovered EVM `from` + a global
   ceiling; `-32005` when empty; env-configurable. Reads get a separate looser limit.
5. **STEP 5 — nonce-gate backpressure** — bounded wait / queue-depth cap so a burst sheds instead of
   blocking workers; track outstanding reserved mana and refuse when it exceeds the account's reservable
   balance. (Pairs with the HIGH nonce-resync fix in AUDIT.)
6. **STEP 6 (per deployment tier) — gate write access for a truly public endpoint:** API-key tiers, or a
   small KOIN deposit/allowlist, or PoW/captcha in front of a faucet UI; tighten CORS from `permissive()` to
   an origin allowlist for writes while leaving reads open.

*Sequencing:* STEP 1 first — zero-code, immediately removes "stolen relay key replaces the engine," unblocks
public-exposure conversations.

---

## §5 — Full eth_* tooling + native-value path  · effort: xlarge

Make ethers/viem, MetaMask, Hardhat, and a block explorer work unmodified. Builds on §2.

- **STEP 0 (prereq):** §2 persistence + log index.
- **STEP 1 — `eth_getLogs`** (highest tooling leverage: wallet history, explorers, The Graph,
  `queryFilter`). Range cap + `-32005` so tooling auto-chunks.
- **STEP 2 — real `logsBloom` + populated block bodies + `eth_getBlockReceipts`** — fill `block.logsBloom`
  / `receipt.logsBloom` (vs zeros at `rpc.rs:657`/`:842`), populate `transactions`, real `gasUsed`,
  best-effort `receiptsRoot`.
- **STEP 3 — WebSocket `eth_subscribe`/`eth_unsubscribe`** (`newHeads`, `logs`) — axum WS upgrade alongside
  the POST route; one head poller pushes `newHeads` and evaluates active log filters. Unblocks viem
  `watchEvent`/`watchBlocks` and `provider.on`, which silently fail on HTTP-only providers.
- **STEP 4 — spec-correct JSON-RPC error objects** — replace the catch-all `-32000` (`rpc.rs:135`) with
  typed mapping (`-32602` bad params, revert code 3 + `data`, distinguishable `-1013`, `-32005` getLogs
  range).
- **STEP 5 — native-ETH funding** (unblocks `swapExactETHForTokens` + any payable tx). Add an
  operator/bridge-gated `EP_CREDIT_BALANCE` entry point that mints native balance by writing the
  accounts_space record (`database.rs serialize_account`) — minimal = operator faucet credit, full = the §7
  bridge. With balance present, the existing `tx.value → TxEnv.value → revm` path already works end to end.
- **STEP 6 — lightweight `debug_traceTransaction` (callTracer)** for Hardhat/Foundry assertions, via a revm
  inspector. Prioritize callTracer over full struct logs; defer `trace_block`.
- **STEP 7 — cheap conveniences tooling probes on connect:** `eth_syncing→false`, `eth_accounts→[]`,
  `getBlockTransactionCountБy*`, `getTransactionByBlock*AndIndex` (trivial given the index), `web3_sha3`,
  `net_listening→true`. Defer `eth_getProof` (needs a Merkle-backed state root the engine doesn't maintain)
  and return `-32601` explicitly.

---

## §6 — Correctness assurance: fuzz, differential, CI  · effort: large

Move from a single scripted happy-path to layered automated assurance. `TESTING.md` honestly admits zero
adversarial/fuzz/differential-against-real-state coverage and **no CI** — for consensus-critical bytecode
that's the biggest process gap. The parser (`tx.rs`) is the trust boundary; a parser bug is *both* a
security and an EVM-compat defect.

1. **WS-A0 — make the engine host-testable** without breaking the WASM build: a `test-host` feature, and
   refactor `tx.rs` crypto behind a `trait Crypto { keccak256; recover }` so production wires
   `sys::hash`/`sys::recover_public_key` and tests wire `tiny-keccak` + `k256`. Keep the pure decode helpers
   crypto-free so they fuzz in isolation. Fence with golden mainnet sender-recovery vectors so the refactor
   is provably behavior-preserving.
2. **WS-A1 — unit tests** for every `TxParseError` rejection + golden positive vectors (legacy / EIP-155 /
   EIP-1559) asserting recovered sender + signing hash vs alloy/etherscan.
3. **WS-A2 — property tests (proptest) as a differential oracle vs alloy** — round-trip (random valid
   `TxEnvelope` → encode → `parse_raw_tx` → assert sender + fields match) and mutational (leading-zero
   injection, trailing bytes, type-byte swaps, high-s flip, y-parity tampering → assert
   accept/reject agreement *and* no panic).
4. **WS-A3 — `cargo-fuzz` targets** — `fuzz_parse_raw_tx`, `fuzz_parse_vs_alloy`, `fuzz_decode_varint`
   (proto bounds + 10-byte cap). Seed from the golden vectors.
5. **WS-A4 — proto codec tests** — varint round-trip incl. the `shift==63` edge, malformed tags, truncation;
   mirror into `eth_codec.rs` to cross-check the two codecs.
6. **WS-B1/B2 — revm-vs-anvil differential harness** with an in-memory `KoinosDatabase` test double, over a
   broadened corpus (OZ ERC20/721/1155, Multicall3, multi-hop routing, **native-value paths**) — asserting
   byte-equal per-account state + logs + receipt status.
7. **WS-B3 — zk-verifier corpus** (deferred, gated on §1) — a Groth16/PLONK verifier with known
   proof+inputs asserting `verify()==true` on both engines, plus a tampered-proof `false` vector.
8. **WS-C1 — GitHub Actions CI** (pinned container): `cargo build --release --locked --features evm` →
   `build.sh --evm` (the MVP-opcode gate + 1 MB size check become the **merge gate**); host test suite;
   `cargo-audit`/`cargo-deny`; `fmt --check` + `clippy -D warnings`. Pin action SHAs.
9. **WS-D1 — invariant / stateful fuzzing** — nonce monotonicity, replay rejection, sender-recovery
   determinism, value conservation, cross-checked against a simple Rust ledger model.

*Sequencing:* WS-A0 first (prereq for all of A and D). Land **WS-A0 + a few A1 tests + the CI skeleton** as a
single starter PR so the gate exists before the suite is filled in.

---

## §7 — KOIN↔EVM bridge · multi-relayer · fee market  · effort: xlarge, org-backed

The "trust / economic / indexing wrapper" `STATUS.md` names as all that remains. **Not solo work** —
capital/trust-intensive. Staged testnet-first, not a big-bang launch.

- **Bridge — lock-mint, not engine-holds-KOIN.** A Koinos-side bridge contract (or reserved engine entry
  point) into which a user locks/burns KOIN; relayers/validators observe the deposit event and the engine
  credits the EVM address's balance object (`database.rs` field 2). Withdrawal is the inverse via an
  `evm.log` the bridge watches. Reject the "engine holds a big float" model — it's an unbacked honeypot.
  Assert `sum(EVM native balances) == KOIN locked in bridge` from chain state.
- **Bridge — stage the minting trust:** Stage 1 operator/multisig-authorized mint (testnet faucet-grade) →
  Stage 2 M-of-N validator attestation → Stage 3 light-client/bonded validators with slashing. Sequence
  **behind §2** (honest deposit/withdraw accounting needs durable queryable events on both sides).
- **Relay — externalize sequencing state.** Move `operator_nonce`/`pending_nonce` from in-process into the
  shared store (the §2 DB), the precondition for >1 relayer.
- **Relay — prevent double-relay/nonce races:** single logical sequencer with interchangeable workers
  (simplest correct step), or sender-sharded assignment / on-chain relay claim. The engine's
  strictly-sequential EVM nonce check (`engine.rs:500–516`) is the safety backstop.
- **Relay — MEV/ordering explicitly.** With `coinbase`/`basefee`/`gas_price` all 0, classic priority-gas-
  auction MEV is absent; define an explicit ordering policy (FIFO-by-arrival or fee-priority) as a
  *deliberate* design choice. Keep a single logical sequencer until decentralized sequencing is funded —
  the hardest, most research-heavy piece.
- **Fee market — map EVM gas → KOIN mana with a real payer.** Option 1 sponsored-but-gated (testnet);
  Option 2 user-pays-in-native (debit bridged balance → reimburse relayer — the sustainable production model,
  why the bridge must come first); Option 3 paymaster/meta-tx. Make `eth_gasPrice`/`maxPriorityFeePerGas`/
  `feeHistory` return real relay-published numbers (vs the `0` stubs at `rpc.rs:70–72`).

**Staging:** Phase 0 (solo) key split + admission gating + real fee-RPC → Phase 1 (med) persistence +
externalized sequencing → Phase 2 (med/large) single-sequencer multi-relayer pool → Phase 3 (large)
lock-mint bridge + native value + WETH → Phase 4 (capital) bonded bridge + user-pays fee market + slashing →
Phase 5 (research) decentralized sequencing. Be explicit in docs that Phases 3–5 are not solo work.

---

## Suggested first sprint (highest value, mostly solo)

1. **Operator-nonce resync** (AUDIT HIGH #1) — small `rpc.rs` change, kills the worst availability bug.
2. **Key split STEP 1** (§4) — zero code, removes "stolen relay key replaces the engine."
3. **Raised-read-limit node** (§3 primary) — config-only, unblocks the #1 usability gap.
4. **Precompile verification + doc fix** (§1 steps 1–4) — proves the EVM is more complete than advertised.
5. **CI skeleton + parser golden vectors** (§6 WS-A0/A1/C1) — make the MVP gate a merge gate.

Each is independent, ships in days, and materially advances both safety and the "standard tooling works"
story before the large persistence/bridge work begins.
