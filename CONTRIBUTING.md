# Contributing / Dev setup

## Toolchain

```bash
# Rust + the MVP-WASM target the engine compiles to
rustup target add wasm32v1-none

# binaryen — wasm-opt / wasm-strip (engine build.sh uses these)
#   macOS:  brew install binaryen
#   Linux:  apt-get install binaryen   (or build from https://github.com/WebAssembly/binaryen)

# Foundry — forge + cast
curl -L https://foundry.paradigm.xyz | bash && foundryup

# Node.js (the Uniswap v3-periphery build is a Hardhat repo; its Solidity deps come from npm)
#   any recent LTS

# koinos-cli (only to upload the engine WASM): https://github.com/koinos/koinos-cli
```

## Build

```bash
git submodule update --init --recursive
( cd scripts/forge && ./apply-patches.sh )           # v2-periphery Pair init-hash (required for V2)
( cd scripts/forge && ./v3p-build/setup-deps.sh )    # v3-periphery npm deps

( cd koinos-evm/engine && ./build.sh --evm )         # WASM engine (MVP-checked)
( cd koinos-evm/rpc    && cargo build --release )     # JSON-RPC proxy

# Foundry — one build per profile:
( cd scripts/forge && forge build )                            # default (helper contracts)
( cd scripts/forge && FOUNDRY_PROFILE=uniswap forge build )    # Uniswap V2  (run apply-patches.sh first)
( cd scripts/forge && FOUNDRY_PROFILE=v3      forge build )    # Uniswap V3 core
( cd scripts/forge && FOUNDRY_PROFILE=v3p     forge build )    # V3 periphery (run setup-deps.sh first)
( cd scripts/forge && FOUNDRY_PROFILE=v3pn    forge build )    # NFPM (low optimizer runs, fits EIP-170)
```

The engine build **fails** if `wasm-opt` leaves any non-MVP opcode in the output (the Koinos Fizzy VM is
MVP-only) — that check is intentional, not flaky.

## Layout

- `koinos-evm/engine/` — the EVM engine (Rust → WASM). Start at `src/lib.rs` (dispatch) → `src/engine.rs`.
- `koinos-evm/rpc/` — the Ethereum-JSON-RPC ↔ Koinos proxy. Start at `src/rpc.rs` (method dispatch).
- `koinos-evm/ui/` — vanilla-JS dApps (no build). `config.js` holds the deployed addresses.
- `scripts/forge/` — Foundry project; build profiles in `foundry.toml`; helper contracts in `*-build/`.
- `scripts/shell/deploy_*.sh` — self-asserting deploy + lifecycle scripts (see `docs/TESTING.md`).

## Secrets — never commit

This repo ships **no keys**. The proxy needs `OPERATOR_PRIVKEY_HEX` and the deploy scripts need
`DEPLOYER_PK`, both supplied at runtime via the environment. Do not add default keys, server IPs, or
wallet files. `.gitignore` blocks `*.wallet`, `*.key`, `*.pem`, and `.env*`; keep it that way.

## Conventions

- Match the surrounding code's style and comment density.
- The deploy/lifecycle scripts assert against a reference EVM — keep new phases self-asserting and
  forward-only (preflight the prior on-chain state before mutating).
- See `docs/STATUS.md` for what's proven vs. open, and please keep claims calibrated.
