#!/usr/bin/env bash
#
# Reproducible deploy + full core lifecycle of Uniswap V3 on an EVM, via cast.
# Runs phase-by-phase so each layer can be codex-reviewed before the next, with
# discovered addresses persisted to a state file across invocations.
#
#   RPC=http://localhost:8546 PHASE=all ./deploy_uniswap_v3.sh   # anvil, one shot
#   RPC=http://localhost:8545 PHASE=l1  ./deploy_uniswap_v3.sh   # Koinos, deploy+pool
#   RPC=http://localhost:8545 PHASE=l2  ./deploy_uniswap_v3.sh   # Koinos, liquidity
#   RPC=http://localhost:8545 PHASE=l3  ./deploy_uniswap_v3.sh   # Koinos, swaps
#   RPC=http://localhost:8545 PHASE=l4  ./deploy_uniswap_v3.sh   # Koinos, exact-out + price-limit swaps
#   RPC=http://localhost:8545 PHASE=l5  ./deploy_uniswap_v3.sh   # Koinos, burn + collect (position lifecycle)
#   RPC=http://localhost:8545 PHASE=l6  ./deploy_uniswap_v3.sh   # Koinos, swap-fee accrual + fee collection
#   RPC=http://localhost:8545 PHASE=l7  ./deploy_uniswap_v3.sh   # Koinos, second fee tier (500/tickSpacing 10)
#   RPC=http://localhost:8545 PHASE=l8  ./deploy_uniswap_v3.sh   # Koinos, flash swap + protocol fees + oracle cardinality
#
# Against the Koinos proxy, run it with a high RC ceiling (see deploy_faucet_tokens.sh).
#
# REFERENCE (anvil running byte-identical deployed artifacts, deterministic for these params); all hard-asserted:
#   mint P1 [-60,+60]  L=2e21 -> amount0=amount1=5990709911821561876
#   mint P2 [-120,120] L=1e21 -> amount0=amount1=5981737760509662599
#   liquidity after mints      = 3e21 ; tickBitmap initializes -120/-60/+60/+120
#   swap1 1e18 t0 (zeroForOne) -> 996668773744192346 t1 ; tick -7 ; liquidity 3e21 (in-range)
#   swap2 10e18 t0 (zeroForOne) -> 9927869650768784282 t1 ; tick -99 ; liquidity 1e21 (cross -60 down)
#   swap3 6e18 t1 (oneForZero) -> 6016305242464638874 t0 ; tick -33 ; liquidity 3e21 (cross -60 up)
#   L4a exact-OUT 1e18 t1 (zeroForOne) -> in 1006658259387542258 t0 ; tick -40 ; sqrtP 79071223714249479436833528914 (in-range)
#   L4b price-limit oneForZero in<=50e18 cap 1:1 -> consumes 5960419683563667635 t1, out 5954333042144740753 t0 ; tick 0 ; sqrtP == 2^96 (partial fill, stops at limit)
#   L5 mint [-60,60] L=500e18 to minter2 -> amount0=amount1=1497677477955390469 ; liq 3e21->3.5e21
#      burn 250e18 -> liq 3.5e21->3.25e21 (no token xfer) ; collect -> 748838738977695234 each (floor; mint-up/burn-down rounding)
#   L6 swap 2e18 t0 (zeroForOne) -> out 1992777354447763433 t1 ; tick -13 ; fee 6e15 t0 -> fg0 +628213...950956 (fg1 unchanged)
#      minter2 fee share (250e18/3.25e21 = 1/13) -> collect 461538461538461 t0 / 0 t1
#   L7 NEW fee-500/tickSpacing-10 pool (independent): createPool+CREATE2+immutables(maxLiqPerTick 1917569901783203986719870431555990)
#      mint P1'[-100,100]2000e18=9974544141498192268 ea ; P2'[-200,200]1000e18=9949671258790518290 ea ; liq 3e21
#      swapA 1e18 t0 -> 999167110824243722 t1 tick -7 ; swapB cross to tick -110 (limit) -> in 14547189109797622435 out 14460006248296962383 liq 1e21
#   L8 (on 3000-pool, post-L6): (A) flash 5e18/5e18 -> fee 15000000000000000 each, fg0/fg1 +1570534001173562139061728957377391
#      (B) setFeeProtocol(4,4)->feeProtocol 68 (C) swap 2e18 t0 -> out 1990335060226168983 tick -25, protocolFees0 1500000000000000
#      (D) collectProtocol -> 1499999999999999 (1 wei kept) (E) increaseObservationCardinalityNext(5) -> cardinalityNext 5
set -euo pipefail

RPC="${RPC:-http://localhost:8546}"
PHASE="${PHASE:-all}"
STATE_FILE="${STATE_FILE:-/tmp/v3_state.env}"
PK="${DEPLOYER_PK:?set DEPLOYER_PK to your EVM deployer key (0x-prefixed 64-hex)}"
SUPPLY="${SUPPLY:-1000000000000000000000000}"   # 1,000,000e18 per token
FEE=3000                                          # 0.30% tier, tickSpacing 60
SQRTP_1TO1=79228162514264337593543950336          # 2^96 == price 1:1, tick 0
MIN_SQRT_PLUS1=4295128740                          # MIN_SQRT_RATIO + 1 (no zeroForOne limit)
MAX_SQRT_M1=1461446703485210103287273052203988822378723970341  # MAX_SQRT_RATIO - 1 (no oneForZero limit)

L1=2000000000000000000000        # 2000e18  liquidity P1 [-60,+60]
L2=1000000000000000000000        # 1000e18  liquidity P2 [-120,+120]
SWAP1_IN=1000000000000000000     # 1e18  token0 -> stays in [-60,+60]
SWAP2_IN=10000000000000000000    # 10e18 token0 -> crosses tick -60 down into P2-only band
SWAP3_IN=6000000000000000000     # 6e18  token1 (oneForZero) -> crosses tick -60 back up (1e21->3e21)
FUND=100000000000000000000       # 100e18 per token to each helper
L5_MINT=500000000000000000000    # 500e18 fresh position [-60,+60], owned BY minter2 (so it can burn it)
L5_BURN=250000000000000000000    # burn half of the L5 position
MAXU128=340282366920938463463374607431768211455   # type(uint128).max — collect "all owed"
L1_500=2000000000000000000000   # L7 fee-500 tier: P1' [-100,+100]
L2_500=1000000000000000000000   # L7 fee-500 tier: P2' [-200,+200]
LIMIT500=78793625419280018986574222727             # sqrtRatioAtTick(-110): price-limit for the spacing-10 crossing swap
MAXLIQ500=1917569901783203986719870431555990       # canonical maxLiquidityPerTick for tickSpacing 10

