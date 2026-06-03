#!/usr/bin/env bash
#
# Deploy + exercise the REAL Uniswap V3 PERIPHERY (canonical SwapRouter from v3-periphery v1.3.0)
# on an EVM, continuing from the post-L8 state of the core breadth script (deploy_uniswap_v3.sh).
# Same methodology: reference values captured from a byte-identical anvil run, hard-asserted, and a
# forward-only on-chain PREFLIGHT so a re-run aborts before mutating the live (persisted) pool.
#
#   RPC=http://localhost:8546 PHASE=p1 ./deploy_v3_periphery.sh   # anvil (after core PHASE=all)
#   RPC=http://localhost:8545 PHASE=p1 ./deploy_v3_periphery.sh   # Koinos (live, after core L8)
#   RPC=http://localhost:8545 PHASE=p2 ./deploy_v3_periphery.sh   # Koinos exactOutputSingle + exactInput(path)
#   RPC=http://localhost:8545 PHASE=p3 ./deploy_v3_periphery.sh   # Koinos NonfungiblePositionManager mint (NFT position)
#   RPC=http://localhost:8545 PHASE=p4 ./deploy_v3_periphery.sh   # Koinos QuoterV2 deploy + quote (read-blocked, Track T)
#
# PREREQS: (1) core chain at post-L8 (run deploy_uniswap_v3.sh first); (2) periphery deps installed
# (scripts/forge/v3p-build/setup-deps.sh). STATE_FILE must hold POOL/T0/T1/FACTORY from the core run.
#
# REFERENCE (anvil, byte-identical artifacts; from post-L8 3000-pool tick -25, feeProtocol 68):
#   p1 exactInputSingle 1e18 token0 -> token1 (fee 3000) => out 994253072417405944 ; tick -25 -> -31
#   p2a exactOutputSingle EXACT 1e18 token1 out (pay token0) => in 1006398445954200346 ; tick -31 -> -37
#   p2b exactInput PATH oneForZero 1e18 token1 in -> token0 => out 1000370037699366977 ; tick -37 -> -31
#   p3 NFPM mint [-60,60] 5e18/5e18 desired => liq 1103721810967816252240, a0 5e18, a1 1614673053446482151 ; NFT tokenId 1 -> deployer
#   p4 QuoterV2 quote 1e18 t0->t1 (post-P3) => 993719482969316842 == actual swap (anvil) ; -1013 read-blocked on Koinos (Track T)
set -euo pipefail

RPC="${RPC:-http://localhost:8546}"
PHASE="${PHASE:-p1}"
STATE_FILE="${STATE_FILE:-/tmp/v3_state.env}"   # core (Koinos) state file by default; pass /tmp/v3_anvil.env for anvil
PK="${DEPLOYER_PK:?set DEPLOYER_PK to your EVM deployer key (0x-prefixed 64-hex)}"

G_WETH=1500000
G_ROUTER=6000000
G_CALL=2000000

DEP="$(cast wallet address --private-key "$PK")"
is_addr() { [[ "$1" =~ ^0x[0-9a-fA-F]{40}$ ]]; }
lc() { echo "$1" | tr 'A-F' 'a-f'; }
send()  { cast send --rpc-url "$RPC" --private-key "$PK" --legacy --gas-price 0 "$@"; }
call()  { cast call --rpc-url "$RPC" "$@"; }
bal()   { call "$1" "balanceOf(address)(uint256)" "$2" | awk '{print $1}'; }
sub()   { python3 -c "import sys;print(int(sys.argv[1])-int(sys.argv[2]))" "$1" "$2"; }
tick()  { call "$1" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '2p'; }
sqrtp() { call "$1" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '1p' | awk '{print $1}'; }
liq_of(){ call "$1" "liquidity()(uint128)" | awk '{print $1}'; }
codesz(){ cast codesize --rpc-url "$RPC" "$1" 2>/dev/null || echo '?'; }
assert_eq() { if [ "$(lc "$2")" != "$(lc "$3")" ]; then echo "   ASSERT FAIL [$1]: got=$2 expect=$3" >&2; exit 1; fi; echo "   ok [$1] = $2"; }
save()  { printf '%s=%s\n' "$1" "$2" >>"$STATE_FILE"; export "$1=$2"; }
addr_of_create() {
  local out a i
  for i in 1 2 3; do
    out="$(send --gas-limit "$1" --json --create "${@:2}" 2>/dev/null || true)"
    a="$(printf '%s' "$out" | jq -r '.contractAddress // empty' 2>/dev/null || true)"
    [[ "$a" =~ ^0x[0-9a-fA-F]{40}$ ]] && { printf '%s' "$a"; return 0; }
  done
  printf '%s' "${a:-}"
}

