// Koinos EVM Pool Manager — Uniswap V3 concentrated-liquidity testing UI.
//
// Reads pool state via the local proxy (light reads: slot0/liquidity/fee — these work;
// NFPM positions()/ownerOf() are read-blocked by the Track-T compute limit, so positions
// are tracked in localStorage from receipt events). Writes go through MetaMask, zero-gas.
//
// M1: connect + live pool state + balances/faucet. (Create/manage/swap added in M2–M4.)

import {
  BrowserProvider,
  Contract,
  Interface,
  parseUnits,
  formatUnits,
  MaxUint256,
} from 'https://esm.sh/ethers@6.13.4';

import {
  CHAIN, TOKENS, ERC20_ABI, GAS,
  V3, V3POOL_ABI, NFPM_ABI, V3ROUTER_ABI, V3_GAS,
} from './config.js';

import {
  readProvider, connect as walletConnect, chainOk,
  fmt, shortAddr, errMessage, createToaster,
} from './wallet.js';

import {
  getSqrtRatioAtTick, getAmountsForLiquidity, computeMint, quoteSingleTick,
  nearestUsableTick, MIN_TICK, MAX_TICK,
} from './v3math.js';

const $ = (id) => document.getElementById(id);
const TX_TIMEOUT_MS = 120000;
const FAUCET_AMOUNT = '1000';

const { toast, toastConfirmed } = createToaster($('toast'));

const state = {
  provider: null,
  signer: null,
  account: null,
  networkOk: false,
  poolNow: null, // { sqrtPriceX96, tick } cached from the latest pool read
};

// ── Pool state (light reads — always available) ─────────────────────────────
const poolRead = new Contract(V3.pool, V3POOL_ABI, readProvider);

// price of token0 in units of token1, scaled to 1e18 for display via fmt(.,18).
function priceToken1PerToken0(sqrtPriceX96, dec0, dec1) {
  const Q192 = 1n << 192n;
  const num = sqrtPriceX96 * sqrtPriceX96 * (10n ** BigInt(dec0)) * (10n ** 18n);
  const den = Q192 * (10n ** BigInt(dec1));
  return num / den;
}