G_TOKEN=4000000
G_FACTORY=8000000
G_CREATEPOOL=10000000
G_DEPLOY_HELPER=2000000
G_CALL=2000000

DEP="$(cast wallet address --private-key "$PK")"
is_addr() { [[ "$1" =~ ^0x[0-9a-fA-F]{40}$ ]]; }
lc() { echo "$1" | tr 'A-F' 'a-f'; }
send()  { cast send --rpc-url "$RPC" --private-key "$PK" --legacy --gas-price 0 "$@"; }
call()  { cast call --rpc-url "$RPC" "$@"; }
bal()   { call "$1" "balanceOf(address)(uint256)" "$2" | awk '{print $1}'; }
sub()   { python3 -c "import sys;print(int(sys.argv[1])-int(sys.argv[2]))" "$1" "$2"; }
add()   { python3 -c "import sys;print(int(sys.argv[1])+int(sys.argv[2]))" "$1" "$2"; }
tick()  { call "$1" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '2p'; }
sqrtp() { call "$1" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '1p' | awk '{print $1}'; }
liq_of() { call "$1" "liquidity()(uint128)" | awk '{print $1}'; }
codesz(){ cast codesize --rpc-url "$RPC" "$1" 2>/dev/null || echo '?'; }
# Bounded retry: cast's receipt step intermittently errors on large deploys against anvil.
# A retry can in principle double-deploy if the tx mined but cast failed to return — harmless
# here (we use whichever address comes back; downstream reads the saved address).
addr_of_create() {
  local out a i
  for i in 1 2 3; do
    out="$(send --gas-limit "$1" --json --create "${@:2}" 2>/dev/null || true)"
    a="$(printf '%s' "$out" | jq -r '.contractAddress // empty' 2>/dev/null || true)"
    [[ "$a" =~ ^0x[0-9a-fA-F]{40}$ ]] && { printf '%s' "$a"; return 0; }
  done
  printf '%s' "${a:-}"
}
save()  { printf '%s=%s\n' "$1" "$2" >>"$STATE_FILE"; export "$1=$2"; }
assert_eq() { if [ "$(lc "$2")" != "$(lc "$3")" ]; then echo "   ASSERT FAIL [$1]: got=$2 expect=$3" >&2; exit 1; fi; echo "   ok [$1] = $2"; }
assert_lt() { if [ "$(python3 -c "import sys;print(int(sys.argv[1])<int(sys.argv[2]))" "$2" "$3")" != "True" ]; then echo "   ASSERT FAIL [$1]: $2 !< $3" >&2; exit 1; fi; echo "   ok [$1] $2 < $3"; }
pos_key() { cast keccak "$(cast abi-encode --packed 'f(address,int24,int24)' "$1" "$2" "$3")"; }   # V3 position key
POS_SIG="positions(bytes32)(uint128,uint256,uint256,uint128,uint128)"
# positions() exceeds Koinos read_contract compute-bandwidth — hard-assert where the read
# works (anvil), otherwise NOTE-and-skip (aggregate liquidity() covers correctness).
check_pos_liq() { local v; v="$(call "$POOL" "$POS_SIG" "$2" 2>/dev/null | sed -n '1p' | awk '{print $1}' || true)"; if [[ "$v" =~ ^[0-9]+$ ]]; then assert_eq "$1" "$v" "$3"; else echo "   NOTE [$1]: positions() read over compute-bandwidth on this RPC; covered by aggregate liquidity()"; fi; }

FORGE_DIR="$(cd "$(dirname "$0")/../forge" && pwd)"
cd "$FORGE_DIR"
echo ">> forge build (default + v3 profiles)"; forge build >/dev/null; FOUNDRY_PROFILE=v3 forge build >/dev/null
bc_of() { jq -r '.bytecode.object' "$1"; }
TOKEN_BC="$(bc_of out/TestToken.sol/TestToken.json)"
FACTORY_BC="$(bc_of out-v3/UniswapV3Factory.sol/UniswapV3Factory.json)"
MINTER_BC="$(bc_of out-v3/V3Minter.sol/V3Minter.json)"
SWAPPER_BC="$(bc_of out-v3/V3Swapper.sol/V3Swapper.json)"
FLASH_BC="$(bc_of out-v3/V3Flash.sol/V3Flash.json)"
# Canonical pool init code hash (verified == mainnet); used for offline CREATE2 check.
POOL_INIT_HASH="0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54"

[ -f "$STATE_FILE" ] && source "$STATE_FILE" || true

phase_l1() {
  : >"$STATE_FILE"   # fresh deployment
  echo "== L1: tokens + Factory + createPool + initialize =="
  local TA TB
  TA="$(addr_of_create "$G_TOKEN" "$TOKEN_BC" "constructor(string,string,address,uint256)" "V3 Test A" "V3A" "$DEP" "$SUPPLY")"
  TB="$(addr_of_create "$G_TOKEN" "$TOKEN_BC" "constructor(string,string,address,uint256)" "V3 Test B" "V3B" "$DEP" "$SUPPLY")"
  is_addr "$TA" && is_addr "$TB" || { echo "ERROR token deploy ($TA,$TB)" >&2; exit 1; }
  if [[ "$(lc "$TA")" < "$(lc "$TB")" ]]; then save T0 "$TA"; save T1 "$TB"; else save T0 "$TB"; save T1 "$TA"; fi
  echo "   token0=$T0  token1=$T1"

  FACTORY="$(addr_of_create "$G_FACTORY" "$FACTORY_BC")"
  is_addr "$FACTORY" || { echo "ERROR factory deploy: $FACTORY" >&2; exit 1; }
  save FACTORY "$FACTORY"; echo "   factory=$FACTORY"
  assert_eq factory-code-size  "$(codesz "$FACTORY")" "24535"
  assert_eq feeTickSpacing-3000 "$(call "$FACTORY" "feeAmountTickSpacing(uint24)(int24)" "$FEE")" "60"
  assert_eq factory-owner      "$(call "$FACTORY" "owner()(address)")" "$DEP"

  send --gas-limit "$G_CREATEPOOL" "$FACTORY" "createPool(address,address,uint24)" "$T0" "$T1" "$FEE" >/dev/null
  POOL="$(call "$FACTORY" "getPool(address,address,uint24)(address)" "$T0" "$T1" "$FEE")"
  is_addr "$POOL" || { echo "ERROR getPool: $POOL" >&2; exit 1; }
  save POOL "$POOL"; echo "   pool=$POOL"
  local EXP; EXP="$(cast compute-address --salt "$(cast keccak "$(cast abi-encode 'f(address,address,uint24)' "$T0" "$T1" "$FEE")")" --init-code-hash "$POOL_INIT_HASH" "$FACTORY" | grep -oE '0x[0-9a-fA-F]{40}')"
  assert_eq create2-derivation "$POOL" "$EXP"
  assert_eq pool-code-size     "$(codesz "$POOL")" "22142"
  # NB: no runtime code-hash check — V3 pools inline immutables (factory/token0/token1/
  # fee/tickSpacing/maxLiquidityPerTick) into runtime, so it never equals the compiled
  # deployedBytecode. create2-derivation above already proves creation code == canonical
  # 0xe34f...; the immutable getters below verify the rest.
  assert_eq pool-factory       "$(call "$POOL" "factory()(address)")" "$FACTORY"
  assert_eq pool-token0        "$(call "$POOL" "token0()(address)")" "$T0"
  assert_eq pool-token1        "$(call "$POOL" "token1()(address)")" "$T1"
  assert_eq pool-fee           "$(call "$POOL" "fee()(uint24)")" "$FEE"
  assert_eq pool-tickSpacing   "$(call "$POOL" "tickSpacing()(int24)")" "60"
  assert_eq pool-maxLiqPerTick "$(call "$POOL" "maxLiquidityPerTick()(uint128)" | awk '{print $1}')" "11505743598341114571880798222544994"
  assert_eq getPool-reverse    "$(call "$FACTORY" "getPool(address,address,uint24)(address)" "$T1" "$T0" "$FEE")" "$POOL"
  send --gas-limit "$G_CALL" "$POOL" "initialize(uint160)" "$SQRTP_1TO1" >/dev/null
  local S0; S0="$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)")"
  assert_eq slot0-sqrtP    "$(printf '%s' "$S0" | sed -n '1p' | awk '{print $1}')" "$SQRTP_1TO1"
  assert_eq slot0-tick     "$(printf '%s' "$S0" | sed -n '2p')" "0"
  assert_eq slot0-unlocked "$(printf '%s' "$S0" | sed -n '7p')" "true"
}

