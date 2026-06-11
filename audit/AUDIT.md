# Koinos EVM Engine — Code Audit

**Date:** 2026-06-09
**Scope:** full repo at commit `c901d19` — engine (`koinos-evm/engine`), proxy (`koinos-evm/rpc`),
UIs (`koinos-evm/ui`), build/deploy scripts (`scripts/`), docs.
**Method:** multi-agent deep read across 7 dimensions (tx parsing & sender recovery, engine execution &
state, proxy/RPC security, EVM-compatibility gaps, build & supply chain, UI/dApp, docs-vs-code), with
every finding adversarially re-verified against the actual code. 76 raw findings → **68 confirmed,
8 refuted** as false positives.

This audit treats the project as what it says it is: a **single-operator testnet proof-of-concept** whose
*execution core is the proven part*. Severities are calibrated to that framing — the security findings
below mostly become real the moment the endpoint is exposed beyond loopback, which the code deliberately
does not do by default (`LISTEN_ADDR` defaults to `127.0.0.1`, `state.rs:50`).

---

## Headline verdict

The hard part is genuinely done and genuinely defended. The transaction trust boundary — RLP decode,
canonical-integer enforcement, EIP-2 low-s malleability, r/s range, chain-id binding, nonce replay — is
**correct**, and the engine's most-feared gaps (cross-chain replay, sender spoofing, malleability) are
**closed in code**, not just in prose. Several things the docs flag as risks turned out to be defended;
several things the docs flag as *missing* turned out to be **present**. The real exposure is concentrated
in the **relay wrapper** (operator-key liveness, unbounded in-memory state, no admission control) and in
**EVM completeness** (no native value, no `eth_getLogs`, no persistence, heavy-read limit) — exactly the
"system wrapper, not the execution core" framing `docs/STATUS.md` already uses.

### Notable corrections to the project's own documentation

These are *under-claims* — the project is more complete than its docs say:

