# Koinos EVM Swap Demo

A minimal browser dApp to **mint test tokens** and **swap** them on the Koinos EVM
testnet via MetaMask. All transactions are **zero-gas**: the JSON-RPC proxy's
operator wallet pays Koinos mana for every relayed tx.

Vanilla HTML/CSS/JS + [ethers v6](https://docs.ethers.org) (ES module from a CDN).
No build step.

## Run

```bash
cd koinos-evm/ui
# OPERATOR_PRIVKEY_HEX must be a funded foundation-testnet operator key — it pays Koinos
# mana for every relayed tx. The repo ships NO default key; supply your own:
OPERATOR_PRIVKEY_HEX=<your-operator-key-hex> ./run.sh   # starts the proxy + serves the UI on :8080
```

Then open <http://localhost:8080> and click **Connect MetaMask**. It will offer to
add/switch to the Koinos EVM network:

| Field | Value |
|---|---|
| Network name | Koinos EVM Testnet |
| RPC URL | `http://localhost:8545` |
| Chain ID | `42069` |
| Currency symbol | `tKOIN` |

> Gas price is 0, so a brand-new account with no balance can transact immediately.

## Use

1. **Faucet** — click *Get 1000 TFA* / *Get 1000 TFB*. Your own wallet signs an open
   `mint()`; the operator pays mana.
2. **Swap** — enter an amount, pick the direction. The quote is computed client-side
   from the pool's `getReserves()` (the on-chain `getAmountsOut` view exceeds Koinos's
   read compute-bandwidth limit). First swap of a token prompts an `approve`, then the
   swap itself. Default slippage 0.5%.

## Quest (quest.html)

A guided four-step onboarding arc for live events: **connect → mint faucet tokens →
swap on real Uniswap V3 → draw on a shared 64×64 pixel canvas**. Every step is
verified ON-CHAIN (balances, the pool's `Swap` event, `pixelsBy()`), so a returning
visitor resumes where they left off. The canvas streams `PixelsSet` events live over
the proxy's WebSocket (`eth_subscribe("logs")`, same port as HTTP) and falls back to
polling. Pixels are batched — up to 128 per `setPixels()` tx. Cost is dominated by
DISTINCT storage words touched (~22k gas each), so the page estimates gas word-aware
and auto-chunks scattered drawings into multiple txs (each V3-swap-sized, proven to
fit). Measured: 128 consecutive px = 1 tx ≈ 1.44 vKOIN; 128 worst-case scattered px
= 5 txs ≈ 8.6 vKOIN; one tx per pixel would be ~75× pricier than the clustered case.
Contract: `scripts/forge/src/PixelCanvas.sol` (20 forge tests).

The quest page uses the **vendored** ethers (`vendor/ethers.umd.min.js`) instead of a
CDN so it works offline / behind a tunnel.

The page is deliberately educational: a testnet banner explains the relay/mana model,
each step has a "How it works" expander, and an **"Under the hood" inspector** shows
every confirmed quest tx on BOTH layers — the signed EVM tx (decoded calldata + events)
next to the wrapping Koinos tx (id, payer, `rc_used` in vKOIN, resource breakdown, links
to koinosblocks + raw REST JSON). The Koinos half is resolved client-side with no proxy
support needed: the eth `blockHash` is the Koinos block id sans `0x1220` multihash prefix,
so one CORS-open REST `/block` fetch + keccak-matching the embedded raw tx recovers the
wrapper (same trick as explorer.js `fillKoinosTxId`).

## Deployed contracts (foundation testnet)

| | Address |
|---|---|
| TFA (Koinos Faucet A) | `0x16DE2d12FA6110e38081662b09cCBf99019E46c8` |
| TFB (Koinos Faucet B) | `0x595Fc4e25fb1Ed866d32aa644F08cbA7b9f19348` |
| Pair (TFA/TFB) | `0xdbe58068f20ccebab16a6fbbbf811740436812c5` |
| UniswapV2 Router02 | `0x884df96ebbb3ab489834e869b533ff049e59e65a` |
| UniswapV2 Factory | `0x603983bf054EEED6eed4262ee696A7b5eA1A00dd` |
| PixelCanvas (quest) | `0x4157cCC46B6B328A1732527ceF7E52E9B7F261e0` |

Addresses live in `config.js`. If the testnet resets, redeploy with
`scripts/shell/deploy_faucet_tokens.sh` and paste the new addresses into `config.js`.

## Notes

- The proxy keeps a **durable SQLite store** (`DB_PATH`, default `./koinos-evm-rpc.sqlite`) for
  txs/receipts/logs/blocks, so `getTransactionByHash`/receipt lookups survive restarts; an indexer
  backfills the engine's entire history on first start. Delete the DB file to reset (it rebuilds
  from chain).
- Reads use a direct provider to `:8545`; writes go through MetaMask. The proxy ships a CORS origin
  allowlist (`CORS_ALLOWED_ORIGINS`, defaulting to the local dev ports; set `"*"` for fully
  permissive dev-only CORS).
- Localhost demo only — `mint()` is intentionally open. Never point this at a network
  where token supply has value.
- `run.sh` **requires** `OPERATOR_PRIVKEY_HEX` — the operator wallet that pays Koinos mana
  for every relayed tx. The repo ships no key; bring a funded foundation-testnet account
  (top it up from the public testnet faucet). Note that this account also owns the deployed
  engine contract, so keep it for testing only — see the security notes in the top-level docs.