phase_l2() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${POOL:-}" ] || { echo "ERROR: no POOL in $STATE_FILE — run L1 first" >&2; exit 1; }
  echo "== L2: V3Minter + two concentrated positions =="
  MINTER="$(addr_of_create "$G_DEPLOY_HELPER" "$MINTER_BC" "constructor(address)" "$POOL")"
  is_addr "$MINTER" || { echo "ERROR minter deploy: $MINTER" >&2; exit 1; }
  save MINTER "$MINTER"; echo "   minter=$MINTER"
  assert_eq minter-owner  "$(call "$MINTER" "owner()(address)")" "$DEP"
  assert_eq minter-pool   "$(call "$MINTER" "pool()(address)")" "$POOL"
  assert_eq minter-token0 "$(call "$MINTER" "token0()(address)")" "$T0"
  assert_eq minter-token1 "$(call "$MINTER" "token1()(address)")" "$T1"
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$MINTER" "$FUND" >/dev/null
  send --gas-limit "$G_CALL" "$T1" "transfer(address,uint256)" "$MINTER" "$FUND" >/dev/null

  local p0b p1b p0a p1a
  p0b=$(bal "$T0" "$POOL"); p1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$MINTER" "mint(address,int24,int24,uint128)" "$DEP" -60 60 "$L1" >/dev/null
  p0a=$(bal "$T0" "$POOL"); p1a=$(bal "$T1" "$POOL")
  echo "   mint P1 [-60,+60]:"
  assert_eq mintP1-amount0 "$(sub "$p0a" "$p0b")" "5990709911821561876"
  assert_eq mintP1-amount1 "$(sub "$p1a" "$p1b")" "5990709911821561876"
  check_pos_liq P1-position-liq "$(pos_key "$DEP" -60 60)" "$L1"

  p0b=$(bal "$T0" "$POOL"); p1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$MINTER" "mint(address,int24,int24,uint128)" "$DEP" -120 120 "$L2" >/dev/null
  p0a=$(bal "$T0" "$POOL"); p1a=$(bal "$T1" "$POOL")
  echo "   mint P2 [-120,120]:"
  assert_eq mintP2-amount0 "$(sub "$p0a" "$p0b")" "5981737760509662599"
  assert_eq mintP2-amount1 "$(sub "$p1a" "$p1b")" "5981737760509662599"
  check_pos_liq P2-position-liq "$(pos_key "$DEP" -120 120)" "$L2"

  assert_eq liquidity-after-mints "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"
  # tickBitmap reads are light (single SLOAD) and work on Koinos where positions()/ticks()
  # exceed compute-bandwidth. word0 bits{1,2}=ticks +60/+120; word-1 bits{254,255}=ticks -120/-60.
  assert_eq tickbitmap-word0  "$(call "$POOL" "tickBitmap(int16)(uint256)" -- 0  | awk '{print $1}')" "6"
  assert_eq tickbitmap-wordm1 "$(call "$POOL" "tickBitmap(int16)(uint256)" -- -1 | awk '{print $1}')" "86844066927987146567678238756515930889952488499230423029593188005934847229952"
}

