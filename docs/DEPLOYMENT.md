# Deployment

How to build the engine + proxy and deploy contracts from a fresh clone. Everything targets the
**Koinos foundation testnet** (`https://testnet.koinosfoundation.org`).

## 0. Prerequisites

- **Rust** with the `wasm32v1-none` target (`rustup target add wasm32v1-none`)
- **binaryen** (`wasm-opt`, `wasm-strip`) — for the engine build
- **Foundry** (`forge`, `cast`) and **Node.js** (for the v3-periphery npm deps)
- A **funded foundation-testnet account** to use as the operator (it pays Koinos mana). Top it up via
  the testnet faucet. You also need its raw secp256k1 key as `OPERATOR_PRIVKEY_HEX`.
- The [`koinos-cli`](https://github.com/koinos/koinos-cli) (only needed to *upload* the engine WASM).

> **Just want to USE the chain (deploy Solidity, swap)?** You don't need most of this. The proxy
> defaults `ENGINE_CONTRACT` to the engine already live on the testnet, so build only the proxy
> (`cd koinos-evm/rpc && cargo build --release`), supply a funded operator key, and skip §1–§3
> (submodules, engine WASM, upload) entirely. See the README "Fast path" and §5 "Deploy your own
> contract" below.

### Koinos prerequisites for Ethereum-native devs

The operator key is the one Koinos-native thing you need. Steps:

1. **Get the CLI.** Download a [`koinos-cli`](https://github.com/koinos/koinos-cli) release.
2. **Create a wallet:** in the CLI, `create my.wallet` (or `open my.wallet`). It prints/stores a
   secp256k1 keypair and an address.
3. **Point it at the testnet** and **fund it** from the Koinos foundation testnet faucet (the foundation
   distributes testnet KOIN; check the current Koinos docs/community for the active faucet — it has moved
   over time). You only need a few KOIN; mana regenerates (~5 days to full).
4. **Export the raw private key** as 32-byte hex for `OPERATOR_PRIVKEY_HEX`: in the CLI, `open` the
   wallet then `private` prints the WIF; convert WIF → raw hex (drop the version byte + checksum, take the
   32-byte payload). That hex string is what the proxy wants.

The same key can deploy your engine (§3) if you go the full path; for the fast path it only ever pays
mana, never owns anything.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for exact install commands.

## 1. Fetch submodules

```bash
git submodule update --init --recursive          # OpenZeppelin, Uniswap v2/v3 core + periphery, solidity-lib
( cd scripts/forge && ./apply-patches.sh )        # patches v2-periphery's Pair init-code hash (required for V2)
( cd scripts/forge && ./v3p-build/setup-deps.sh ) # npm deps the v3-periphery (Hardhat) build needs
```

> The `apply-patches.sh` step is **required for Uniswap V2**: our Router uses a locally-built Pair whose
> init-code hash differs from mainnet, so the patch updates `UniswapV2Library.pairFor()` accordingly.

## 2. Build the engine (WASM) and the proxy

```bash
( cd koinos-evm/engine && ./build.sh --evm )   # → target/koinos_evm_engine.wasm (~407 KB, MVP-checked)
( cd koinos-evm/rpc    && cargo build --release )
```

## 3. Upload the engine to a Koinos account (one-time)

The engine contract lives at a Koinos account you control. Upload the WASM with `koinos-cli`:

```bash
koinos-cli \
  -x 'connect https://testnet.koinosfoundation.org/jsonrpc' \
  -x 'open <your-wallet-file> <password>' \
  -x 'upload koinos-evm/engine/target/koinos_evm_engine.wasm'
# note the contract account address it prints — that is your ENGINE_CONTRACT
```

> Re-uploading to the same account **upgrades** the engine in place; the EVM KV state is preserved.

## 4. Run the proxy

```bash
cd koinos-evm/rpc
OPERATOR_PRIVKEY_HEX=<operator-key-hex> \
ENGINE_CONTRACT=<your-engine-account> \
LISTEN_ADDR=127.0.0.1:8545 \
RC_LIMIT_MANA=1500000000 \
./target/release/koinos-evm-rpc
```

`RC_LIMIT_MANA=1500000000` is enough for large deploys (a 22 KB pool CREATE2 ≈ 0.26e9 mana). The
default 200M is too low for contract deploys.

### Proxy hardening / persistence knobs (all optional, sensible defaults)

| Env | Default | Meaning |
|---|---|---|
| `DB_PATH` | `./koinos-evm-rpc.sqlite` | Durable SQLite store for txs + receipts/logs/blocks. Makes `eth_getTransactionReceipt`/`ByHash` restart-safe and backs `eth_getLogs` + block bodies. Delete the file to reset (the indexer rebuilds it from chain). |
| `RECEIPT_POLL_SECS` | `3` | Background poller that settles pending receipts into the store (0 = off). |
| `INDEXER_POLL_SECS` / `INDEXER_PAGE_SIZE` | `3` / `50` | Account-history backfill indexer: on first start it indexes the engine's ENTIRE history (any relayer), then tails the head with block-global log indexes. 0 secs = off. |
| `WS_POLL_SECS` | `2` | WebSocket push feeds (`eth_subscribe` newHeads + logs). 0 = pushes off (the WS endpoint still answers regular JSON-RPC). |
| `ESTIMATE_GAS_FALLBACK` | `5000000` | Gas returned by `eth_estimateGas` when the estimation view hits the node's read-compute limit (`-1013`), which happens for ALL writes on a default public node. 0 = disable (error propagates). Harmless to over-estimate: users pay zero gas and relay mana is independent of this figure. |
| `CORS_ALLOWED_ORIGINS` | localhost:8080/3000 | Comma-separated browser-origin allowlist; `"*"` restores fully-permissive CORS (dev only). |
| `RATE_LIMIT_RPS` / `RATE_LIMIT_BURST` | `50` / `500` | Per-client-IP token bucket; one token per JSON-RPC request, batches cost their length (0 rps = off). Defaults are generous because all loopback clients share one bucket. |
| `TRUST_PROXY_HEADERS` | `0` (off) | For deployments behind a reverse proxy ON THE SAME HOST: when the TCP peer is loopback, the rate limiter keys on `X-Real-IP` (set by your proxy) or the last `X-Forwarded-For` hop instead of the peer address — otherwise every visitor shares one bucket. Non-loopback peers always keep their TCP address (headers are forgeable on direct connections). Never enable without a trusted proxy in front. |
| `RPC_MAX_BATCH` / `RPC_MAX_BODY_BYTES` | `100` / `1048576` | JSON-RPC batch-size and HTTP body-size caps. |
| `MIN_GAS_PRICE_WEI` | `0` (off) | Admission floor on the sender-committed gas price (legacy `gas_price` / 1559 `max_fee_per_gas`). The engine still charges 0 ETH — this is an anti-spam gate; `eth_gasPrice`/`eth_feeHistory` advertise the floor so wallets auto-comply. |
| `NONCE_RECONCILE_SECS` | `30` | Periodic operator-nonce reconcile against chain (0 = off; error-triggered resync stays on). |
| `TX_META_MAX` / `TX_META_TTL_SECS` | `10000` / `3600` | In-memory tx-metadata cache bound (hot path in front of the SQLite store). |
| `PENDING_NONCE_MAX` / `PENDING_NONCE_TTL_SECS` | `10000` / `600` | Per-sender pending-nonce map bound; the TTL also heals stale too-high entries. |
| `GETLOGS_MAX_BLOCK_RANGE` / `GETLOGS_MAX_RESULTS` | `10000` / `10000` | `eth_getLogs` caps; exceeding either returns `-32005` so clients auto-chunk. |

## 5. Deploy contracts

All deploy scripts take an EVM deployer key via `DEPLOYER_PK` (0x-prefixed) and target the proxy at
`RPC` (default `http://localhost:8545`). They are self-asserting against baked-in reference values.

```bash
# Faucet tokens + a seeded V2 pair (powers the swap UI)
DEPLOYER_PK=<deployer-key> RPC=http://localhost:8545 ./scripts/shell/deploy_faucet_tokens.sh

# Uniswap V3 core (phases l1..l8) — see TESTING.md for the phased lifecycle
DEPLOYER_PK=<deployer-key> RPC=http://localhost:8545 PHASE=l1 ./scripts/shell/deploy_uniswap_v3.sh

# Uniswap V3 periphery (phases p1..p4)
DEPLOYER_PK=<deployer-key> RPC=http://localhost:8545 PHASE=p1 ./scripts/shell/deploy_v3_periphery.sh

# A V3 pool for the pool-manager UI
DEPLOYER_PK=<deployer-key> RPC=http://localhost:8545 ./scripts/shell/deploy_v3_faucet_pool.sh
```

After deploying, paste the printed addresses into `koinos-evm/ui/config.js` so the UIs point at your
deployment.

### Deploy your OWN contract (Foundry)

Nothing special — point `forge`/`cast` at the running proxy. A brand-new key with zero balance works,
because gas price is 0:

```bash
cast wallet new                          # throwaway EVM key; no funding needed
forge create src/MyContract.sol:MyContract \
  --rpc-url http://localhost:8545 \
  --private-key <key> \
  --gas-limit 8000000 \                  # pass an explicit limit (see gas note)
  --legacy --broadcast
```

Gas notes: `eth_estimateGas` returns a 5M fallback on a default public node (the estimation view hits
the read-compute limit), so set `--gas-limit` explicitly — **≥ 8M for contracts larger than ~10 KB**
(the Factory ≈ 3M, Router02 ≈ 4.8M). `--legacy` avoids 1559 fee fields the chain reports as 0. Reads of
heavy view functions (`QuoterV2`, `positions()`) need a raised-read-limit node — on a default node your
tooling sees `-32005` (the Koinos node reports `-1013` internally).

## 6. Public exposure (TLS reverse proxy)

The proxy binds loopback by default and should STAY on loopback in public deployments — terminate
TLS with a reverse proxy on the same host and forward to it. MetaMask requires `https://` RPC URLs
for non-localhost networks, and the same endpoint serves WebSocket (`eth_subscribe`) on GET, so the
proxy block must pass upgrade headers. Minimal nginx example:

```nginx
# http context (e.g. conf.d/ws-upgrade.conf)
map $http_upgrade $connection_upgrade { default upgrade; '' close; }

# inside your TLS server block
location = /evm-rpc {
    limit_req zone=your_zone burst=40 nodelay;     # edge rate limit (defense in depth)
    proxy_pass http://127.0.0.1:8545/;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;
    proxy_set_header X-Real-IP $remote_addr;        # pairs with TRUST_PROXY_HEADERS=1
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_read_timeout 3600s;                       # long-lived WS subscriptions
}
```

Required env for the proxy behind that block:

```bash
TRUST_PROXY_HEADERS=1                                   # per-IP limiting sees real client IPs
CORS_ALLOWED_ORIGINS=https://your.site,https://www.your.site
```

The bundled UIs are origin-aware: served from any non-localhost origin they call
`{origin}/evm-rpc` (and derive `wss://` for the live feeds) instead of `http://localhost:8545`,
so the same static files work locally and deployed.

**Public-node read limits.** If the proxy points at a default public Koinos node instead of your own
raised-read-limit node, heavy `eth_call` views fail with `-32005` (the node caps read compute at
~10M). Writes, light reads, `eth_getLogs` (served from the proxy's own index), and the WS feeds are
unaffected — the quest page even rebuilds its canvas from event replay when the heavy view is
unavailable. Run your own node with `read-compute-bandwidth-limit` raised if you need Quoter /
`positions()` / other heavy views.

**Operator-key hygiene.** Exactly ONE proxy instance should relay per operator key — concurrent
relays race on the operator's Koinos nonce (each submit self-heals via resync-retry, but
simultaneous submitters will see intermittent failures). Keep the key in a root-only env file
(`chmod 600`), run the service sandboxed (`ProtectSystem=strict`, `MemoryMax=`), and remember the
mempool reserves the full `RC_LIMIT_MANA` per pending tx until irreversibility (~3 min): burst
capacity ≈ operator mana ÷ `RC_LIMIT_MANA`.

## Notes / gotchas

- **Mana, not gas.** Large txs are dominated by the Koinos *network-bandwidth* RC charge, not compute.
  Bump `RC_LIMIT_MANA` if you see `insufficient rc`.
- **EVM gas for deploys.** Use ≥ 8M EVM gas for contracts > 10 KB (Factory ≈ 3M, Router02 ≈ 4.8M).
- **Heavy view calls** (`getAmountsOut`, V3 `positions()`, the `Quoter`) revert `-1013` on the public
  node — see [STATUS.md](STATUS.md). Compute quotes off-chain or run a raised-limit node.
- **Testnet resets** wipe deployments; just re-run the deploy scripts.
