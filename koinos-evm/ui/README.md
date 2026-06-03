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

## Deployed contracts (foundation testnet)

| | Address |
|---|---|
| TFA (Koinos Faucet A) | `0x16DE2d12FA6110e38081662b09cCBf99019E46c8` |
| TFB (Koinos Faucet B) | `0x595Fc4e25fb1Ed866d32aa644F08cbA7b9f19348` |
| Pair (TFA/TFB) | `0xdbe58068f20ccebab16a6fbbbf811740436812c5` |
| UniswapV2 Router02 | `0x884df96ebbb3ab489834e869b533ff049e59e65a` |
| UniswapV2 Factory | `0x603983bf054EEED6eed4262ee696A7b5eA1A00dd` |

Addresses live in `config.js`. If the testnet resets, redeploy with
`scripts/shell/deploy_faucet_tokens.sh` and paste the new addresses into `config.js`.

## Notes

- The proxy keeps tx metadata **in memory** — restarting it mid-session breaks
  receipt/`getTransactionByHash` lookups for prior txs. Don't restart while a tx is pending.
- Reads use a direct provider to `:8545`; writes go through MetaMask. The proxy sends
  permissive CORS so the browser can read directly.
- Localhost demo only — `mint()` is intentionally open. Never point this at a network
  where token supply has value.
- `run.sh` **requires** `OPERATOR_PRIVKEY_HEX` — the operator wallet that pays Koinos mana
  for every relayed tx. The repo ships no key; bring a funded foundation-testnet account
  (top it up from the public testnet faucet). Note that this account also owns the deployed
  engine contract, so keep it for testing only — see the security notes in the top-level docs.