phase_l3() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${POOL:-}" ] || { echo "ERROR: no POOL in $STATE_FILE — run L1 first" >&2; exit 1; }
  echo "== L3: V3Swapper + in-range and tick-crossing swaps =="
  SWAPPER="$(addr_of_create "$G_DEPLOY_HELPER" "$SWAPPER_BC" "constructor(address)" "$POOL")"
  is_addr "$SWAPPER" || { echo "ERROR swapper deploy: $SWAPPER" >&2; exit 1; }
  save SWAPPER "$SWAPPER"; echo "   swapper=$SWAPPER"
  assert_eq swapper-owner  "$(call "$SWAPPER" "owner()(address)")" "$DEP"
  assert_eq swapper-pool   "$(call "$SWAPPER" "pool()(address)")" "$POOL"
  assert_eq swapper-token0 "$(call "$SWAPPER" "token0()(address)")" "$T0"
  assert_eq swapper-token1 "$(call "$SWAPPER" "token1()(address)")" "$T1"
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$SWAPPER" "$FUND" >/dev/null

  local ps0b ps1b ps0a ps1a
  ps0b=$(bal "$T0" "$POOL"); ps1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" true "$SWAP1_IN" "$MIN_SQRT_PLUS1" >/dev/null
  ps0a=$(bal "$T0" "$POOL"); ps1a=$(bal "$T1" "$POOL")
  echo "   swap1 (in-range):"
  assert_eq swap1-in        "$(sub "$ps0a" "$ps0b")" "$SWAP1_IN"
  assert_eq swap1-out       "$(sub "$ps1b" "$ps1a")" "996668773744192346"
  assert_eq swap1-tick      "$(tick "$POOL")" "-7"
  assert_eq swap1-liquidity "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"

  ps0b=$(bal "$T0" "$POOL"); ps1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" true "$SWAP2_IN" "$MIN_SQRT_PLUS1" >/dev/null
  ps0a=$(bal "$T0" "$POOL"); ps1a=$(bal "$T1" "$POOL")
  echo "   swap2 (tick-crossing DOWN, zeroForOne):"
  assert_eq swap2-in        "$(sub "$ps0a" "$ps0b")" "$SWAP2_IN"
  assert_eq swap2-out       "$(sub "$ps1b" "$ps1a")" "9927869650768784282"
  assert_eq swap2-tick      "$(tick "$POOL")" "-99"
  assert_eq swap2-sqrtP     "$(sqrtp "$POOL")" "78837264347043311077509218897"
  assert_eq swap2-liquidity "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$L2"

  # swap3: oneForZero exact-input (pays token1) crossing tick -60 UPWARD -> liquidity 1e21->3e21.
  # Exercises the up-direction crossing AND the swapper's amount1Delta>0 (token1) payment branch.
  send --gas-limit "$G_CALL" "$T1" "transfer(address,uint256)" "$SWAPPER" "$FUND" >/dev/null
  ps0b=$(bal "$T0" "$POOL"); ps1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" false "$SWAP3_IN" "$MAX_SQRT_M1" >/dev/null
  ps0a=$(bal "$T0" "$POOL"); ps1a=$(bal "$T1" "$POOL")
  echo "   swap3 (tick-crossing UP, oneForZero):"
  assert_eq swap3-in        "$(sub "$ps1a" "$ps1b")" "$SWAP3_IN"
  assert_eq swap3-out       "$(sub "$ps0b" "$ps0a")" "6016305242464638874"
  assert_eq swap3-tick      "$(tick "$POOL")" "-33"
  assert_eq swap3-sqrtP     "$(sqrtp "$POOL")" "79097633101754234216031376898"
  assert_eq swap3-liquidity "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"
}

phase_l4() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${POOL:-}" ]    || { echo "ERROR: no POOL in $STATE_FILE — run L1 first" >&2; exit 1; }
  [ -n "${SWAPPER:-}" ] || { echo "ERROR: no SWAPPER in $STATE_FILE — run L3 first" >&2; exit 1; }
  echo "== L4: exact-output + price-limit (partial-fill) swaps (continues from post-L3 tick -33) =="
  # PREFLIGHT (forward-only safety): refuse to mutate the live, persisted pool unless it is EXACTLY
  # at the post-L3 state. A double-run or any unexpected intervening swap is caught HERE, before any
  # state-changing tx — so the irreversible Koinos pool is never advanced from the wrong base.
  echo "   preflight: pool must be at post-L3 state before any swap"
  assert_eq l4-pre-tick "$(tick "$POOL")" "-33"
  assert_eq l4-pre-sqrtP "$(sqrtp "$POOL")" "79097633101754234216031376898"
  assert_eq l4-pre-liq  "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"
  # Top up the swapper so L4 is self-contained against the live (persisted) Koinos pool. The
  # swapper's own balance never enters an assertion — only pool balance DELTAS do — so a larger
  # balance can't change any expected value; it just guarantees the callback can pay.
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$SWAPPER" "$FUND" >/dev/null
  send --gas-limit "$G_CALL" "$T1" "transfer(address,uint256)" "$SWAPPER" "$FUND" >/dev/null

  # L4a — EXACT-OUTPUT swap (negative amountSpecified): deliver exactly 1e18 token1 out
  # (zeroForOne); the pool pulls the computed token0 input. Stays inside [-60,+60] (no crossing).
  local a0b a1b a0a a1a
  a0b=$(bal "$T0" "$POOL"); a1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" true -1000000000000000000 "$MIN_SQRT_PLUS1" >/dev/null
  a0a=$(bal "$T0" "$POOL"); a1a=$(bal "$T1" "$POOL")
  echo "   L4a exact-output (zeroForOne, exact out 1e18 t1):"
  assert_eq l4a-out-exact "$(sub "$a1b" "$a1a")" "1000000000000000000"
  assert_eq l4a-in        "$(sub "$a0a" "$a0b")" "1006658259387542258"
  assert_eq l4a-tick      "$(tick "$POOL")" "-40"
  assert_eq l4a-sqrtP     "$(sqrtp "$POOL")" "79071223714249479436833528914"
  assert_eq l4a-liquidity "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"

  # L4b — PRICE-LIMIT partial fill (oneForZero): request 50e18 token1 in but cap at the 1:1 price.
  # The swap consumes only what's needed to reach the limit and stops; the rest of the input is
  # never charged. Proves the price-limit early-exit path AND the oneForZero direction here.
  local b0b b1b b0a b1a
  b0b=$(bal "$T0" "$POOL"); b1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" false 50000000000000000000 "$SQRTP_1TO1" >/dev/null
  b0a=$(bal "$T0" "$POOL"); b1a=$(bal "$T1" "$POOL")
  echo "   L4b price-limit partial fill (oneForZero, in<=50e18 t1, cap 1:1):"
  assert_lt l4b-is-partial     "$(sub "$b1a" "$b1b")" "50000000000000000000"  # consumed < 50e18 requested
  assert_eq l4b-in-partial     "$(sub "$b1a" "$b1b")" "5960419683563667635"
  assert_eq l4b-out            "$(sub "$b0b" "$b0a")" "5954333042144740753"
  assert_eq l4b-stops-at-limit "$(sqrtp "$POOL")" "$SQRTP_1TO1"
  assert_eq l4b-tick           "$(tick "$POOL")" "0"
  assert_eq l4b-liquidity      "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"
}

