// Uniswap V3 TickMath + LiquidityAmounts, ported to BigInt.
//
// Exact constants from v3-core `TickMath.sol` (getSqrtRatioAtTick) and the formulas
// from v3-periphery `LiquidityAmounts.sol`. Used by pool.js to compute mint amounts /
// liquidity CLIENT-SIDE — there is no on-chain Quoter on Koinos (Track-T read limit),
// so the dApp does this math itself. BigInt has no overflow, so FullMath.mulDiv(a,b,d)
// is simply (a*b)/d here.

const Q96 = 1n << 96n;
const MAX_U256 = (1n << 256n) - 1n;
export const MIN_TICK = -887272;
export const MAX_TICK = 887272;

// sqrtPriceX96 = sqrt(1.0001^tick) * 2^96. Matches v3-core TickMath exactly (incl. rounding).
export function getSqrtRatioAtTick(tick) {
  tick = Number(tick) | 0;
  const absTick = tick < 0 ? -tick : tick;
  if (absTick > MAX_TICK) throw new Error('T: tick out of range');

  let ratio = (absTick & 0x1) !== 0
    ? 0xfffcb933bd6fad37aa2d162d1a594001n
    : 0x100000000000000000000000000000000n;
  if (absTick & 0x2)     ratio = (ratio * 0xfff97272373d413259a46990580e213an) >> 128n;
  if (absTick & 0x4)     ratio = (ratio * 0xfff2e50f5f656932ef12357cf3c7fdccn) >> 128n;
  if (absTick & 0x8)     ratio = (ratio * 0xffe5caca7e10e4e61c3624eaa0941cd0n) >> 128n;
  if (absTick & 0x10)    ratio = (ratio * 0xffcb9843d60f6159c9db58835c926644n) >> 128n;
  if (absTick & 0x20)    ratio = (ratio * 0xff973b41fa98c081472e6896dfb254c0n) >> 128n;
  if (absTick & 0x40)    ratio = (ratio * 0xff2ea16466c96a3843ec78b326b52861n) >> 128n;
  if (absTick & 0x80)    ratio = (ratio * 0xfe5dee046a99a2a811c461f1969c3053n) >> 128n;
  if (absTick & 0x100)   ratio = (ratio * 0xfcbe86c7900a88aedcffc83b479aa3a4n) >> 128n;
  if (absTick & 0x200)   ratio = (ratio * 0xf987a7253ac413176f2b074cf7815e54n) >> 128n;
  if (absTick & 0x400)   ratio = (ratio * 0xf3392b0822b70005940c7a398e4b70f3n) >> 128n;
  if (absTick & 0x800)   ratio = (ratio * 0xe7159475a2c29b7443b29c7fa6e889d9n) >> 128n;
  if (absTick & 0x1000)  ratio = (ratio * 0xd097f3bdfd2022b8845ad8f792aa5825n) >> 128n;
  if (absTick & 0x2000)  ratio = (ratio * 0xa9f746462d870fdf8a65dc1f90e061e5n) >> 128n;
  if (absTick & 0x4000)  ratio = (ratio * 0x70d869a156d2a1b890bb3df62baf32f7n) >> 128n;
  if (absTick & 0x8000)  ratio = (ratio * 0x31be135f97d08fd981231505542fcfa6n) >> 128n;
  if (absTick & 0x10000) ratio = (ratio * 0x9aa508b5b7a84e1c677de54f3e99bc9n)  >> 128n;
  if (absTick & 0x20000) ratio = (ratio * 0x5d6af8dedb81196699c329225ee604n)   >> 128n;
  if (absTick & 0x40000) ratio = (ratio * 0x2216e584f5fa1ea926041bedfe98n)     >> 128n;
  if (absTick & 0x80000) ratio = (ratio * 0x48a170391f7dc42444e8fa2n)          >> 128n;

  if (tick > 0) ratio = MAX_U256 / ratio;

  // Q128.128 -> Q128.96, rounding up.
  let sqrtP = ratio >> 32n;
  if (ratio % (1n << 32n) !== 0n) sqrtP += 1n;
  return sqrtP;
}

// Snap a tick down to the nearest multiple of `spacing` (toward -inf), like the pool.
export function nearestUsableTick(tick, spacing) {
  tick = Number(tick) | 0;
  const rounded = Math.round(tick / spacing) * spacing;
  if (rounded < MIN_TICK) return rounded + spacing;
  if (rounded > MAX_TICK) return rounded - spacing;
  return rounded;
}

// ── LiquidityAmounts ────────────────────────────────────────────────────────
function sort(a, b) { return a > b ? [b, a] : [a, b]; }

export function getLiquidityForAmount0(sqrtA, sqrtB, amount0) {
  [sqrtA, sqrtB] = sort(sqrtA, sqrtB);
  const intermediate = (sqrtA * sqrtB) / Q96;
  return (amount0 * intermediate) / (sqrtB - sqrtA);
}
export function getLiquidityForAmount1(sqrtA, sqrtB, amount1) {
  [sqrtA, sqrtB] = sort(sqrtA, sqrtB);
  return (amount1 * Q96) / (sqrtB - sqrtA);
}
export function getLiquidityForAmounts(sqrtP, sqrtA, sqrtB, amount0, amount1) {
  [sqrtA, sqrtB] = sort(sqrtA, sqrtB);
  if (sqrtP <= sqrtA) return getLiquidityForAmount0(sqrtA, sqrtB, amount0);
  if (sqrtP < sqrtB) {
    const l0 = getLiquidityForAmount0(sqrtP, sqrtB, amount0);
    const l1 = getLiquidityForAmount1(sqrtA, sqrtP, amount1);
    return l0 < l1 ? l0 : l1;
  }
  return getLiquidityForAmount1(sqrtA, sqrtB, amount1);
}

