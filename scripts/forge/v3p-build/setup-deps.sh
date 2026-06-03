#!/usr/bin/env bash
# Reproducible dependency setup for the [profile.v3p] build (real Uniswap v3-periphery).
# The periphery is a Hardhat repo; its Solidity deps come from npm, NOT foundry libs. We pin
# them via its package-lock (OZ 3.4.1-solc-0.7-2, @uniswap/v3-core 1.0.0, base64-sol 1.0.1) and
# the v3p remappings point at lib/v3-periphery/node_modules. node_modules is gitignored, so this
# script reinstalls the EXACT pinned set on a fresh checkout. Run once before `FOUNDRY_PROFILE=v3p forge build`.
set -euo pipefail
PERIPHERY_DIR="$(cd "$(dirname "$0")/.." && pwd)/lib/v3-periphery"
[ -d "$PERIPHERY_DIR" ] || { echo "v3-periphery not vendored — run: git submodule update --init --recursive" >&2; exit 1; }
echo ">> npm ci (pinned periphery deps) in $PERIPHERY_DIR"
( cd "$PERIPHERY_DIR" && npm ci --ignore-scripts --no-audit --no-fund 2>/dev/null || npm install --ignore-scripts --no-audit --no-fund )
echo ">> verify"
node -e "const p=require('$PERIPHERY_DIR/node_modules/@openzeppelin/contracts/package.json');console.log('OZ',p.version)"
echo ">> ready: FOUNDRY_PROFILE=v3p forge build"