phase_l5() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${POOL:-}" ] || { echo "ERROR: no POOL in $STATE_FILE — run L1 first" >&2; exit 1; }
  echo "== L5: burn + collect via a fresh owner-guarded minter (continues from post-L4b tick 0) =="
  # PREFLIGHT (forward-only safety): pool must be EXACTLY at the post-L4b state before we mint.
  echo "   preflight: pool must be at post-L4b state before mint"
  assert_eq l5-pre-tick  "$(tick "$POOL")" "0"
  assert_eq l5-pre-sqrtP "$(sqrtp "$POOL")" "$SQRTP_1TO1"
  assert_eq l5-pre-liq   "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$L1" "$L2")"
  # Pool reserves are a tighter fingerprint than (tick,sqrtP,liq): they also catch any unexpected
  # fee accrual / out-of-band interaction that leaves price+liquidity unchanged. Identical anvil==Koinos.
  assert_eq l5-pre-bal0  "$(bal "$T0" "$POOL")" "12008467647109387106"
  assert_eq l5-pre-bal1  "$(bal "$T1" "$POOL")" "12008328931381915482"

  # A SECOND minter that owns (and thus can burn/collect) its own position. The original L2 minter
  # owns P1/P2; rather than disturb them we prove the full lifecycle on a fresh position. In V3,
  # pool.mint keys the position by `recipient` while pool.burn keys by `msg.sender` — so the minting
  # contract MUST be the recipient to be able to burn later. (Found via an 'LS' addDelta underflow.)
  # Reuse a saved+validated MINTER2 (so a re-run after a mid-phase failure doesn't strand a second
  # funded helper); the liquidity preflight above already blocks a double-mint.
  if [ -n "${MINTER2:-}" ] && is_addr "${MINTER2:-}" \
     && [ "$(lc "$(call "$MINTER2" "pool()(address)" 2>/dev/null || echo x)")"  = "$(lc "$POOL")" ] \
     && [ "$(lc "$(call "$MINTER2" "owner()(address)" 2>/dev/null || echo x)")" = "$(lc "$DEP")" ]; then
    echo "   reusing saved minter2=$MINTER2"
  else
    MINTER2="$(addr_of_create "$G_DEPLOY_HELPER" "$MINTER_BC" "constructor(address)" "$POOL")"
    is_addr "$MINTER2" || { echo "ERROR minter2 deploy: $MINTER2" >&2; exit 1; }
    save MINTER2 "$MINTER2"; echo "   minter2=$MINTER2"
  fi
  assert_eq minter2-owner "$(call "$MINTER2" "owner()(address)")" "$DEP"
  assert_eq minter2-pool  "$(call "$MINTER2" "pool()(address)")" "$POOL"
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$MINTER2" "$FUND" >/dev/null
  send --gas-limit "$G_CALL" "$T1" "transfer(address,uint256)" "$MINTER2" "$FUND" >/dev/null

  # MINT to MINTER2 itself.
  local m0b m1b
  m0b=$(bal "$T0" "$POOL"); m1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$MINTER2" "mint(address,int24,int24,uint128)" "$MINTER2" -60 60 "$L5_MINT" >/dev/null
  echo "   L5 mint [-60,+60] L=500e18 (owner=minter2):"
  assert_eq l5-mint-amount0   "$(sub "$(bal "$T0" "$POOL")" "$m0b")" "1497677477955390469"
  assert_eq l5-mint-amount1   "$(sub "$(bal "$T1" "$POOL")" "$m1b")" "1497677477955390469"
  assert_eq l5-liq-after-mint "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$(add "$L1" "$L2")" "$L5_MINT")"
  check_pos_liq l5-pos-liq-after-mint "$(pos_key "$MINTER2" -60 60)" "$L5_MINT"

  # BURN half — credits principal to tokensOwed; NO token transfer yet (pool balances unchanged).
  local bp0 bp1
  bp0=$(bal "$T0" "$POOL"); bp1=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$MINTER2" "burn(int24,int24,uint128)" -60 60 "$L5_BURN" >/dev/null
  local L5_REMAIN; L5_REMAIN="$(sub "$L5_MINT" "$L5_BURN")"   # liquidity left in the L5 position
  echo "   L5 burn half (250e18):"
  assert_eq l5-liq-after-burn "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$(add "$L1" "$L2")" "$L5_REMAIN")"
  assert_eq l5-burn-no-xfer0  "$(sub "$(bal "$T0" "$POOL")" "$bp0")" "0"
  assert_eq l5-burn-no-xfer1  "$(sub "$(bal "$T1" "$POOL")" "$bp1")" "0"
  check_pos_liq l5-pos-liq-after-burn "$(pos_key "$MINTER2" -60 60)" "$L5_REMAIN"

  # COLLECT owed to the deployer — withdraws the burned principal (floor; no fees accrued this phase).
  local c0b c1b d0b d1b
  c0b=$(bal "$T0" "$POOL"); c1b=$(bal "$T1" "$POOL"); d0b=$(bal "$T0" "$DEP"); d1b=$(bal "$T1" "$DEP")
  send --gas-limit "$G_CALL" "$MINTER2" "collect(address,int24,int24,uint128,uint128)" "$DEP" -60 60 "$MAXU128" "$MAXU128" >/dev/null
  echo "   L5 collect to deployer:"
  assert_eq l5-collect-pool0 "$(sub "$c0b" "$(bal "$T0" "$POOL")")" "748838738977695234"
  assert_eq l5-collect-pool1 "$(sub "$c1b" "$(bal "$T1" "$POOL")")" "748838738977695234"
  assert_eq l5-collect-dep0  "$(sub "$(bal "$T0" "$DEP")" "$d0b")" "748838738977695234"
  assert_eq l5-collect-dep1  "$(sub "$(bal "$T1" "$DEP")" "$d1b")" "748838738977695234"
  assert_eq l5-liq-final     "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$(add "$(add "$L1" "$L2")" "$L5_REMAIN")"
}