FORGE_DIR="$(cd "$(dirname "$0")/../forge" && pwd)"
cd "$FORGE_DIR"
echo ">> forge build (v3p profile — real periphery)"; FOUNDRY_PROFILE=v3p forge build >/dev/null
echo ">> forge build (v3pn profile — NFPM at runs=2000 to fit EIP-170)"; FOUNDRY_PROFILE=v3pn forge build >/dev/null
WETH9_BC="$(jq -r '.bytecode.object' out-v3p/WETH9.sol/WETH9.json)"
ROUTER_BC="$(jq -r '.bytecode.object' out-v3p/SwapRouter.sol/SwapRouter.json)"
NFPM_BC="$(jq -r '.bytecode.object' out-v3pn/NonfungiblePositionManager.sol/NonfungiblePositionManager.json)"
QUOTERV2_BC="$(jq -r '.bytecode.object' out-v3p/QuoterV2.sol/QuoterV2.json)"
NFPM_RT_B="$(jq -r '.deployedBytecode.object' out-v3pn/NonfungiblePositionManager.sol/NonfungiblePositionManager.json | awk '{print (length($0)-2)/2}')"

[ -f "$STATE_FILE" ] && source "$STATE_FILE" || true

phase_p1() {
  [ -n "${POOL:-}" ] && [ -n "${FACTORY:-}" ] && [ -n "${T0:-}" ] && [ -n "${T1:-}" ] \
    || { echo "ERROR: need POOL/FACTORY/T0/T1 in $STATE_FILE (run core deploy_uniswap_v3.sh first)" >&2; exit 1; }
  echo "== P1: real Uniswap V3 SwapRouter — exactInputSingle (continues from core post-L8) =="
  # PREFLIGHT (forward-only): the 3000-pool must be EXACTLY at the core post-L8 state. A re-run after
  # the swap lands at tick -31 and aborts here before re-mutating the live pool.
  echo "   preflight: 3000-pool must be at core post-L8 state"
  assert_eq p1-pre-tick  "$(tick "$POOL")" "-25"
  assert_eq p1-pre-sqrtP "$(sqrtp "$POOL")" "79131062613432839082079540120"
  assert_eq p1-pre-liq   "$(liq_of "$POOL")" "3250000000000000000000"
  assert_eq p1-pre-bal0  "$(bal "$T0" "$POOL")" "16770344847625543881"
  assert_eq p1-pre-bal1  "$(bal "$T1" "$POOL")" "8789055255685678301"
  assert_eq p1-pre-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '6p')" "68"

  # Deploy a WETH9 (for the router's immutable; untouched by ERC20->ERC20) + the canonical SwapRouter.
  WETH9="$(addr_of_create "$G_WETH" "$WETH9_BC")"
  is_addr "$WETH9" || { echo "ERROR weth9 deploy: $WETH9" >&2; exit 1; }
  save WETH9 "$WETH9"; echo "   weth9=$WETH9"
  ROUTER="$(addr_of_create "$G_ROUTER" "$ROUTER_BC" "constructor(address,address)" "$FACTORY" "$WETH9")"
  is_addr "$ROUTER" || { echo "ERROR router deploy: $ROUTER" >&2; exit 1; }
  save ROUTER "$ROUTER"; echo "   swapRouter=$ROUTER (runtime $(codesz "$ROUTER") B)"
  assert_eq router-factory "$(call "$ROUTER" "factory()(address)")" "$FACTORY"
  assert_eq router-weth9   "$(call "$ROUTER" "WETH9()(address)")" "$WETH9"

  # exactInputSingle: approve the router for token0, then swap 1e18 token0 -> token1 to the deployer.
  # The router pulls token0 via transferFrom and invokes pool.swap; the pool's CallbackValidation
  # (PoolAddress CREATE2 from our factory + canonical init hash) accepts the router's callback.
  send --gas-limit "$G_CALL" "$T0" "approve(address,uint256)" "$ROUTER" 1000000000000000000 >/dev/null
  local p0b p1b d1b
  p0b=$(bal "$T0" "$POOL"); p1b=$(bal "$T1" "$POOL"); d1b=$(bal "$T1" "$DEP")
  # amountOutMinimum = the exact expected output: an on-chain slippage guard so the swap REVERTS in-tx
  # (rather than mutating the live pool at a worse price) if anything shifted since the preflight.
  send --gas-limit "$G_CALL" "$ROUTER" "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))" \
    "($T0,$T1,3000,$DEP,9999999999,1000000000000000000,994253072417405944,0)" >/dev/null
  echo "   exactInputSingle 1e18 t0 -> t1:"
  assert_eq p1-pool-in   "$(sub "$(bal "$T0" "$POOL")" "$p0b")" "1000000000000000000"
  assert_eq p1-pool-out  "$(sub "$p1b" "$(bal "$T1" "$POOL")")" "994253072417405944"
  assert_eq p1-recipient "$(sub "$(bal "$T1" "$DEP")" "$d1b")" "994253072417405944"
  assert_eq p1-tick      "$(tick "$POOL")" "-31"
  # the router should have pulled EXACTLY the approved 1e18 — allowance back to zero.
  assert_eq p1-allowance-zeroed "$(call "$T0" "allowance(address,address)(uint256)" "$DEP" "$ROUTER" | awk '{print $1}')" "0"
}

