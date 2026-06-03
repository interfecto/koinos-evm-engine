#!/usr/bin/env bash
#
# One-time setup of an open-mint TFA/TFB Uniswap V3 0.3% pool on the Koinos EVM
# foundation testnet, for the dApp pool manager (koinos-evm/ui/pool.html).
#
# Creates + initializes the pool (price 1.0, tick 0) and seeds a FULL-RANGE position
# from the deployer, so ANY MetaMask wallet can faucet-mint TFA/TFB and test V3
# (add liquidity / swap / collect) zero-gas against a pool that already has depth.
#
# Idempotent: skips createPool/initialize if already done; the seed mint adds more
# liquidity if re-run (intended only to be run once).
#
# Prereq: the EVM JSON-RPC proxy must be running with a high RC ceiling, e.g.
#   cd koinos-evm/rpc && OPERATOR_PRIVKEY_HEX=... ENGINE_CONTRACT=1E8igxy... \
#     LISTEN_ADDR=127.0.0.1:8545 RC_LIMIT_MANA=1500000000 ./target/release/koinos-evm-rpc
#
# Output: POOL address + seed tokenId to paste into koinos-evm/ui/config.js.
#
set -euo pipefail

RPC="${RPC:-http://localhost:8545}"
# EVM deployer (holds the FaucetToken supply, seeds the pool). Operator pays Koinos mana.
PK="${DEPLOYER_PK:?set DEPLOYER_PK to your EVM deployer key (0x-prefixed 64-hex)}"
DEP="$(cast wallet address --private-key "$PK")"

# Live foundation-testnet addresses (config.js / koinos_evm_uniswap_v3_periphery).
FACTORY="${FACTORY:-0x7be78d086661587b6f62979bd2d5650c45e2eff1}"   # Uniswap V3 Factory
NFPM="${NFPM:-0xd6e62f045a84a77bd9a6176a0f61aa515c131c75}"        # NonfungiblePositionManager
TFA="${TFA:-0x16DE2d12FA6110e38081662b09cCBf99019E46c8}"          # open-mint FaucetToken A
TFB="${TFB:-0x595Fc4e25fb1Ed866d32aa644F08cbA7b9f19348}"          # open-mint FaucetToken B
FEE="${FEE:-3000}"                                                # 0.3% -> tickSpacing 60
# Full-range usable ticks for spacing 60: ±887220 (887272 floored to a multiple of 60).
TL="${TL:--887220}"
TU="${TU:-887220}"
SEED_TOKENS="${SEED_TOKENS:-100000}"     # each token in the seed position
EXTRA_TOKENS="${EXTRA_TOKENS:-200000}"   # deployer self-mint before seeding (open faucet)
SQRTP_1="79228162514264337593543950336"  # 2**96 -> price 1.0 -> tick 0
DEADLINE=9999999999
ZERO="0x0000000000000000000000000000000000000000"

is_addr(){ [[ "$1" =~ ^0x[0-9a-fA-F]{40}$ ]]; }
send(){ cast send --rpc-url "$RPC" --private-key "$PK" --legacy --gas-price 0 "$@"; }
call(){ cast call --rpc-url "$RPC" "$@"; }

SEED_WEI="$(cast to-wei "$SEED_TOKENS")"
EXTRA_WEI="$(cast to-wei "$EXTRA_TOKENS")"

# V3 pools + NFPM require token0 < token1 (160-bit compare — too big for bash arithmetic).
read -r T0 T1 < <(python3 -c "import sys;a,b=sys.argv[1].lower(),sys.argv[2].lower();print(a,b) if int(a,16)<int(b,16) else print(b,a)" "$TFA" "$TFB")
echo ">> deployer=$DEP"
echo ">> token0=$T0  token1=$T1  fee=$FEE  ticks=[$TL,$TU]"

# 1. Create the pool if it doesn't exist yet.
POOL="$(call "$FACTORY" "getPool(address,address,uint24)(address)" "$T0" "$T1" "$FEE")"
if [ "$POOL" = "$ZERO" ] || ! is_addr "$POOL"; then
  echo ">> createPool(TFA,TFB,$FEE)"
  send --gas-limit 6000000 "$FACTORY" "createPool(address,address,uint24)" "$T0" "$T1" "$FEE" >/dev/null
  POOL="$(call "$FACTORY" "getPool(address,address,uint24)(address)" "$T0" "$T1" "$FEE")"
fi
is_addr "$POOL" || { echo "ERROR: no pool address returned: '$POOL'" >&2; exit 1; }
echo "   POOL=$POOL"

# 2. Initialize the price if the pool is still uninitialized (slot0.sqrtPriceX96 == 0).
#    Read the whole tuple then take the first line WITHOUT a pipe (cast + `head` + pipefail
#    can abort on SIGPIPE).
SLOT0_NOW="$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)")"
SQRTP_NOW="${SLOT0_NOW%%$'\n'*}"
if [ "$SQRTP_NOW" = "0" ]; then
  echo ">> initialize(2**96)  (price 1.0, tick 0)"
  send --gas-limit 600000 "$POOL" "initialize(uint160)" "$SQRTP_1" >/dev/null
else
  echo ">> already initialized (sqrtPriceX96=$SQRTP_NOW) — skipping initialize"
fi

# 3. Top up the deployer balance (open faucet) and approve the NFPM for both tokens.
echo ">> mint ${EXTRA_TOKENS} TFA/TFB to deployer + approve NFPM"
send --gas-limit 150000 "$TFA" "mint(address,uint256)" "$DEP" "$EXTRA_WEI" >/dev/null
send --gas-limit 150000 "$TFB" "mint(address,uint256)" "$DEP" "$EXTRA_WEI" >/dev/null
send --gas-limit 120000 "$TFA" "approve(address,uint256)" "$NFPM" "$EXTRA_WEI" >/dev/null
send --gas-limit 120000 "$TFB" "approve(address,uint256)" "$NFPM" "$EXTRA_WEI" >/dev/null

# 4. Seed a full-range position from the deployer (recipient = deployer). Both tokens are
#    used ~equally at tick 0 over a full range, giving the pool immediate two-sided depth.
echo ">> NFPM.mint full-range seed ${SEED_TOKENS}/${SEED_TOKENS}"
send --gas-limit 2000000 "$NFPM" \
  "mint((address,address,uint24,int24,int24,uint256,uint256,uint256,uint256,address,uint256))" \
  "($T0,$T1,$FEE,$TL,$TU,$SEED_WEI,$SEED_WEI,0,0,$DEP,$DEADLINE)" >/dev/null

# 5. Report final state. positions()/ownerOf() are read-blocked on Koinos (Track-T), but
#    slot0/liquidity/totalSupply are light reads that work. tokenId is sequential, so the
#    seed position's id is NFPM.totalSupply() after the mint (no NFTs have been burned).
echo
echo "=== V3 TFA/TFB POOL READY ==="
echo "POOL    = $POOL"
echo "token0  = $T0"
echo "token1  = $T1"
echo "fee     = $FEE   tickSpacing = 60"
echo "slot0   = $(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | tr '\n' ' ')"
echo "liquidity = $(call "$POOL" "liquidity()(uint128)")"
echo "seed tokenId (NFPM.totalSupply) = $(call "$NFPM" "totalSupply()(uint256)")"
echo
echo "Paste POOL into koinos-evm/ui/config.js (V3_POOL.tfaTfb3000)."