phase_l6() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${POOL:-}" ]    || { echo "ERROR: no POOL in $STATE_FILE — run L1 first" >&2; exit 1; }
  [ -n "${SWAPPER:-}" ] || { echo "ERROR: no SWAPPER in $STATE_FILE — run L3 first" >&2; exit 1; }
  [ -n "${MINTER2:-}" ] || { echo "ERROR: no MINTER2 in $STATE_FILE — run L5 first" >&2; exit 1; }
  echo "== L6: swap-fee accrual (feeGrowthGlobal) + position fee collection (continues from post-L5) =="
  local L6_LIQ FG0_BEFORE FG1_BEFORE
  L6_LIQ="$(add "$(add "$L1" "$L2")" "$(sub "$L5_MINT" "$L5_BURN")")"   # 3.25e21 (P1+P2 + minter2 250e18)
  FG0_BEFORE=5419436605672002120294400166531547
  FG1_BEFORE=5393146338779220563933678391096783
  # PREFLIGHT (forward-only): exact post-L5 fingerprint INCLUDING accumulated feeGrowthGlobal.
  echo "   preflight: pool must be at post-L5 state (incl. feeGrowthGlobal)"
  assert_eq l6-pre-tick "$(tick "$POOL")" "0"
  assert_eq l6-pre-sqrtP "$(sqrtp "$POOL")" "$SQRTP_1TO1"
  assert_eq l6-pre-liq  "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$L6_LIQ"
  assert_eq l6-pre-bal0 "$(bal "$T0" "$POOL")" "12757306386087082341"
  assert_eq l6-pre-bal1 "$(bal "$T1" "$POOL")" "12757167670359610717"
  assert_eq l6-pre-fg0  "$(call "$POOL" "feeGrowthGlobal0X128()(uint256)" | awk '{print $1}')" "$FG0_BEFORE"
  assert_eq l6-pre-fg1  "$(call "$POOL" "feeGrowthGlobal1X128()(uint256)" | awk '{print $1}')" "$FG1_BEFORE"
  # feeProtocol must be 0 here — a nonzero value (set out-of-band; L8 sets it later) would divert
  # part of the swap fee to protocolFees and break the LP-fee assertions AFTER mutation. slot0 field 6.
  assert_eq l6-pre-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n 6p)" "0"

  # Generate fees with an in-range zeroForOne swap of 2e18 token0 (fee = 0.3% = 6e15 token0).
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$SWAPPER" "$FUND" >/dev/null
  local s0b s1b
  s0b=$(bal "$T0" "$POOL"); s1b=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" true 2000000000000000000 "$MIN_SQRT_PLUS1" >/dev/null
  echo "   L6 swap 2e18 t0 (zeroForOne, in-range):"
  assert_eq l6-swap-in  "$(sub "$(bal "$T0" "$POOL")" "$s0b")" "2000000000000000000"
  assert_eq l6-swap-out "$(sub "$s1b" "$(bal "$T1" "$POOL")")" "1992777354447763433"
  assert_eq l6-swap-tick "$(tick "$POOL")" "-13"
  assert_eq l6-swap-liq "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$L6_LIQ"
  # token0 fee grows feeGrowthGlobal0; feeGrowthGlobal1 unchanged (one-sided zeroForOne fee).
  assert_eq l6-fg0-after     "$(call "$POOL" "feeGrowthGlobal0X128()(uint256)" | awk '{print $1}')" "6047650206141426975919091749482503"
  assert_eq l6-fg1-unchanged "$(call "$POOL" "feeGrowthGlobal1X128()(uint256)" | awk '{print $1}')" "$FG1_BEFORE"

  # Poke MINTER2's [-60,+60] position (burn 0) to realize its fee share into tokensOwed, then collect.
  # Its 250e18 of 3.25e21 active liquidity == 1/13 of the 6e15 token0 fee = 461538461538461 (floor).
  send --gas-limit "$G_CALL" "$MINTER2" "burn(int24,int24,uint128)" -60 60 0 >/dev/null
  local f0b f1b fd0b fd1b
  f0b=$(bal "$T0" "$POOL"); f1b=$(bal "$T1" "$POOL"); fd0b=$(bal "$T0" "$DEP"); fd1b=$(bal "$T1" "$DEP")
  send --gas-limit "$G_CALL" "$MINTER2" "collect(address,int24,int24,uint128,uint128)" "$DEP" -60 60 "$MAXU128" "$MAXU128" >/dev/null
  echo "   L6 MINTER2 fee collect (burn0 poke + collect):"
  assert_eq l6-fee-pool0 "$(sub "$f0b" "$(bal "$T0" "$POOL")")" "461538461538461"
  assert_eq l6-fee-pool1 "$(sub "$f1b" "$(bal "$T1" "$POOL")")" "0"
  assert_eq l6-fee-dep0  "$(sub "$(bal "$T0" "$DEP")" "$fd0b")" "461538461538461"
  assert_eq l6-fee-dep1  "$(sub "$(bal "$T1" "$DEP")" "$fd1b")" "0"
  assert_eq l6-liq-after-collect "$(call "$POOL" "liquidity()(uint128)" | awk '{print $1}')" "$L6_LIQ"
}