phase_p2() {
  [ -n "${POOL:-}" ] && [ -n "${FACTORY:-}" ] && [ -n "${T0:-}" ] && [ -n "${T1:-}" ] && [ -n "${ROUTER:-}" ] \
    || { echo "ERROR: need POOL/FACTORY/T0/T1/ROUTER in $STATE_FILE (run p1 first)" >&2; exit 1; }
  echo "== P2: SwapRouter exactOutputSingle + exactInput(path) (continues from post-P1) =="
  # PREFLIGHT (forward-only): 3000-pool must be EXACTLY at post-P1 (tick -31). Reuses the p1 ROUTER.
  echo "   preflight: 3000-pool must be at post-P1 state"
  assert_eq p2-pre-tick  "$(tick "$POOL")" "-31"
  assert_eq p2-pre-sqrtP "$(sqrtp "$POOL")" "79106824815278441276693391397"
  assert_eq p2-pre-liq   "$(liq_of "$POOL")" "3250000000000000000000"
  assert_eq p2-pre-bal0  "$(bal "$T0" "$POOL")" "17770344847625543881"
  assert_eq p2-pre-bal1  "$(bal "$T1" "$POOL")" "7794802183268272357"
  assert_eq p2-pre-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '6p')" "68"
  assert_eq router-still-bound "$(call "$ROUTER" "factory()(address)")" "$FACTORY"

  # P2a — exactOutputSingle: pay token0 to receive EXACTLY 1e18 token1 (zeroForOne). Approve exactly the
  # expected input and set amountInMaximum to it (tight slippage bound), so the router pulls it all (allowance->0).
  # NB: with the exact entry state pinned by the preflight, the swap produces the exact expected amounts and
  # won't revert on the tight bound, so no dangling approval is left. A leftover DEP->router allowance would
  # only be usable by DEP anyway (the canonical router pulls from msg.sender), so it is not a drain vector.
  send --gas-limit "$G_CALL" "$T0" "approve(address,uint256)" "$ROUTER" 1006398445954200346 >/dev/null
  local a0b a1b ad1b
  a0b=$(bal "$T0" "$POOL"); a1b=$(bal "$T1" "$POOL"); ad1b=$(bal "$T1" "$DEP")
  send --gas-limit "$G_CALL" "$ROUTER" "exactOutputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))" \
    "($T0,$T1,3000,$DEP,9999999999,1000000000000000000,1006398445954200346,0)" >/dev/null
  echo "   P2a exactOutputSingle (EXACT 1e18 t1 out, pay t0):"
  assert_eq p2a-exact-out "$(sub "$(bal "$T1" "$DEP")" "$ad1b")" "1000000000000000000"
  assert_eq p2a-pool-out  "$(sub "$a1b" "$(bal "$T1" "$POOL")")" "1000000000000000000"
  assert_eq p2a-in        "$(sub "$(bal "$T0" "$POOL")" "$a0b")" "1006398445954200346"
  assert_eq p2a-tick      "$(tick "$POOL")" "-37"
  assert_eq p2a-allowance-zeroed "$(call "$T0" "allowance(address,address)(uint256)" "$DEP" "$ROUTER" | awk '{print $1}')" "0"

  # P2b — exactInput via ENCODED PATH (oneForZero): path = token1 ++ fee(3000,uint24) ++ token0. Pay 1e18
  # token1, receive token0. Exercises the Path library + the other swap direction through the router.
  local PATH_B; PATH_B="$(cast abi-encode --packed 'f(address,uint24,address)' "$T1" 3000 "$T0")"
  send --gas-limit "$G_CALL" "$T1" "approve(address,uint256)" "$ROUTER" 1000000000000000000 >/dev/null
  local b0b b1b bd0b
  b0b=$(bal "$T0" "$POOL"); b1b=$(bal "$T1" "$POOL"); bd0b=$(bal "$T0" "$DEP")
  send --gas-limit "$G_CALL" "$ROUTER" "exactInput((bytes,address,uint256,uint256,uint256))" \
    "($PATH_B,$DEP,9999999999,1000000000000000000,1000370037699366977)" >/dev/null
  echo "   P2b exactInput(path) oneForZero (1e18 t1 in -> t0):"
  assert_eq p2b-out       "$(sub "$(bal "$T0" "$DEP")" "$bd0b")" "1000370037699366977"
  assert_eq p2b-pool-out  "$(sub "$b0b" "$(bal "$T0" "$POOL")")" "1000370037699366977"
  assert_eq p2b-pool-in   "$(sub "$(bal "$T1" "$POOL")" "$b1b")" "1000000000000000000"
  assert_eq p2b-tick      "$(tick "$POOL")" "-31"
  assert_eq p2b-allowance-zeroed "$(call "$T1" "allowance(address,address)(uint256)" "$DEP" "$ROUTER" | awk '{print $1}')" "0"
}

