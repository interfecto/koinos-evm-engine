#!/usr/bin/env bash
#
# Reproducible deploy + liquidity-seed of the Koinos EVM swap-demo test tokens.
# Deploys two open-mint FaucetTokens (TFA, TFB) and seeds a Uniswap V2 pair
# through the existing (patched-init-hash) Router02. Use this to recover the
# demo after a testnet reset.
#
# Prereq: the EVM JSON-RPC proxy must be running with a high RC ceiling, e.g.
#   cd koinos-evm/rpc && OPERATOR_PRIVKEY_HEX=... ENGINE_CONTRACT=1E8igxy... \
#     LISTEN_ADDR=127.0.0.1:8545 RC_LIMIT_MANA=1500000000 ./target/release/koinos-evm-rpc
#
# Output: prints TFA / TFB / PAIR addresses to paste into koinos-evm/ui/config.js
#
set -euo pipefail

RPC="${RPC:-http://localhost:8545}"
# EVM deployer (holds initial supply, seeds the pool). Operator pays Koinos mana.
PK="${DEPLOYER_PK:?set DEPLOYER_PK to your EVM deployer key (0x-prefixed 64-hex)}"
# Reused live Uniswap V2 Router02 on the foundation testnet (patched init hash).
ROUTER="${ROUTER:-0x884df96ebbb3ab489834e869b533ff049e59e65a}"
SUPPLY="${SUPPLY:-1000000ether}"   # minted to deployer at construction
SEED="${SEED:-500000ether}"        # liquidity added to the pool (1:1)

# DEP must be the PK signer: it both receives the initial supply AND seeds the
# pool, so deriving it from PK avoids minting to one address while addLiquidity
# pulls from another.
DEP="$(cast wallet address --private-key "$PK")"

is_addr() { [[ "$1" =~ ^0x[0-9a-fA-F]{40}$ ]]; }

FORGE_DIR="$(cd "$(dirname "$0")/../forge" && pwd)"
cd "$FORGE_DIR"

echo ">> forge build"
forge build >/dev/null
BC="$(forge inspect FaucetToken bytecode)"

send() { cast send --rpc-url "$RPC" --private-key "$PK" --legacy --gas-price 0 "$@"; }

deploy() { # $1=name $2=symbol -> echoes deployed address
  send --gas-limit 4000000 --json \
    --create "$BC" "constructor(string,string,address,uint256)" "$1" "$2" "$DEP" "$SUPPLY" \
  | jq -r .contractAddress
}

echo ">> deploy TFA"; TFA="$(deploy 'Koinos Faucet A' 'TFA')"; echo "   TFA=$TFA"
is_addr "$TFA" || { echo "ERROR: TFA deploy returned no address: '$TFA'" >&2; exit 1; }
echo ">> deploy TFB"; TFB="$(deploy 'Koinos Faucet B' 'TFB')"; echo "   TFB=$TFB"
is_addr "$TFB" || { echo "ERROR: TFB deploy returned no address: '$TFB'" >&2; exit 1; }

echo ">> approve router for both tokens"
send --gas-limit 120000 "$TFA" "approve(address,uint256)" "$ROUTER" "$SUPPLY" >/dev/null
send --gas-limit 120000 "$TFB" "approve(address,uint256)" "$ROUTER" "$SUPPLY" >/dev/null

echo ">> addLiquidity ${SEED}/${SEED} (deploys the pair via CREATE2)"
send --gas-limit 8000000 "$ROUTER" \
  "addLiquidity(address,address,uint256,uint256,uint256,uint256,address,uint256)" \
  "$TFA" "$TFB" "$SEED" "$SEED" 1 1 "$DEP" 9999999999 >/dev/null

# Derive the factory from the router so the seeded pool and the printed pair
# always come from the same factory.
FACTORY="$(cast call --rpc-url "$RPC" "$ROUTER" "factory()(address)")"
PAIR="$(cast call --rpc-url "$RPC" "$FACTORY" "getPair(address,address)(address)" "$TFA" "$TFB")"

echo
echo "=== DEPLOYED ==="
echo "TFA  = $TFA"
echo "TFB  = $TFB"
echo "PAIR = $PAIR"
echo "reserves:"
cast call --rpc-url "$RPC" "$PAIR" "getReserves()(uint112,uint112,uint32)"