phase_l7() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${FACTORY:-}" ] || { echo "ERROR: no FACTORY in $STATE_FILE — run L1 first" >&2; exit 1; }
  echo "== L7: second fee tier — fee 500 / tickSpacing 10 (NEW independent pool via same factory) =="
  # PREFLIGHT (forward-only): the 500-tier pool must NOT exist yet. A re-run aborts here (pool exists)
  # before the irreversible createPool. The factory must already enable the 500 tier (tickSpacing 10).
  # ONE-SHOT POLICY (codex-reviewed, deliberate): after createPool mines, L7 is NOT resumable — the
  # subsequent mint/swap steps aren't idempotent, so partial reuse could double-mint. If a live run
  # fails mid-phase, do NOT re-run PHASE=l7; instead read POOL500's slot0/liquidity/tickBitmap to see
  # how far it got and hand-run only the remaining steps. Anvil PHASE=all fully de-risks the logic first.
  echo "   preflight: 500-tier pool must not exist yet"
  assert_eq l7-pre-nopool         "$(call "$FACTORY" "getPool(address,address,uint24)(address)" "$T0" "$T1" 500)" "0x0000000000000000000000000000000000000000"
  assert_eq l7-feeTickSpacing-500 "$(call "$FACTORY" "feeAmountTickSpacing(uint24)(int24)" 500)" "10"

  send --gas-limit "$G_CREATEPOOL" "$FACTORY" "createPool(address,address,uint24)" "$T0" "$T1" 500 >/dev/null
  POOL500="$(call "$FACTORY" "getPool(address,address,uint24)(address)" "$T0" "$T1" 500)"
  is_addr "$POOL500" || { echo "ERROR getPool 500: $POOL500" >&2; exit 1; }
  save POOL500 "$POOL500"; echo "   pool500=$POOL500"
  local EXP500; EXP500="$(cast compute-address --salt "$(cast keccak "$(cast abi-encode 'f(address,address,uint24)' "$T0" "$T1" 500)")" --init-code-hash "$POOL_INIT_HASH" "$FACTORY" | grep -oE '0x[0-9a-fA-F]{40}')"
  assert_eq l7-create2       "$POOL500" "$EXP500"
  assert_eq l7-codesize      "$(codesz "$POOL500")" "22142"
  assert_eq l7-factory       "$(call "$POOL500" "factory()(address)")" "$FACTORY"
  assert_eq l7-token0        "$(call "$POOL500" "token0()(address)")" "$T0"
  assert_eq l7-token1        "$(call "$POOL500" "token1()(address)")" "$T1"
  assert_eq l7-fee           "$(call "$POOL500" "fee()(uint24)")" "500"
  assert_eq l7-tickSpacing   "$(call "$POOL500" "tickSpacing()(int24)")" "10"
  assert_eq l7-maxLiqPerTick "$(call "$POOL500" "maxLiquidityPerTick()(uint128)" | awk '{print $1}')" "$MAXLIQ500"
  send --gas-limit "$G_CALL" "$POOL500" "initialize(uint160)" "$SQRTP_1TO1" >/dev/null
  assert_eq l7-init-tick     "$(tick "$POOL500")" "0"
  assert_eq l7-init-unlocked "$(call "$POOL500" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '7p')" "true"

  # minter bound to the 500 pool + two concentrated positions at spacing-10 ticks
  MINTER500="$(addr_of_create "$G_DEPLOY_HELPER" "$MINTER_BC" "constructor(address)" "$POOL500")"
  is_addr "$MINTER500" || { echo "ERROR minter500 deploy: $MINTER500" >&2; exit 1; }
  save MINTER500 "$MINTER500"; echo "   minter500=$MINTER500"
  assert_eq minter500-pool "$(call "$MINTER500" "pool()(address)")" "$POOL500"
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$MINTER500" "$FUND" >/dev/null
  send --gas-limit "$G_CALL" "$T1" "transfer(address,uint256)" "$MINTER500" "$FUND" >/dev/null
  local q0 q1
  q0=$(bal "$T0" "$POOL500"); q1=$(bal "$T1" "$POOL500")
  send --gas-limit "$G_CALL" "$MINTER500" "mint(address,int24,int24,uint128)" "$MINTER500" -100 100 "$L1_500" >/dev/null
  echo "   mint P1' [-100,+100] L=2000e18:"
  assert_eq l7-mintP1-amount0 "$(sub "$(bal "$T0" "$POOL500")" "$q0")" "9974544141498192268"
  assert_eq l7-mintP1-amount1 "$(sub "$(bal "$T1" "$POOL500")" "$q1")" "9974544141498192268"
  q0=$(bal "$T0" "$POOL500"); q1=$(bal "$T1" "$POOL500")
  send --gas-limit "$G_CALL" "$MINTER500" "mint(address,int24,int24,uint128)" "$MINTER500" -200 200 "$L2_500" >/dev/null
  echo "   mint P2' [-200,+200] L=1000e18:"
  assert_eq l7-mintP2-amount0 "$(sub "$(bal "$T0" "$POOL500")" "$q0")" "9949671258790518290"
  assert_eq l7-mintP2-amount1 "$(sub "$(bal "$T1" "$POOL500")" "$q1")" "9949671258790518290"
  assert_eq l7-liq-after-mints "$(liq_of "$POOL500")" "$(add "$L1_500" "$L2_500")"
  # spacing-10 tickbitmap: compressed ticks +10/+20 (word0 bits 10,20 = 1049600); -10/-20 (word-1 bits 246,236).
  assert_eq l7-tickbitmap-word0  "$(call "$POOL500" "tickBitmap(int16)(uint256)" -- 0  | awk '{print $1}')" "1049600"
  assert_eq l7-tickbitmap-wordm1 "$(call "$POOL500" "tickBitmap(int16)(uint256)" -- -1 | awk '{print $1}')" "113188640087365246113929996141343217420198187143594339504665397270308454400"

  # swapper bound to the 500 pool: in-range + a price-limited tick-crossing swap (crosses -100 down)
  SWAPPER500="$(addr_of_create "$G_DEPLOY_HELPER" "$SWAPPER_BC" "constructor(address)" "$POOL500")"
  is_addr "$SWAPPER500" || { echo "ERROR swapper500 deploy: $SWAPPER500" >&2; exit 1; }
  save SWAPPER500 "$SWAPPER500"; echo "   swapper500=$SWAPPER500"
  assert_eq swapper500-pool "$(call "$SWAPPER500" "pool()(address)")" "$POOL500"
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$SWAPPER500" "$FUND" >/dev/null
  local r1b
  r1b=$(bal "$T1" "$POOL500")
  send --gas-limit "$G_CALL" "$SWAPPER500" "swap(address,bool,int256,uint160)" "$DEP" true 1000000000000000000 "$MIN_SQRT_PLUS1" >/dev/null
  echo "   swapA (in-range 1e18 t0):"
  assert_eq l7-swapA-out  "$(sub "$r1b" "$(bal "$T1" "$POOL500")")" "999167110824243722"
  assert_eq l7-swapA-tick "$(tick "$POOL500")" "-7"
  assert_eq l7-swapA-liq  "$(liq_of "$POOL500")" "$(add "$L1_500" "$L2_500")"
  local r0b2 r1b2
  r0b2=$(bal "$T0" "$POOL500"); r1b2=$(bal "$T1" "$POOL500")
  send --gas-limit "$G_CALL" "$SWAPPER500" "swap(address,bool,int256,uint160)" "$DEP" true 50000000000000000000 "$LIMIT500" >/dev/null
  echo "   swapB (price-limited crossing to tick -110, drops to 1e21):"
  assert_eq l7-swapB-in    "$(sub "$(bal "$T0" "$POOL500")" "$r0b2")" "14547189109797622435"
  assert_eq l7-swapB-out   "$(sub "$r1b2" "$(bal "$T1" "$POOL500")")" "14460006248296962383"
  assert_eq l7-swapB-tick  "$(tick "$POOL500")" "-110"
  assert_eq l7-swapB-sqrtP "$(sqrtp "$POOL500")" "$LIMIT500"
  assert_eq l7-swapB-liq   "$(liq_of "$POOL500")" "$L2_500"
}

