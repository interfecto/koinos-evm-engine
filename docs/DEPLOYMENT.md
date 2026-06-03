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

## Notes / gotchas

- **Mana, not gas.** Large txs are dominated by the Koinos *network-bandwidth* RC charge, not compute.
  Bump `RC_LIMIT_MANA` if you see `insufficient rc`.
- **EVM gas for deploys.** Use ≥ 8M EVM gas for contracts > 10 KB (Factory ≈ 3M, Router02 ≈ 4.8M).
- **Heavy view calls** (`getAmountsOut`, V3 `positions()`, the `Quoter`) revert `-1013` on the public
  node — see [STATUS.md](STATUS.md). Compute quotes off-chain or run a raised-limit node.
- **Testnet resets** wipe deployments; just re-run the deploy scripts.