- **Precompiles `0x05`–`0x09` are present, not missing.** `docs/STATUS.md` and `ARCHITECTURE.md` imply
  modexp / bn254 add·mul·pairing / blake2f are unimplemented. They are not: the engine runs
  `SpecId::CANCUN` (`engine.rs:166`), and revm's `Precompiles::cancun()` already links modexp (0x05),
  bn128 (0x06–0x08), and blake2f (0x09). An audit agent **built the engine** and confirmed they compile
  MVP-clean inside the existing ~407 KB artifact. Only `0x0a` (KZG point-eval, EIP-4844) is genuinely
  absent (it's a fatal stub). → see ROADMAP §1; this is *verification + doc fix*, not implementation.
- **RLP canonicality is fully defended.** Non-canonical integers, trailing-byte smuggling
  (`valid_tx || garbage`), and scalar overflow are all rejected (`tx.rs:243`, `tx.rs:95`, `tx.rs:253`).
  No tx-hash ambiguity.
- **Signature malleability is closed** (EIP-2 low-s + r/s in group order + non-zero + y-parity bound,
  `tx.rs:365–382`), and **unexpected `recover_public_key` lengths are handled strictly** — no
  zero-address / garbage-sender fallthrough (`tx.rs:414–423`).
- **The proto3 default-skip `success=false` bug is genuinely fixed** for the receipt path
  (`engine.rs:258–268` writes field 1 by hand), and **reject paths emit the failure event** so MetaMask
  sees `status 0x0` (`engine.rs:283`).

---

## Confirmed findings by severity

Severity = the *adjusted* severity after adversarial verification. "Exposure" notes when a finding is
latent today (loopback-only) vs always-on.

### HIGH

| ID | Title | Where |
|---|---|---|
| `cached-operator-nonce-desync-no-resync` | Operator nonce fetched once, cached, never re-synced — a single desync **permanently stalls the relay** until restart | `rpc.rs:375–380, 423, 462–466` |
| `no-receipt-persistence` / `tx-meta-lost-on-restart` | All tx/receipt metadata is in-memory only — any restart returns `null` for every prior `eth_getTransactionReceipt`/`getTransactionByHash` | `rpc.rs:443–446, 667–675, 706–713`; `state.rs:153` |
| `no-native-eth-value-funding` | No native-ETH supply/bridge → every `msg.value > 0` path reverts (WETH `deposit()`, `swapExactETHForTokens`, payable mints) | `database.rs:64–114`; `engine.rs:531` |

**1. Operator-nonce desync wedges the whole relay.** The operator's Koinos nonce is read from chain once
(when the cached `Option` is `None`, `rpc.rs:375`) then tracked purely in memory: `+1` on success, untouched
on failure, **never re-fetched**. A realistic event on a busy/forking testnet — a tx accepted into mempool
then dropped, or a submit the relay records as failure but the chain applied — diverges the in-memory value
from chain, and *every* subsequent `eth_sendRawTransaction` fails with a bad nonce forever. This is both an
availability bug and a griefing target. **Fix:** on any nonce-related submit error, re-fetch the chain nonce
under the lock and reset the cached value; periodically reconcile; consider a reserve/confirm scheme that
rolls back unused nonces. (Related: `operator-nonce-gate-held-across-network-io`, MEDIUM — the global mutex
is held across the entire ≤30 s Koinos round-trip, serializing all sends with head-of-line blocking.)

**2. No persistence.** `tx_meta` lives only in a `RwLock<HashMap>` reinitialized empty on boot
(`state.rs:179`). After any restart/crash/OOM, every previously-relayed tx becomes permanently unqueryable
(the koinos-tx-id → eth-hash mapping is gone, so it can't even be reconstructed). **Fix:** durable SQLite
store + startup backfill from Koinos account history — see ROADMAP §2.

**3. No native value.** revm enforces balances for value-bearing calls (the engine does *not* set
`disable_balance_check`), and balances come purely from Koinos state with no mint/bridge, so EVM accounts
hold 0 native. The engine carries `tx.value` correctly — this is a *funding* gap, not an opcode gap. **Fix:**
an operator-gated credit entry point now, a KOIN↔EVM bridge later — see ROADMAP §5 / §7.

### MEDIUM

| ID | Title | Where | Exposure |
|---|---|---|---|
| `no-admission-control-mana-drain` | Permissive CORS + zero auth/rate-limit → any origin drains operator mana for free | `main.rs:50–53`; `state.rs:50` | on non-loopback exposure |
| `unbounded-txmeta-pending-nonce-memory-dos` | `tx_meta` & `pending_nonce` are unbounded `HashMap`s — attacker-controlled memory exhaustion → OOM | `state.rs:153,170`; `rpc.rs:444–455` | on exposure |
| `operator-nonce-gate-held-across-network-io` | Global nonce mutex held across the full submit round-trip (≤30 s) — serializes all sends | `rpc.rs:368–464`; `koinos.rs:46` | always |
| `tx-meta-lost-on-restart` | (see HIGH #2 — restart loses all receipts) | `rpc.rs:667–713` | always |
| `heavy-read-1013-compute-limit` | Heavy views (`Quoter`, `positions()`, `getAmountsOut`, `observe()`) exceed node read-compute limit → `-1013` | `rpc.rs:331–338, 570–577` | always |
| `block-context-zeroed-randomness-mev` | `basefee`/`coinbase`/`difficulty`/`PREVRANDAO`/`BLOCKHASH` all zero (Cancun → DIFFICULTY *is* PREVRANDAO) | `engine.rs:125–141`; `database.rs:156` | always |
| `rejected-tx-types-2930-4844-7702` | Type-0x01 / 0x03 / 0x04 rejected; even a 0x02 tx with a **non-empty access list** is rejected | `tx.rs:82–85, 198–199` | always |
| `cancun-blob-context-unset` | `SpecId::CANCUN` selected but blob base-fee / beacon-roots (EIP-4788) context never initialized | `engine.rs:133, 166` | always |
| `no-ci` | No CI anywhere — the MVP-opcode gate, size limit, and reproducible build all rely on a human | repo-wide | always |

The first two are the documented #1 weakness (`STATUS.md` gap 5) and its amplifier. Today they're latent
because the proxy binds loopback by default, but `LISTEN_ADDR=0.0.0.0` is explicitly supported — so this is
the gate that must close before any public exposure. `cancun-blob-context-unset` is worth resolving by
**deciding the hardfork honestly**: either pin to `SHANGHAI` (no blobs/beacon-roots) or fully initialize
Cancun context (set a real `prevrandao` randomness source, blob base-fee, and run the EIP-4788 system call).

### LOW (selected — 24 total)

- **`selfdestruct-storage-leak`** (`database.rs:173`) — on `SELFDESTRUCT` the account row is removed but its
  storage slots are deliberately *not* cleared ("for simplicity"). A subsequent CREATE2 redeploy to the
  same address can **resurrect stale storage**, diverging from Ethereum. Also ignores EIP-6780's
  same-tx-only condition (it follows revm for the deletion itself, but the storage-orphan is the engine's).
- **`cross-koinos-network-replay`** (`engine.rs:22`) — the inner Eth tx binds only to `ENGINE_CHAIN_ID=42069`,
  not to the Koinos network id. If the same engine with the same EVM chain-id ever runs on two Koinos
  networks (mainnet + testnet), a signed raw tx is replayable across them. This is standard EVM
  chain-id-collision behavior, *not* a code defect — the fix is operational: give each network a distinct
  EVM chain-id (and move it into config space, per the TODO at `engine.rs:21`).
- **`relay-signs-arbitrary-unvalidated-raw-tx`** + **`full-rc-limit-reserved-per-tx-grief`** (`rpc.rs`) — the
  relay signs/pays for any RLP-decodable tx with no destination/value/gas/calldata policy, and each tx
  reserves the *full* `RC_LIMIT_MANA`, amplifying drain.
- **`v3-quote-ignores-tick-crossing`** (`v3math.js`) — the UI's client-side swap quote (the `-1013`
  workaround) ignores tick-crossing; it's an optimistic estimate, but **fund-safe** because the on-chain
  `minOut` still bounds the trade.
- **`estimategas-not-real-metering`** (`rpc.rs`) — `eth_estimateGas` returns `view_gas_used × 1.2` with a
  21000 floor, not a binary search. Fine under a zero-fee model; would mislead under a real fee market.
- Supply chain: **caret-range deps** for an on-chain artifact (`Cargo.toml` `revm="19"`), **`cargo build`
  without `--locked`** (`build.sh`), **MVP check silently skipped** when `wasm-objdump` is missing,
  **`npm ci`→`npm install` fallback** (`setup-deps.sh`), **patch applied without checksum verification**
  (`apply-patches.sh`).
- **`error-messages-leak-koinos-internals`** / **`ssrf-via-koinos-rpc-rest-url`** / **`no-request-batch-size-limit`**
  / **`curl-pipe-bash-toolchain`** — standard endpoint-hardening items.

### INFO / positive confirmations (30 total)

Worth recording because they reduce the audit surface:

- `nonce-handling-correct-no-double-increment`, `revert-halt-success-distinction-ok`,
  `balance-check-zero-gasprice-value-still-enforced` — execution-result accounting is correct.
- `rlp-canonical-and-trailing-defended`, `low-s-malleability-enforced`,
  `recover-pubkey-length-handling-correct`, `chain-id-is-validated` — the tx trust boundary is sound.
- `panic-abort-clean-state` — a panic maps to a clean Koinos failure exit, no partial state.
- `eip170-3860-defaults-enforced` — code-size / initcode limits active at revm defaults.
- `secrets-handling-clean` — no `set -x` key leakage, no hardcoded keys in deploy scripts.
- `dev-unsafe-caller-no-compiletime-guard` — the spoofable `execute`/`deploy_code` paths are unreachable in
  default builds; only build discipline (a Cargo feature) keeps them out. A belt-and-braces
  `compile_error!` guard would make it structural.
- Two reflected-XSS-adjacent notes in the explorer (`explorer-unescaped-createdaddress`,
  `explorer-txid-path-injection`) — low impact for a demo over on-chain data, but worth an `esc()`.

---

## Refuted (false positives — do **not** action)

The adversarial pass killed 8 plausible-but-wrong findings. Recording them prevents re-litigation:

1. **"Precompiles 0x05–0x09 are missing / return empty success."** False — they ship in the Cancun set and
   were empirically built. (The opposite is true; see Headline.)
2. **"Hardcoded EVM gas ceiling has no cross-tx backpressure (DoS)."** The per-tx compute is bounded by the
   block gas limit and per-tx `RC_LIMIT_MANA`; the cross-tx griefing angle is the *admission-control*
   finding, not a gas-ceiling defect.
3. **"CREATE/CREATE2 address derivation unreviewed/wrong."** revm computes both with its bundled keccak,
   matching Ethereum; `engine.rs:311` only reads the result back.
4. **"proto default-skip `success=false` bug still latent."** Fixed and hardened; the "residual risk" was
   hypothetical.
5. **"hex-quantity u64 overflow truncation."** The authoritative tx validation path rejects oversized
   scalars; the relay's loose `decode_u64` doesn't feed an exploitable sink.
6. **"estimateGas ignores intrinsic/CREATE cost → under-estimates."** Both estimateGas and the real tx run
   the same revm CANCUN path; premise is wrong.
7. **"MVP-opcode grep denylist has gaps."** The primary gate is `wasm-opt -Oz --mvp-features` under
   `set -euo pipefail` *before* the grep; every post-MVP family was empirically tested and caught.
8. **"Explorer treats no-result as success → failed txs look successful."** The engine guarantees a
   committing EVM tx always emits `evm.result`, so a result-less receipt can't be a failed EVM tx in
   production.

---

## What to fix, in order

1. **Operator-nonce resync** (HIGH, small) — re-fetch + reconcile on submit failure. One-file change in
   `rpc.rs`; removes the single worst availability bug.
2. **Persistence (SQLite) + restart-safe receipts** (HIGH, large) — see ROADMAP §2; also unblocks
   `eth_getLogs`.
3. **Admission control + bounded maps + key split** *before any non-loopback exposure* (MEDIUM, medium) —
   see ROADMAP §4. The cheapest high-value step is the **zero-code account-level key split** (run the relay
   from a *funded-but-not-engine-owning* Koinos account) so a stolen relay key can't replace the engine.
4. **CI skeleton + parser fuzz** (MEDIUM, medium/large) — see ROADMAP §6. Make the MVP-opcode gate and a
   parser test suite a merge gate so none of the above can silently regress.
5. **Native-value funding path** (HIGH-impact for compat, large) — operator-gated credit now, bridge later
   (ROADMAP §5/§7).
6. **Doc corrections** (trivial) — precompile completeness, the `eth_getLogs` comment in `engine.rs`, the
   "mainnet reference EVM" phrasing (the reference is a local anvil with byte-identical artifacts), and the
   `0x0a` KZG divergence.

See **ROADMAP.md** for the forward EVM-compatibility plan that subsumes most of these.