phase_l8() {
  [ -f "$STATE_FILE" ] && source "$STATE_FILE" || true
  [ -n "${POOL:-}" ]    || { echo "ERROR: no POOL in $STATE_FILE — run L1 first" >&2; exit 1; }
  [ -n "${SWAPPER:-}" ] || { echo "ERROR: no SWAPPER in $STATE_FILE — run L3 first" >&2; exit 1; }
  echo "== L8: flash swap + protocol fees + oracle cardinality (3000-pool, continues from post-L6) =="
  local L6_LIQ; L6_LIQ="$(add "$(add "$L1" "$L2")" "$(sub "$L5_MINT" "$L5_BURN")")"   # 3.25e21
  # PREFLIGHT (forward-only): exact post-L6 fingerprint. feeProtocol MUST be 0 (we set it in step B);
  # a re-run after B would see feeProtocol=68 and abort here before re-mutating.
  echo "   preflight: pool must be at post-L6 state (feeProtocol still 0)"
  assert_eq l8-pre-tick  "$(tick "$POOL")" "-13"
  assert_eq l8-pre-sqrtP "$(sqrtp "$POOL")" "79179582794851127394151969098"
  assert_eq l8-pre-liq   "$(liq_of "$POOL")" "$L6_LIQ"
  assert_eq l8-pre-bal0  "$(bal "$T0" "$POOL")" "14756844847625543880"
  assert_eq l8-pre-bal1  "$(bal "$T1" "$POOL")" "10764390315911847284"
  assert_eq l8-pre-fg0   "$(call "$POOL" "feeGrowthGlobal0X128()(uint256)" | awk '{print $1}')" "6047650206141426975919091749482503"
  assert_eq l8-pre-fg1   "$(call "$POOL" "feeGrowthGlobal1X128()(uint256)" | awk '{print $1}')" "5393146338779220563933678391096783"
  assert_eq l8-pre-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '6p')" "0"
  assert_eq l8-pre-cardinalityNext "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '5p')" "1"

  # (A) FLASH 5e18 of each token (feeProtocol still 0 → the whole 0.3% fee accrues to LPs via fg).
  FLASH="$(addr_of_create "$G_DEPLOY_HELPER" "$FLASH_BC" "constructor(address)" "$POOL")"
  is_addr "$FLASH" || { echo "ERROR flash deploy: $FLASH" >&2; exit 1; }
  save FLASH "$FLASH"; echo "   flash=$FLASH"
  assert_eq flash-pool "$(call "$FLASH" "pool()(address)")" "$POOL"
  # Fund only a small fee buffer (1e18 >> the 1.5e16 fee) rather than FUND: the helper has no sweep,
  # so a large balance would be stranded. The flash returns the principal; the helper only needs >= fee.
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$FLASH" 1000000000000000000 >/dev/null
  send --gas-limit "$G_CALL" "$T1" "transfer(address,uint256)" "$FLASH" 1000000000000000000 >/dev/null
  local fa0 fa1
  fa0=$(bal "$T0" "$POOL"); fa1=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$FLASH" "flash(uint256,uint256)" 5000000000000000000 5000000000000000000 >/dev/null
  echo "   (A) flash 5e18/5e18 (feeProtocol=0):"
  assert_eq l8a-fee0 "$(sub "$(bal "$T0" "$POOL")" "$fa0")" "15000000000000000"   # 5e18 * 0.3%
  assert_eq l8a-fee1 "$(sub "$(bal "$T1" "$POOL")" "$fa1")" "15000000000000000"
  assert_eq l8a-fg0  "$(call "$POOL" "feeGrowthGlobal0X128()(uint256)" | awk '{print $1}')" "7618184207314989114980820706859894"
  assert_eq l8a-fg1  "$(call "$POOL" "feeGrowthGlobal1X128()(uint256)" | awk '{print $1}')" "6963680339952782702995407348474174"
  assert_eq l8a-tick  "$(tick "$POOL")" "-13"   # flash doesn't move price...
  assert_eq l8a-sqrtP "$(sqrtp "$POOL")" "79179582794851127394151969098"   # ...nor the sqrt price
  assert_eq l8a-liq   "$(liq_of "$POOL")" "$L6_LIQ"

  # (B) setFeeProtocol(4,4) — onlyFactoryOwner == deployer; now 1/4 of swap fees goes to the protocol.
  send --gas-limit "$G_CALL" "$POOL" "setFeeProtocol(uint8,uint8)" 4 4 >/dev/null
  echo "   (B) setFeeProtocol(4,4):"
  assert_eq l8b-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '6p')" "68"   # 4 | (4<<4)

  # (C) swap 2e18 t0 with protocol fee active → 1/4 of the 6e15 fee accrues to protocolFees.token0.
  send --gas-limit "$G_CALL" "$T0" "transfer(address,uint256)" "$SWAPPER" "$FUND" >/dev/null
  local sc1
  sc1=$(bal "$T1" "$POOL")
  send --gas-limit "$G_CALL" "$SWAPPER" "swap(address,bool,int256,uint160)" "$DEP" true 2000000000000000000 "$MIN_SQRT_PLUS1" >/dev/null
  echo "   (C) swap 2e18 t0 (feeProtocol active):"
  assert_eq l8c-out  "$(sub "$sc1" "$(bal "$T1" "$POOL")")" "1990335060226168983"
  assert_eq l8c-tick "$(tick "$POOL")" "-25"
  assert_eq l8c-liq  "$(liq_of "$POOL")" "$L6_LIQ"
  assert_eq l8c-protocolfees0 "$(call "$POOL" "protocolFees()(uint128,uint128)" | sed -n '1p' | awk '{print $1}')" "1500000000000000"   # 6e15 / 4
  assert_eq l8c-protocolfees1 "$(call "$POOL" "protocolFees()(uint128,uint128)" | sed -n '2p' | awk '{print $1}')" "0"   # zeroForOne -> no token1 protocol fee

  # (D) collectProtocol to deployer — withdraws protocolFees-1 (V3 leaves 1 wei to keep the slot warm).
  local pd0
  pd0=$(bal "$T0" "$DEP")
  send --gas-limit "$G_CALL" "$POOL" "collectProtocol(address,uint128,uint128)" "$DEP" "$MAXU128" "$MAXU128" >/dev/null
  echo "   (D) collectProtocol:"
  assert_eq l8d-dep-gain "$(sub "$(bal "$T0" "$DEP")" "$pd0")" "1499999999999999"
  assert_eq l8d-protocolfees0-left "$(call "$POOL" "protocolFees()(uint128,uint128)" | sed -n '1p' | awk '{print $1}')" "1"

  # (E) oracle: grow observation cardinality. observe() values are time-dependent + may exceed Koinos
  # read bandwidth, so we assert only the (deterministic) cardinality bump and probe observe() softly.
  send --gas-limit "$G_CALL" "$POOL" "increaseObservationCardinalityNext(uint16)" 5 >/dev/null
  echo "   (E) oracle cardinality:"
  assert_eq l8e-cardinalityNext "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '5p')" "5"
  if call "$POOL" "observe(uint32[])(int56[],uint160[])" "[0]" >/dev/null 2>&1; then
    echo "   ok [l8e-observe] observe([0]) readable (oracle queryable)"
  else
    echo "   NOTE [l8e-observe]: observe() over compute-bandwidth on this RPC (expected on Koinos; cardinality bump still proven)"
  fi
}

case "$PHASE" in
  l1|L1) phase_l1 ;;
  l2|L2) phase_l2 ;;
  l3|L3) phase_l3 ;;
  l4|L4) phase_l4 ;;
  l5|L5) phase_l5 ;;
  l6|L6) phase_l6 ;;
  l7|L7) phase_l7 ;;
  l8|L8) phase_l8 ;;
  all)   phase_l1; phase_l2; phase_l3; phase_l4; phase_l5; phase_l6; phase_l7; phase_l8 ;;
  *) echo "unknown PHASE=$PHASE (use l1|l2|l3|l4|l5|l6|l7|l8|all)" >&2; exit 1 ;;
esac

echo
echo "=== STATE ($STATE_FILE) ==="; cat "$STATE_FILE"