phase_p3() {
  [ -n "${POOL:-}" ] && [ -n "${FACTORY:-}" ] && [ -n "${T0:-}" ] && [ -n "${T1:-}" ] && [ -n "${WETH9:-}" ] \
    || { echo "ERROR: need POOL/FACTORY/T0/T1/WETH9 in $STATE_FILE (run p1 first)" >&2; exit 1; }
  echo "== P3: NonfungiblePositionManager — deploy + mint an NFT-wrapped position (post-P2) =="
  # PREFLIGHT (forward-only): 3000-pool at post-P2. A re-run lands liq 4.35e21 and aborts here BEFORE
  # the (expensive ~25KB) NFPM deploy + mint.
  echo "   preflight: 3000-pool must be at post-P2 state"
  assert_eq p3-pre-tick  "$(tick "$POOL")" "-31"
  assert_eq p3-pre-sqrtP "$(sqrtp "$POOL")" "79106751681589966571150997355"
  assert_eq p3-pre-liq   "$(liq_of "$POOL")" "3250000000000000000000"
  assert_eq p3-pre-bal0  "$(bal "$T0" "$POOL")" "17776373255880377250"
  assert_eq p3-pre-bal1  "$(bal "$T1" "$POOL")" "7794802183268272357"
  assert_eq p3-pre-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '6p')" "68"

  # Guard against optimizer-config drift: NFPM runtime MUST be <= EIP-170 or the engine rejects the deploy.
  [ "$NFPM_RT_B" -le 24576 ] || { echo "ERROR: NFPM runtime $NFPM_RT_B B exceeds EIP-170 24576 — lower optimizer_runs in [profile.v3pn]" >&2; exit 1; }
  echo "   NFPM runtime $NFPM_RT_B B (<= 24576 ok)"
  # Deploy the REAL NonfungiblePositionManager. The 3rd ctor arg is the NFT SVG descriptor, used ONLY by
  # tokenURI() — never by mint — so we pass the deployer EOA as a harmless placeholder (no real descriptor).
  NFPM="$(addr_of_create "8000000" "$NFPM_BC" "constructor(address,address,address)" "$FACTORY" "$WETH9" "$DEP")"
  is_addr "$NFPM" || { echo "ERROR nfpm deploy: $NFPM" >&2; exit 1; }
  save NFPM "$NFPM"; echo "   nfpm=$NFPM (runtime $(codesz "$NFPM") B)"
  assert_eq nfpm-factory "$(call "$NFPM" "factory()(address)")" "$FACTORY"
  assert_eq nfpm-weth9   "$(call "$NFPM" "WETH9()(address)")" "$WETH9"

  # Approve both tokens, then mint a concentrated position [-60,+60] (in-range at tick -31). NFPM wraps it
  # as ERC-721 tokenId 1 owned by `recipient`; the underlying pool position is owned by NFPM (msg.sender).
  send --gas-limit "$G_CALL" "$T0" "approve(address,uint256)" "$NFPM" 10000000000000000000 >/dev/null
  send --gas-limit "$G_CALL" "$T1" "approve(address,uint256)" "$NFPM" 10000000000000000000 >/dev/null
  local lqb m0b m1b
  lqb=$(liq_of "$POOL"); m0b=$(bal "$T0" "$POOL"); m1b=$(bal "$T1" "$POOL")
  # amount0Min/amount1Min = exact expected deposits: in-tx slippage guard (reverts if the price shifted).
  send --gas-limit "4000000" "$NFPM" "mint((address,address,uint24,int24,int24,uint256,uint256,uint256,uint256,address,uint256))" \
    "($T0,$T1,3000,-60,60,5000000000000000000,5000000000000000000,5000000000000000000,1614673053446482151,$DEP,9999999999)" >/dev/null
  echo "   NFPM mint [-60,+60] (5e18/5e18 desired):"
  assert_eq p3-minted-liquidity "$(sub "$(liq_of "$POOL")" "$lqb")" "1103721810967816252240"
  assert_eq p3-amount0          "$(sub "$(bal "$T0" "$POOL")" "$m0b")" "5000000000000000000"
  assert_eq p3-amount1          "$(sub "$(bal "$T1" "$POOL")" "$m1b")" "1614673053446482151"
  assert_eq p3-pool-liq-after   "$(liq_of "$POOL")" "4353721810967816252240"
  # ERC-721 wrapping: tokenId 1 minted to the deployer. totalSupply() + balanceOf() are light (single
  # length SLOAD) and stay within Koinos read bandwidth — keep them HARD. They alone prove an NFT exists
  # and the deployer owns exactly one.
  assert_eq p3-nfpm-totalsupply "$(call "$NFPM" "totalSupply()(uint256)" | awk '{print $1}')" "1"
  assert_eq p3-nfpm-balanceof   "$(call "$NFPM" "balanceOf(address)(uint256)" "$DEP" | awk '{print $1}')" "1"
  # ownerOf(1) routes through OZ EnumerableMap.get (index lookup + revert-string) — exceeds Koinos read
  # compute-bandwidth on the 24KB NFPM (Track T), though it works on anvil. Soft-assert: prove on anvil,
  # NOTE-skip on Koinos (ownership already covered by balanceOf=1 + totalSupply=1).
  local OWNER1; OWNER1="$(call "$NFPM" "ownerOf(uint256)(address)" 1 2>/dev/null || true)"
  if is_addr "$OWNER1"; then assert_eq p3-nfpm-ownerof1 "$OWNER1" "$DEP";
  else echo "   NOTE [p3-nfpm-ownerof1]: ownerOf(1) over read compute-bandwidth (Track T); covered by balanceOf=1 + totalSupply=1"; fi
  # NFPM.positions(1) is a heavy 12-field struct read — over Koinos read compute-bandwidth (Track T).
  if call "$NFPM" "positions(uint256)(uint96,address,address,address,uint24,int24,int24,uint128,uint256,uint256,uint128,uint128)" 1 >/dev/null 2>&1; then
    echo "   ok [p3-nfpm-positions] positions(1) readable"
  else
    echo "   NOTE [p3-nfpm-positions]: positions(1) over read compute-bandwidth (expected on Koinos; covered by pool liquidity + balanceOf)"
  fi
}