export function getAmount0ForLiquidity(sqrtA, sqrtB, L) {
  [sqrtA, sqrtB] = sort(sqrtA, sqrtB);
  // mulDiv(L << 96, sqrtB - sqrtA, sqrtB) / sqrtA
  return (((L << 96n) * (sqrtB - sqrtA)) / sqrtB) / sqrtA;
}
export function getAmount1ForLiquidity(sqrtA, sqrtB, L) {
  [sqrtA, sqrtB] = sort(sqrtA, sqrtB);
  return (L * (sqrtB - sqrtA)) / Q96;
}
export function getAmountsForLiquidity(sqrtP, sqrtA, sqrtB, L) {
  [sqrtA, sqrtB] = sort(sqrtA, sqrtB);
  let a0 = 0n, a1 = 0n;
  if (sqrtP <= sqrtA) {
    a0 = getAmount0ForLiquidity(sqrtA, sqrtB, L);
  } else if (sqrtP < sqrtB) {
    a0 = getAmount0ForLiquidity(sqrtP, sqrtB, L);
    a1 = getAmount1ForLiquidity(sqrtA, sqrtP, L);
  } else {
    a1 = getAmount1ForLiquidity(sqrtA, sqrtB, L);
  }
  return [a0, a1];
}

// ── Single-tick swap quote (no on-chain Quoter on Koinos — Track-T) ─────────────
// Exact-output assuming the swap stays within the CURRENT tick at constant liquidity L
// (ignores crossing initialized ticks). The 0.3% fee is taken from amountIn first.
// Rounds conservatively: sqrtP_next up for token0-in, amountOut down. Returns
// { amountOut, sqrtPNext, crossedToLimit } — crossedToLimit flags a degenerate result.
const FEE_NUM = 997000n;   // 0.3% fee: amountInAfterFee = amountIn * 997000 / 1000000
const FEE_DEN = 1000000n;
function ceilDiv(a, b) { return (a + b - 1n) / b; }

export function quoteSingleTick(sqrtP, liquidity, amountIn, zeroForOne) {
  if (liquidity <= 0n || amountIn <= 0n) return { amountOut: 0n, sqrtPNext: sqrtP, crossedToLimit: false };
  const amountInAfterFee = (amountIn * FEE_NUM) / FEE_DEN;
  if (amountInAfterFee <= 0n) return { amountOut: 0n, sqrtPNext: sqrtP, crossedToLimit: false };
  let sqrtPNext, amountOut;
  if (zeroForOne) {
    // token0 in, price decreases. sqrtP_next = L*sqrtP*Q96 / (L*Q96 + amountInAfterFee*sqrtP), round UP.
    const num = liquidity * sqrtP * Q96;
    const den = liquidity * Q96 + amountInAfterFee * sqrtP;
    sqrtPNext = ceilDiv(num, den);
    if (sqrtPNext >= sqrtP) sqrtPNext = sqrtP; // numerical floor
    amountOut = (liquidity * (sqrtP - sqrtPNext)) / Q96; // token1 out, round down
  } else {
    // token1 in, price increases. sqrtP_next = sqrtP + amountInAfterFee*Q96/L (round down).
    sqrtPNext = sqrtP + (amountInAfterFee * Q96) / liquidity;
    // token0 out = L*Q96*(sqrtP_next - sqrtP)/(sqrtP_next*sqrtP), round down
    amountOut = (liquidity * Q96 * (sqrtPNext - sqrtP)) / (sqrtPNext * sqrtP);
  }
  if (amountOut < 0n) amountOut = 0n;
  return { amountOut, sqrtPNext, crossedToLimit: false };
}

// ── UI helper: given a range + ONE provided amount, derive liquidity + both amounts ──
// Returns { liquidity, amount0, amount1, kind } where kind describes which token(s) the
// position needs at the current price. `provided` = { amount0 } or { amount1 }.
export function computeMint(sqrtP, tickLower, tickUpper, provided) {
  const sqrtA = getSqrtRatioAtTick(tickLower);
  const sqrtB = getSqrtRatioAtTick(tickUpper);
  let L;
  let kind;
  if (sqrtP <= sqrtA) {
    // price below range → all token0
    kind = 'token0-only';
    if (provided.amount0 == null) return { liquidity: 0n, amount0: 0n, amount1: 0n, kind, sqrtA, sqrtB };
    L = getLiquidityForAmount0(sqrtA, sqrtB, provided.amount0);
  } else if (sqrtP >= sqrtB) {
    // price above range → all token1
    kind = 'token1-only';
    if (provided.amount1 == null) return { liquidity: 0n, amount0: 0n, amount1: 0n, kind, sqrtA, sqrtB };
    L = getLiquidityForAmount1(sqrtA, sqrtB, provided.amount1);
  } else {
    // in range → needs both; derive L from whichever amount the user typed
    kind = 'both';
    if (provided.amount0 != null) L = getLiquidityForAmount0(sqrtP, sqrtB, provided.amount0);
    else if (provided.amount1 != null) L = getLiquidityForAmount1(sqrtA, sqrtP, provided.amount1);
    else return { liquidity: 0n, amount0: 0n, amount1: 0n, kind, sqrtA, sqrtB };
  }
  const [amount0, amount1] = getAmountsForLiquidity(sqrtP, sqrtA, sqrtB, L);
  return { liquidity: L, amount0, amount1, kind, sqrtA, sqrtB };
}