let poolStateSeq = 0;
async function renderPoolState() {
  const seq = ++poolStateSeq;
  const host = $('poolState');
  try {
    const [slot0, liquidity, fee] = await Promise.all([
      poolRead.slot0(),
      poolRead.liquidity(),
      poolRead.fee(),
    ]);
    if (seq !== poolStateSeq) return; // superseded by a newer refresh
    const sqrtPriceX96 = slot0[0];
    const tick = Number(slot0[1]);
    // Cache for the create-position form + swap quote (price-relative math).
    state.poolNow = { sqrtPriceX96, tick, liquidity };
    if (typeof recomputeCreate === 'function') recomputeCreate();
    if (typeof recomputeSwap === 'function') recomputeSwap();
    const price = priceToken1PerToken0(sqrtPriceX96, V3.token0.decimals, V3.token1.decimals);
    const t0 = V3.token0, t1 = V3.token1;
    host.innerHTML = `
      <div class="k">Price</div>
      <div class="v big">1 ${esc(t0.symbol)} ≈ ${esc(fmt(price, 18, 6))} ${esc(t1.symbol)}</div>
      <div class="k">Current tick</div><div class="v">${tick}</div>
      <div class="k">In-range liquidity</div><div class="v">${esc(liquidity.toString())}</div>
      <div class="k">Fee tier</div><div class="v">${(Number(fee) / 10000).toFixed(2)}% (spacing ${V3.tickSpacing})</div>
      <div class="k">token0</div><div class="v mono" title="${esc(t0.address)}">${esc(t0.symbol)} · ${esc(shortAddr(t0.address))}</div>
      <div class="k">token1</div><div class="v mono" title="${esc(t1.address)}">${esc(t1.symbol)} · ${esc(shortAddr(t1.address))}</div>
      <div class="k">sqrtPriceX96</div><div class="v mono">${esc(sqrtPriceX96.toString())}</div>`;
  } catch (e) {
    if (seq !== poolStateSeq) return;
    host.innerHTML = `<div class="empty">Pool read failed: ${esc(errMessage(e))}</div>`;
  }
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

// ── Balances + faucet ───────────────────────────────────────────────────────
function buildFaucet() {
  const wrap = $('faucetBtns');
  wrap.innerHTML = '';
  for (const t of TOKENS) {
    const btn = document.createElement('button');
    btn.className = 'btn';
    btn.textContent = `Get ${FAUCET_AMOUNT} ${t.symbol}`;
    btn.addEventListener('click', () => mintToken(t, btn));
    wrap.appendChild(btn);
  }
  applyNetworkGate();
}

let balanceSeq = 0;
async function refreshBalances() {
  if (!state.account) return;
  const account = state.account;
  const seq = ++balanceSeq;
  const rows = [];
  for (const t of TOKENS) {
    let amt = 'error';
    try {
      const c = new Contract(t.address, ERC20_ABI, readProvider);
      amt = fmt(await c.balanceOf(account), t.decimals);
    } catch { amt = 'error'; }
    rows.push({ t, amt });
  }
  if (seq !== balanceSeq || state.account !== account) return;
  const ul = $('balances');
  ul.innerHTML = '';
  for (const { t, amt } of rows) {
    const li = document.createElement('li');
    li.className = 'bal';
    li.innerHTML = `<span class="sym">${esc(t.symbol)}<span class="name">${esc(t.name)}</span></span><span class="amt">${esc(amt)}</span>`;
    ul.appendChild(li);
  }
}

async function mintToken(token, btn) {
  if (!requireWallet()) return;
  const account = state.account;
  const original = btn.textContent;
  btn.disabled = true;
  btn.textContent = 'Minting…';
  try {
    const c = new Contract(token.address, ERC20_ABI, state.signer);
    const amount = parseUnits(FAUCET_AMOUNT, token.decimals);
    toast(`Requesting ${FAUCET_AMOUNT} ${token.symbol} — confirm in your wallet…`, 'pending');
    const tx = await c.mint(account, amount, { gasLimit: GAS.mint });
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    toastConfirmed(`Minted ${FAUCET_AMOUNT} ${token.symbol} ✓`, receipt?.blockNumber);
    if (state.account === account) await refreshBalances();
  } catch (err) {
    if (err?.code === 'TIMEOUT') {
      toast('Confirmation timed out — the mint may still settle. Refresh shortly.', 'pending');
      if (state.account === account) await refreshBalances();
    } else {
      toast(`Mint failed: ${errMessage(err)}`, 'err');
    }
  } finally {
    btn.disabled = false;
    btn.textContent = original;
  }
}

// ── Create position (NFPM.mint) ─────────────────────────────────────────────
const SLIPPAGE_BPS = 50n; // 0.5%
const BPS = 10000n;
const nfpmIface = new Interface(NFPM_ABI);
const create = { primary: 'amount0', lastPrimary: null, plan: null, busy: false };

// Positions are tracked in localStorage (NFPM positions()/ownerOf() are read-blocked
// by the Track-T compute limit), keyed by account. BigInts are stored as strings.
function posKey(account) { return `koinos-evm-v3-pos:${account.toLowerCase()}`; }
function loadPositions(account) {
  try {
    const p = JSON.parse(localStorage.getItem(posKey(account)) || '[]');
    return Array.isArray(p) ? p : [];
  } catch { return []; }
}
function savePositions(account, arr) { localStorage.setItem(posKey(account), JSON.stringify(arr)); }
function addStoredPosition(account, p) {
  const arr = loadPositions(account);
  arr.unshift(p);
  savePositions(account, arr);
}

function parseTick(v) {
  const s = String(v).trim();
  if (!/^-?\d+$/.test(s)) return null;
  return Number(s);
}
function parseAmt(raw, decimals) {
  const s = String(raw || '').trim();
  if (!s) return null;
  try { return parseUnits(s, decimals); } catch { return null; }
}
const slipMin = (x) => (x * (BPS - SLIPPAGE_BPS)) / BPS;

// Recompute the create-position plan from the current inputs + live price.
function recomputeCreate() {
  if (create.busy) return; // a mint is in flight — don't touch the form/button
  const note = $('createNote');
  const btn = $('addLiqBtn');
  const a0in = $('amount0'), a1in = $('amount1');
  const t0 = V3.token0, t1 = V3.token1;
  $('amt0Label').textContent = `${t0.symbol} amount`;
  $('amt1Label').textContent = `${t1.symbol} amount`;
  create.plan = null;
  if (!state.poolNow) { note.textContent = 'Pool state loading…'; btn.disabled = true; return; }

  const tl = parseTick($('tickLower').value);
  const tu = parseTick($('tickUpper').value);
  if (tl === null || tu === null) { note.textContent = 'Enter integer ticks.'; btn.disabled = true; return; }
  if (tl % V3.tickSpacing !== 0 || tu % V3.tickSpacing !== 0) { note.textContent = `Ticks must be multiples of ${V3.tickSpacing}.`; btn.disabled = true; return; }
  if (tl >= tu) { note.textContent = 'Lower tick must be < upper tick.'; btn.disabled = true; return; }
  if (tl < MIN_TICK || tu > MAX_TICK) { note.textContent = `Ticks must be within ±${MAX_TICK}.`; btn.disabled = true; return; }

  const { sqrtPriceX96: sqrtP, tick } = state.poolNow;
  // Which token(s) does the range need at the current price?
  const kind = tick < tl ? 'token0-only' : (tick >= tu ? 'token1-only' : 'both');
  // token1-only -> user types token1; otherwise token0 is the primary input.
  if (kind === 'token1-only') { create.primary = 'amount1'; a0in.readOnly = true; a1in.readOnly = false; }
  else { create.primary = 'amount0'; a0in.readOnly = false; a1in.readOnly = true; }

  // If the primary input just flipped (range moved across the current price), the
  // now-primary field still holds a stale COMPUTED value — clear it so it isn't
  // silently treated as user-entered input for the new range.
  if (create.lastPrimary && create.lastPrimary !== create.primary) {
    if (create.primary === 'amount0') a0in.value = '';
    else a1in.value = '';
  }
  create.lastPrimary = create.primary;

  const provided = {};
  if (create.primary === 'amount0') provided.amount0 = parseAmt(a0in.value, t0.decimals);
  else provided.amount1 = parseAmt(a1in.value, t1.decimals);
  const primaryVal = create.primary === 'amount0' ? provided.amount0 : provided.amount1;
  if (primaryVal === null || primaryVal <= 0n) {
    note.textContent = kind === 'both'
      ? `In range — enter ${t0.symbol}; ${t1.symbol} is computed.`
      : kind === 'token0-only'
        ? `Price below range — single-sided ${t0.symbol}.`
        : `Price above range — single-sided ${t1.symbol}.`;
    btn.textContent = 'Enter an amount'; btn.disabled = true; return;
  }

  let res;
  try { res = computeMint(sqrtP, tl, tu, provided); }
  catch (e) { note.textContent = errMessage(e); btn.disabled = true; return; }
  if (res.liquidity <= 0n) { note.textContent = 'Amount too small for this range.'; btn.disabled = true; return; }

  // Fill the non-primary field with the computed counterpart amount.
  if (create.primary === 'amount0') a1in.value = formatUnits(res.amount1, t1.decimals);
  else a0in.value = formatUnits(res.amount0, t0.decimals);

  create.plan = { tl, tu, amount0: res.amount0, amount1: res.amount1, liquidity: res.liquidity, kind };
  note.textContent = `Liquidity ${res.liquidity} · needs ${fmt(res.amount0, t0.decimals, 6)} ${t0.symbol} + ${fmt(res.amount1, t1.decimals, 6)} ${t1.symbol}`;
  btn.textContent = 'Add liquidity';
  btn.disabled = !state.networkOk;
}

// Approve `spender` for `amount` of `token` if the current allowance is short.
// `btn` (optional) gets a transient "Approving …" label.
async function ensureAllowance(token, spender, amount, sym, btn) {
  if (amount <= 0n) return;
  const erc20 = new Contract(token, ERC20_ABI, state.signer);
  const allowance = await erc20.allowance(state.account, spender);
  if (allowance >= amount) return;
  if (btn) btn.textContent = `Approving ${sym}…`;
  toast(`Approve ${sym} — confirm in your wallet…`, 'pending');
  const atx = await erc20.approve(spender, MaxUint256, { gasLimit: V3_GAS.approve });
  await atx.wait(1, TX_TIMEOUT_MS);
}

// Pull tokenId + liquidity from the mint receipt's NFPM IncreaseLiquidity event.
function parseMintReceipt(receipt) {
  for (const log of receipt?.logs || []) {
    try {
      const p = nfpmIface.parseLog({ topics: log.topics, data: log.data });
      if (p && p.name === 'IncreaseLiquidity') {
        return { tokenId: p.args.tokenId.toString(), liquidity: p.args.liquidity.toString() };
      }
    } catch { /* not an NFPM event */ }
  }
  return null;
}

async function addLiquidity() {
  if (!requireWallet()) return;
  if (!create.plan) { toast('Enter an amount first.', 'err'); return; }
  const account = state.account;
  const plan = create.plan; // pin the plan; recomputeCreate is a no-op while busy
  const t0 = V3.token0, t1 = V3.token1;
  const btn = $('addLiqBtn');
  create.busy = true;
  btn.disabled = true;
  try {
    await ensureAllowance(t0.address, V3.nfpm, plan.amount0, t0.symbol, btn);
    await ensureAllowance(t1.address, V3.nfpm, plan.amount1, t1.symbol, btn);
    const nfpm = new Contract(V3.nfpm, NFPM_ABI, state.signer);
    const deadline = Math.floor(Date.now() / 1000) + 1200;
    btn.textContent = 'Minting position…';
    toast('Add liquidity — confirm in your wallet…', 'pending');
    const tx = await nfpm.mint({
      token0: t0.address, token1: t1.address, fee: V3.fee,
      tickLower: plan.tl, tickUpper: plan.tu,
      amount0Desired: plan.amount0, amount1Desired: plan.amount1,
      amount0Min: slipMin(plan.amount0), amount1Min: slipMin(plan.amount1),
      recipient: account, deadline,
    }, { gasLimit: V3_GAS.mint });
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    const got = parseMintReceipt(receipt);
    if (got) {
      addStoredPosition(account, {
        tokenId: got.tokenId, liquidity: got.liquidity,
        fee: V3.fee, tickLower: plan.tl, tickUpper: plan.tu,
        pool: V3.pool, token0: t0.address, token1: t1.address,
        block: receipt?.blockNumber ?? null,
      });
      toastConfirmed(`Position #${got.tokenId} created ✓`, receipt?.blockNumber);
    } else {
      toastConfirmed('Position created ✓ (tokenId not parsed from receipt)', receipt?.blockNumber);
    }
    $('amount0').value = ''; $('amount1').value = '';
    create.plan = null;
    if (state.account === account) {
      await refreshBalances();
      renderPoolState();
      renderPositions();
    }
  } catch (err) {
    if (err?.code === 'TIMEOUT') {
      toast('Confirmation timed out — the mint may still settle. Refresh shortly.', 'pending');
    } else {
      toast(`Add liquidity failed: ${errMessage(err)}`, 'err');
    }
  } finally {
    create.busy = false;
    recomputeCreate(); // re-derive the button label/enabled state from the current form
  }
}

function buildCreateForm() {
  $('tickLower').value = String(V3.minTick);
  $('tickUpper').value = String(V3.maxTick);
  $('tickLower').addEventListener('input', recomputeCreate);
  $('tickUpper').addEventListener('input', recomputeCreate);
  $('amount0').addEventListener('input', () => { if (create.primary === 'amount0') recomputeCreate(); });
  $('amount1').addEventListener('input', () => { if (create.primary === 'amount1') recomputeCreate(); });
  $('addLiqBtn').addEventListener('click', addLiquidity);
  for (const chip of $('rangePresets').children) {
    chip.addEventListener('click', () => {
      const r = chip.dataset.range;
      if (r === 'full') {
        $('tickLower').value = String(V3.minTick);
        $('tickUpper').value = String(V3.maxTick);
      } else {
        const c = state.poolNow ? nearestUsableTick(state.poolNow.tick, V3.tickSpacing) : 0;
        const span = Number(r);
        $('tickLower').value = String(nearestUsableTick(c - span, V3.tickSpacing));
        $('tickUpper').value = String(nearestUsableTick(c + span, V3.tickSpacing));
      }
      recomputeCreate();
    });
  }
  recomputeCreate();
}

// ── Manage positions (collect / increase / decrease) ────────────────────────
const MAX_U128 = (1n << 128n) - 1n;

// Per-position in-flight lock: prevents stacking mutations on the SAME position,
// which would let a later receipt overwrite tracked liquidity from a stale base.
const busyPositions = new Set();

function tickInRange(tick, tl, tu) { return tick >= tl && tick < tu; }

// Update a stored position's tracked liquidity after an increase/decrease.
// Clears any `stale` flag since this write is from a confirmed receipt.
function updateStoredLiquidity(account, tokenId, newLiquidity) {
  const arr = loadPositions(account);
  const i = arr.findIndex((p) => p.tokenId === tokenId);
  if (i < 0) return;
  arr[i].liquidity = (newLiquidity > 0n ? newLiquidity : 0n).toString();
  delete arr[i].stale;
  savePositions(account, arr);
}

// Mark a position's tracked liquidity as possibly out-of-date (e.g. a tx that timed
// out may still have confirmed). positions() is read-blocked, so we can't self-heal —
// we surface it and let the user Forget the position if tracking has drifted.
function markPositionStale(account, tokenId) {
  const arr = loadPositions(account);
  const i = arr.findIndex((p) => p.tokenId === tokenId);
  if (i < 0) return;
  arr[i].stale = true;
  savePositions(account, arr);
}

// Stop tracking a position locally (does NOT touch the on-chain NFT).
function removeStoredPosition(account, tokenId) {
  savePositions(account, loadPositions(account).filter((p) => p.tokenId !== tokenId));
}

function mkBtn(label, onClick) {
  const b = document.createElement('button');
  b.className = 'btn';
  b.textContent = label;
  b.addEventListener('click', () => onClick(b));
  return b;
}

function renderPositions() {
  const host = $('positionsList');
  if (!host) return;
  if (!state.account) { host.innerHTML = '<div class="empty">Connect to see your positions.</div>'; return; }
  const positions = loadPositions(state.account);
  if (!positions.length) {
    host.innerHTML = '<div class="empty">No tracked positions yet — add liquidity above.</div>';
    return;
  }
  host.innerHTML = '';
  const tick = state.poolNow?.tick;
  for (const p of positions) host.appendChild(buildPositionCard(p, tick));
}

function buildPositionCard(p, currentTick) {
  const t0 = V3.token0, t1 = V3.token1;
  const liq = (() => { try { return BigInt(p.liquidity || '0'); } catch { return 0n; } })();
  const closed = liq <= 0n;
  const inRange = currentTick != null && tickInRange(currentTick, p.tickLower, p.tickUpper);
  const stateLabel = closed ? 'closed' : inRange ? '● in range' : '○ out of range';

  const el = document.createElement('div');
  el.className = 'pos';
  el.innerHTML = `
    <div class="pos-head">
      <span class="pos-id">Position #${esc(p.tokenId)}</span>
      <span class="${inRange && !closed ? 'in-range' : 'out-range'}">${esc(stateLabel)}</span>
    </div>
    <div class="pos-range">ticks [${esc(p.tickLower)}, ${esc(p.tickUpper)}] · fee ${(Number(p.fee) / 10000).toFixed(2)}% · tracked L ${esc(p.liquidity)}</div>
    ${p.stale ? '<div class="warn-note">⚠ Tracked liquidity may be out of date (a transaction timed out). If it looks wrong, Forget this position.</div>' : ''}
    <div class="pos-actions"></div>
    <div class="pos-form"></div>`;
  const actions = el.querySelector('.pos-actions');
  const formHost = el.querySelector('.pos-form');

  actions.appendChild(mkBtn('Collect fees', (b) => doCollect(p, b)));
  if (!closed) {
    actions.appendChild(mkBtn('Increase', () => toggleIncreaseForm(p, formHost)));
    actions.appendChild(mkBtn('Decrease', () => toggleDecreaseForm(p, formHost)));
  }
  actions.appendChild(mkBtn('Burn (close)', (b) => doBurn(p, b)));
  // Forget: drop local tracking only (never touches the on-chain NFT). Not network-gated.
  const forget = mkBtn('Forget', () => {
    removeStoredPosition(state.account, p.tokenId);
    renderPositions();
  });
  actions.appendChild(forget);
  for (const b of actions.children) if (b !== forget) b.disabled = !state.networkOk;
  return el;
}

function parseNfpmEvent(receipt, name) {
  for (const log of receipt?.logs || []) {
    try {
      const p = nfpmIface.parseLog({ topics: log.topics, data: log.data });
      if (p && p.name === name) return p.args;
    } catch { /* not an NFPM event */ }
  }
  return null;
}

async function doCollect(p, btn) {
  if (!requireWallet()) return;
  if (busyPositions.has(p.tokenId)) { toast('An operation on this position is already pending.', 'err'); return; }
  const account = state.account;
  const orig = btn.textContent;
  busyPositions.add(p.tokenId);
  btn.disabled = true; btn.textContent = 'Collecting…';
  try {
    const nfpm = new Contract(V3.nfpm, NFPM_ABI, state.signer);
    toast('Collect fees — confirm in your wallet…', 'pending');
    const tx = await nfpm.collect(
      { tokenId: p.tokenId, recipient: account, amount0Max: MAX_U128, amount1Max: MAX_U128 },
      { gasLimit: V3_GAS.collect },
    );
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    const args = parseNfpmEvent(receipt, 'Collect');
    const msg = args
      ? `Collected ${fmt(args.amount0, V3.token0.decimals, 6)} ${V3.token0.symbol} + ${fmt(args.amount1, V3.token1.decimals, 6)} ${V3.token1.symbol} ✓`
      : 'Collected ✓';
    toastConfirmed(msg, receipt?.blockNumber);
    if (state.account === account) await refreshBalances();
  } catch (err) {
    if (err?.code === 'TIMEOUT') toast('Confirmation timed out — may still settle. Refresh shortly.', 'pending');
    else toast(`Collect failed: ${errMessage(err)}`, 'err');
  } finally {
    busyPositions.delete(p.tokenId);
    btn.disabled = false; btn.textContent = orig;
  }
}

// Increase: a kind-aware single-token input (mirrors create), then NFPM.increaseLiquidity.
// The token kind is re-derived on every input so a tick that crosses the range can't leave
// the form asking for the wrong token.
function toggleIncreaseForm(p, host) {
  if (host.dataset.open === 'inc') { host.innerHTML = ''; host.dataset.open = ''; return; }
  host.dataset.open = 'inc';
  host.innerHTML = `
    <div class="form-row"><div class="field">
      <label class="incLabel">Add liquidity</label>
      <input class="incAmt" type="text" inputmode="decimal" autocomplete="off" placeholder="0.0" />
    </div></div>
    <p class="form-note incNote">&nbsp;</p>
    <div class="btn-row"><button class="btn btn-primary incGo" disabled>Increase liquidity</button></div>`;
  const amt = host.querySelector('.incAmt');
  const label = host.querySelector('.incLabel');
  const note = host.querySelector('.incNote');
  const go = host.querySelector('.incGo');
  let plan = null;
  const recompute = () => {
    plan = null; go.disabled = true;
    if (!state.poolNow) { note.textContent = 'Pool state loading…'; return; }
    const tick = state.poolNow.tick;
    const kind = tick < p.tickLower ? 'token0-only' : tick >= p.tickUpper ? 'token1-only' : 'both';
    const useToken = kind === 'token1-only' ? V3.token1 : V3.token0;
    label.textContent = `Add ${useToken.symbol}`;
    const val = parseAmt(amt.value, useToken.decimals);
    if (val === null || val <= 0n) { note.textContent = 'Enter an amount.'; return; }
    const provided = kind === 'token1-only' ? { amount1: val } : { amount0: val };
    let res;
    try { res = computeMint(state.poolNow.sqrtPriceX96, p.tickLower, p.tickUpper, provided); }
    catch (e) { note.textContent = errMessage(e); return; }
    if (res.liquidity <= 0n) { note.textContent = 'Amount too small for this range.'; return; }
    plan = { amount0: res.amount0, amount1: res.amount1, liquidity: res.liquidity };
    note.textContent = `+${fmt(res.amount0, V3.token0.decimals, 6)} ${V3.token0.symbol} + ${fmt(res.amount1, V3.token1.decimals, 6)} ${V3.token1.symbol}`;
    go.disabled = !state.networkOk;
  };
  amt.addEventListener('input', recompute);
  // Recompute at click time too: the 12s pool refresh can move the tick across the range
  // after the user typed, so re-derive the plan/token against the CURRENT price before submit.
  go.addEventListener('click', () => { recompute(); if (plan) doIncrease(p, plan, go); });
  recompute(); // initial label
}

async function doIncrease(p, plan, btn) {
  if (!requireWallet()) return;
  if (busyPositions.has(p.tokenId)) { toast('An operation on this position is already pending.', 'err'); return; }
  const account = state.account;
  const orig = btn.textContent;
  busyPositions.add(p.tokenId);
  btn.disabled = true;
  try {
    const t0 = V3.token0, t1 = V3.token1;
    await ensureAllowance(t0.address, V3.nfpm, plan.amount0, t0.symbol, btn);
    await ensureAllowance(t1.address, V3.nfpm, plan.amount1, t1.symbol, btn);
    const nfpm = new Contract(V3.nfpm, NFPM_ABI, state.signer);
    const deadline = Math.floor(Date.now() / 1000) + 1200;
    btn.textContent = 'Increasing…';
    toast('Increase liquidity — confirm in your wallet…', 'pending');
    const tx = await nfpm.increaseLiquidity({
      tokenId: p.tokenId,
      amount0Desired: plan.amount0, amount1Desired: plan.amount1,
      amount0Min: slipMin(plan.amount0), amount1Min: slipMin(plan.amount1),
      deadline,
    }, { gasLimit: V3_GAS.increase });
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    const args = parseNfpmEvent(receipt, 'IncreaseLiquidity');
    if (args) {
      const prev = (() => { try { return BigInt(p.liquidity || '0'); } catch { return 0n; } })();
      updateStoredLiquidity(account, p.tokenId, prev + args.liquidity);
    }
    toastConfirmed(`Increased position #${p.tokenId} ✓`, receipt?.blockNumber);
    if (state.account === account) { await refreshBalances(); renderPoolState(); renderPositions(); }
  } catch (err) {
    if (err?.code === 'TIMEOUT') {
      // The tx may still confirm; we never saw the event, so tracked L is now uncertain.
      markPositionStale(account, p.tokenId);
      toast('Confirmation timed out — if it settled, tracked liquidity may be off (see warning).', 'pending');
      if (state.account === account) renderPositions();
    } else {
      toast(`Increase failed: ${errMessage(err)}`, 'err');
    }
  } finally {
    busyPositions.delete(p.tokenId);
    btn.disabled = false; btn.textContent = orig;
  }
}

// Decrease: remove a % of tracked liquidity. Tokens become owed; Collect withdraws them.
function toggleDecreaseForm(p, host) {
  if (host.dataset.open === 'dec') { host.innerHTML = ''; host.dataset.open = ''; return; }
  host.dataset.open = 'dec';
  host.innerHTML = `
    <div class="preset-btns" style="margin-top:10px">
      <span class="chip" data-pct="25">25%</span>
      <span class="chip" data-pct="50">50%</span>
      <span class="chip" data-pct="100">100% (close)</span>
    </div>
    <p class="form-note decNote">Removing liquidity credits tokens to the position — then Collect to withdraw.</p>`;
  for (const chip of host.querySelectorAll('.chip')) {
    chip.addEventListener('click', () => doDecrease(p, Number(chip.dataset.pct), chip));
  }
}

async function doDecrease(p, pct, chip) {
  if (!requireWallet()) return;
  if (busyPositions.has(p.tokenId)) { toast('An operation on this position is already pending.', 'err'); return; }
  const account = state.account;
  let liq;
  try { liq = BigInt(p.liquidity || '0'); } catch { liq = 0n; }
  if (liq <= 0n) { toast('Position has no tracked liquidity.', 'err'); return; }
  const remove = pct >= 100 ? liq : (liq * BigInt(pct)) / 100n;
  if (remove <= 0n) { toast('Nothing to remove.', 'err'); return; }
  // Expected withdrawn amounts (for slippage mins) from the V3 math.
  let a0Min = 0n, a1Min = 0n;
  if (state.poolNow) {
    const sa = getSqrtRatioAtTick(p.tickLower), sb = getSqrtRatioAtTick(p.tickUpper);
    const [e0, e1] = getAmountsForLiquidity(state.poolNow.sqrtPriceX96, sa, sb, remove);
    a0Min = slipMin(e0); a1Min = slipMin(e1);
  }
  busyPositions.add(p.tokenId);
  const orig = chip.textContent;
  chip.style.pointerEvents = 'none'; chip.textContent = '…';
  try {
    const nfpm = new Contract(V3.nfpm, NFPM_ABI, state.signer);
    const deadline = Math.floor(Date.now() / 1000) + 1200;
    toast(`Remove ${pct}% liquidity — confirm in your wallet…`, 'pending');
    const tx = await nfpm.decreaseLiquidity({
      tokenId: p.tokenId, liquidity: remove,
      amount0Min: a0Min, amount1Min: a1Min, deadline,
    }, { gasLimit: V3_GAS.decrease });
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    const args = parseNfpmEvent(receipt, 'DecreaseLiquidity');
    const removed = args ? args.liquidity : remove;
    updateStoredLiquidity(account, p.tokenId, liq - removed);
    toastConfirmed(`Removed ${pct}% of #${p.tokenId} ✓ — Collect to withdraw`, receipt?.blockNumber);
    if (state.account === account) { await refreshBalances(); renderPoolState(); renderPositions(); }
  } catch (err) {
    if (err?.code === 'TIMEOUT') {
      markPositionStale(account, p.tokenId);
      toast('Confirmation timed out — tracked liquidity may be off (see warning).', 'pending');
      if (state.account === account) renderPositions();
    } else {
      toast(`Decrease failed: ${errMessage(err)}`, 'err');
      chip.style.pointerEvents = ''; chip.textContent = orig;
    }
  } finally {
    busyPositions.delete(p.tokenId);
  }
}

// Burn = fully close a position: decreaseLiquidity(all) + collect(all) + burn, atomically
// via NFPM.multicall (one wallet confirmation). Relies on tracked liquidity being accurate
// (positions() is read-blocked); if it's stale the on-chain calls revert and the user can Forget.
async function doBurn(p, btn) {
  if (!requireWallet()) return;
  if (busyPositions.has(p.tokenId)) { toast('An operation on this position is already pending.', 'err'); return; }
  const account = state.account;
  let liq;
  try { liq = BigInt(p.liquidity || '0'); } catch { liq = 0n; }
  const orig = btn.textContent;
  busyPositions.add(p.tokenId);
  btn.disabled = true; btn.textContent = 'Closing…';
  try {
    const deadline = Math.floor(Date.now() / 1000) + 1200;
    const calls = [];
    if (liq > 0n) {
      calls.push(nfpmIface.encodeFunctionData('decreaseLiquidity', [{
        tokenId: p.tokenId, liquidity: liq, amount0Min: 0n, amount1Min: 0n, deadline,
      }]));
    }
    calls.push(nfpmIface.encodeFunctionData('collect', [{
      tokenId: p.tokenId, recipient: account, amount0Max: MAX_U128, amount1Max: MAX_U128,
    }]));
    calls.push(nfpmIface.encodeFunctionData('burn', [p.tokenId]));
    const nfpm = new Contract(V3.nfpm, NFPM_ABI, state.signer);
    toast('Close position (remove + collect + burn) — confirm in your wallet…', 'pending');
    const tx = await nfpm.multicall(calls, { gasLimit: V3_GAS.mint });
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    removeStoredPosition(account, p.tokenId);
    toastConfirmed(`Position #${p.tokenId} closed & burned ✓`, receipt?.blockNumber);
    if (state.account === account) { await refreshBalances(); renderPoolState(); renderPositions(); }
  } catch (err) {
    if (err?.code === 'TIMEOUT') {
      markPositionStale(account, p.tokenId);
      toast('Confirmation timed out — may still settle. If it did, Forget the position.', 'pending');
      if (state.account === account) renderPositions();
    } else {
      toast(`Burn failed: ${errMessage(err)}`, 'err');
    }
  } finally {
    busyPositions.delete(p.tokenId);
    btn.disabled = false; btn.textContent = orig;
  }
}

// ── Swap (SwapRouter.exactInputSingle) ──────────────────────────────────────
const SWAP_SLIPPAGE_BPS = 100n; // 1%
const swap = { plan: null, busy: false };

function swapDirTokens() {
  return $('swapDir').value === '1to0'
    ? { tokenIn: V3.token1, tokenOut: V3.token0, zeroForOne: false }
    : { tokenIn: V3.token0, tokenOut: V3.token1, zeroForOne: true };
}

function recomputeSwap() {
  if (swap.busy) return;
  const note = $('swapNote');
  const go = $('swapGo');
  const { tokenIn, tokenOut, zeroForOne } = swapDirTokens();
  $('swapInLabel').textContent = `You pay (${tokenIn.symbol})`;
  swap.plan = null;
  const amountIn = parseAmt($('swapAmtIn').value, tokenIn.decimals);
  if (amountIn === null || amountIn <= 0n) { note.textContent = ' '; go.textContent = 'Enter an amount'; go.disabled = true; return; }
  if (!state.poolNow || state.poolNow.liquidity <= 0n) { note.textContent = 'Pool state loading…'; go.disabled = true; return; }
  const q = quoteSingleTick(state.poolNow.sqrtPriceX96, state.poolNow.liquidity, amountIn, zeroForOne);
  if (q.amountOut <= 0n) { note.textContent = 'Amount too small.'; go.textContent = 'Enter an amount'; go.disabled = true; return; }
  const minOut = (q.amountOut * (BPS - SWAP_SLIPPAGE_BPS)) / BPS;
  swap.plan = { tokenIn, tokenOut, zeroForOne, amountIn, amountOut: q.amountOut, minOut };
  note.textContent = `≈ ${fmt(q.amountOut, tokenOut.decimals, 6)} ${tokenOut.symbol} · min ${fmt(minOut, tokenOut.decimals, 6)} (1% slippage) · estimate assumes no tick crossing`;
  go.textContent = `Swap ${tokenIn.symbol} → ${tokenOut.symbol}`;
  go.disabled = !state.networkOk;
}

async function doSwap() {
  if (!requireWallet()) return;
  if (!swap.plan) { toast('Enter an amount.', 'err'); return; }
  const account = state.account;
  const plan = swap.plan;
  const go = $('swapGo');
  swap.busy = true;
  go.disabled = true;
  try {
    await ensureAllowance(plan.tokenIn.address, V3.swapRouter, plan.amountIn, plan.tokenIn.symbol, go);
    const router = new Contract(V3.swapRouter, V3ROUTER_ABI, state.signer);
    const deadline = Math.floor(Date.now() / 1000) + 1200;
    go.textContent = 'Swapping…';
    toast(`Swap ${plan.tokenIn.symbol} → ${plan.tokenOut.symbol} — confirm in your wallet…`, 'pending');
    const tx = await router.exactInputSingle({
      tokenIn: plan.tokenIn.address, tokenOut: plan.tokenOut.address, fee: V3.fee,
      recipient: account, deadline, amountIn: plan.amountIn,
      amountOutMinimum: plan.minOut, sqrtPriceLimitX96: 0n,
    }, { gasLimit: V3_GAS.swap });
    toast(`Submitted ${tx.hash.slice(0, 10)}… — waiting…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    toastConfirmed(`Swapped ${plan.tokenIn.symbol} → ${plan.tokenOut.symbol} ✓`, receipt?.blockNumber);
    $('swapAmtIn').value = '';
    swap.plan = null;
    if (state.account === account) { await refreshBalances(); renderPoolState(); renderPositions(); }
  } catch (err) {
    if (err?.code === 'TIMEOUT') toast('Confirmation timed out — the swap may still settle. Refresh shortly.', 'pending');
    else toast(`Swap failed: ${errMessage(err)}`, 'err');
  } finally {
    swap.busy = false;
    recomputeSwap();
  }
}

function buildSwapForm() {
  const t0 = V3.token0, t1 = V3.token1;
  $('swapDir').options[0].textContent = `${t0.symbol} → ${t1.symbol}`;
  $('swapDir').options[1].textContent = `${t1.symbol} → ${t0.symbol}`;
  $('swapAmtIn').addEventListener('input', recomputeSwap);
  $('swapDir').addEventListener('change', () => { $('swapAmtIn').value = ''; recomputeSwap(); });
  $('swapGo').addEventListener('click', doSwap);
  recomputeSwap();
}

// ── Network gate / connect ──────────────────────────────────────────────────
function requireWallet() {
  if (!state.signer) { toast('Connect your wallet first.', 'err'); return false; }
  if (!state.networkOk) { toast('Switch to the Koinos EVM network in your wallet.', 'err'); return false; }
  return true;
}

function applyNetworkGate() {
  for (const b of $('faucetBtns').children) b.disabled = !state.networkOk;
  recomputeCreate(); // re-gates the Add-liquidity button on network state
  recomputeSwap();   // re-gates the Swap button on network state
}

async function renderNetwork() {
  const pill = $('network');
  state.networkOk = await chainOk(state.provider);
  if (state.networkOk) {
    pill.textContent = `${CHAIN.chainName} (${CHAIN.chainIdNum})`;
    pill.className = 'pill';
  } else {
    let cid = '?';
    try { cid = Number((await state.provider.getNetwork()).chainId); } catch {}
    pill.textContent = `Wrong network (${cid})`;
    pill.className = 'pill warn';
  }
  applyNetworkGate();
}

async function onConnected() {
  $('connectBtn').textContent = shortAddr(state.account);
  $('account').textContent = state.account;
  $('statusCard').classList.remove('hidden');
  $('balancesCard').classList.remove('hidden');
  $('createCard').classList.remove('hidden');
  $('positionsCard').classList.remove('hidden');
  $('swapCard').classList.remove('hidden');
  await renderNetwork();
  await refreshBalances();
  renderPositions();
}

async function doConnect() {
  const btn = $('connectBtn');
  btn.disabled = true;
  try {
    const { provider, signer, account } = await walletConnect();
    state.provider = provider; state.signer = signer; state.account = account;
    await onConnected();
  } catch (err) {
    toast(`Connect failed: ${errMessage(err)}`, 'err');
  } finally {
    btn.disabled = false;
  }
}

function wireWalletEvents() {
  if (!window.ethereum?.on) return;
  window.ethereum.on('accountsChanged', async (accounts) => {
    balanceSeq++;
    if (!accounts || accounts.length === 0) {
      state.account = null; state.signer = null; state.networkOk = false;
      $('connectBtn').textContent = 'Connect MetaMask';
      $('statusCard').classList.add('hidden');
      $('balancesCard').classList.add('hidden');
      $('createCard').classList.add('hidden');
      $('positionsCard').classList.add('hidden');
      $('swapCard').classList.add('hidden');
      applyNetworkGate();
      return;
    }
    // Re-create provider/signer for the new account (may change without a fresh Connect).
    state.provider = new BrowserProvider(window.ethereum);
    state.signer = await state.provider.getSigner(accounts[0]);
    state.account = accounts[0];
    await onConnected();
  });
  window.ethereum.on('chainChanged', async () => {
    state.provider = new BrowserProvider(window.ethereum);
    if (state.account) {
      state.signer = await state.provider.getSigner();
      await renderNetwork();
      await refreshBalances();
    }
  });
}

// ── Init ────────────────────────────────────────────────────────────────────
$('poolFoot').textContent = V3.pool;
$('connectBtn').addEventListener('click', doConnect);
$('refreshPoolBtn').addEventListener('click', renderPoolState);
buildFaucet();
buildCreateForm();
buildSwapForm();
wireWalletEvents();
renderPoolState();
setInterval(renderPoolState, 12000); // keep the tick/price live as swaps move the pool