phase_p4() {
  [ -n "${POOL:-}" ] && [ -n "${FACTORY:-}" ] && [ -n "${T0:-}" ] && [ -n "${T1:-}" ] && [ -n "${WETH9:-}" ] && [ -n "${ROUTER:-}" ] \
    || { echo "ERROR: need POOL/FACTORY/T0/T1/WETH9/ROUTER in $STATE_FILE (run p1..p3 first)" >&2; exit 1; }
  echo "== P4: QuoterV2 — quote correctness (anvil) / read-bandwidth block (Koinos), post-P3 =="
  # Anchor the deterministic quote value to post-P3 state. (Koinos quote is read-blocked regardless, but
  # this keeps the anvil quote == the 993719482969316842 reference.)
  echo "   preflight: 3000-pool must be at post-P3 state (full fingerprint)"
  assert_eq p4-pre-tick "$(tick "$POOL")" "-31"
  assert_eq p4-pre-liq  "$(liq_of "$POOL")" "4353721810967816252240"
  assert_eq p4-pre-sqrtP "$(sqrtp "$POOL")" "79106751681589966571150997355"
  assert_eq p4-pre-bal0  "$(bal "$T0" "$POOL")" "22776373255880377250"
  assert_eq p4-pre-bal1  "$(bal "$T1" "$POOL")" "9409475236714754508"
  assert_eq p4-pre-feeprotocol "$(call "$POOL" "slot0()(uint160,int24,uint16,uint16,uint16,uint8,bool)" | sed -n '6p')" "68"

  # Reuse a saved+validated QuoterV2 (chain-idempotent on re-run); else deploy.
  if [ -n "${QUOTER:-}" ] && is_addr "${QUOTER:-}" \
     && [ "$(lc "$(call "$QUOTER" "factory()(address)" 2>/dev/null || echo x)")" = "$(lc "$FACTORY")" ]; then
    echo "   reusing saved quoterV2=$QUOTER"
  else
    QUOTER="$(addr_of_create "$G_ROUTER" "$QUOTERV2_BC" "constructor(address,address)" "$FACTORY" "$WETH9")"
    is_addr "$QUOTER" || { echo "ERROR quoterv2 deploy: $QUOTER" >&2; exit 1; }
    save QUOTER "$QUOTER"; echo "   quoterV2=$QUOTER (runtime $(codesz "$QUOTER") B)"
  fi
  assert_eq quoterv2-factory "$(call "$QUOTER" "factory()(address)")" "$FACTORY"
  assert_eq quoterv2-weth9   "$(call "$QUOTER" "WETH9()(address)")" "$WETH9"

  # quoteExactInputSingle is a swap-simulation (non-view; reverts internally to return the result), invoked
  # via eth_call → read_contract. On Koinos this runs a full swap under the 10M read cap → -1013 (Track T).
  local QOUT
  QOUT="$(call "$QUOTER" "quoteExactInputSingle((address,address,uint256,uint24,uint160))(uint256,uint160,uint32,uint256)" "($T0,$T1,1000000000000000000,3000,0)" 2>/dev/null | sed -n '1p' | awk '{print $1}' || true)"
  if [[ "$QOUT" =~ ^[0-9]+$ ]]; then
    echo "   quote readable: amountOut = $QOUT"
    assert_eq p4-quote "$QOUT" "993719482969316842"
    # Proving quote == a real swap MUTATES the pool, so it's opt-in via P4_PROVE_SWAP=1 (anvil only) — NOT
    # gated merely on "the quote returned a number", so if Koinos later lifts the read cap this won't
    # silently swap on the live pool.
    if [ "${P4_PROVE_SWAP:-0}" = "1" ]; then
      echo "   P4_PROVE_SWAP=1: proving the quote predicts a real swap to the wei (mutates the pool)"
      send --gas-limit "$G_CALL" "$T0" "approve(address,uint256)" "$ROUTER" 1000000000000000000 >/dev/null
      local qp1; qp1="$(bal "$T1" "$POOL")"
      send --gas-limit "$G_CALL" "$ROUTER" "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))" \
        "($T0,$T1,3000,$DEP,9999999999,1000000000000000000,993719482969316842,0)" >/dev/null
      assert_eq p4-quote-matches-swap "$(sub "$qp1" "$(bal "$T1" "$POOL")")" "$QOUT"
    else
      echo "   NOTE: quote value verified; comparison swap skipped (set P4_PROVE_SWAP=1 on a throwaway chain to prove quote == real swap)"
    fi
  else
    echo "   NOTE [p4-quote]: quoteExactInputSingle over read compute-bandwidth (-1013, Track T)."
    echo "        QuoterV2 CONTRACT deployed OK (tx path); only the read-path quote is blocked. Quote"
    echo "        correctness is proven on anvil (== 993719482969316842 == a real swap). Unblock = raise"
    echo "        the node read-compute-bandwidth-limit (see memory koinos-read-compute-limit)."
  fi
}

case "$PHASE" in
  p1|P1) phase_p1 ;;
  p2|P2) phase_p2 ;;
  p3|P3) phase_p3 ;;
  p4|P4) phase_p4 ;;
  *) echo "unknown PHASE=$PHASE (use p1|p2|p3|p4)" >&2; exit 1 ;;
esac

echo
echo "=== STATE ($STATE_FILE) ==="; tail -6 "$STATE_FILE"
