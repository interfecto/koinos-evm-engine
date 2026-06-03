import {
  BrowserProvider,
  JsonRpcProvider,
  Contract,
  formatUnits,
  parseUnits,
  MaxUint256,
} from 'https://esm.sh/ethers@6.13.4';

import {
  CHAIN,
  TOKENS,
  ERC20_ABI,
  PAIR_ABI,
  ROUTER_ABI,
  CONTRACTS,
  GAS,
  FEE_NUM,
  FEE_DEN,
  SLIPPAGE_BPS,
  BPS_DEN,
  DEADLINE_SECS,
  TX_TIMEOUT_MS,
  EXPLORER_BLOCK_URL,
} from './config.js';

// Whole tokens dispensed per faucet click.
const FAUCET_AMOUNT = '1000';

// Direct read provider — used for all view calls (balances, reserves). Works
// before connect and avoids leaning on MetaMask's provider for reads. The proxy
// sends permissive CORS so the browser can call it directly.
const readProvider = new JsonRpcProvider(CHAIN.rpcUrl, {
  chainId: CHAIN.chainIdNum,
  name: CHAIN.chainName,
});

const state = {
  browserProvider: null,
  signer: null,
  account: null,
};

const $ = (id) => document.getElementById(id);

// ── UI helpers ────────────────────────────────────────────────────────────
function shortAddr(a) {
  return a ? `${a.slice(0, 6)}…${a.slice(-4)}` : '—';
}

function shortHash(h) {
  return h ? `${h.slice(0, 10)}…` : '';
}

// Format a uint256 token amount for display WITHOUT going through Number()
// (which loses precision on large balances — and mint() here is open).
function fmt(value, decimals, places = 4) {
  const s = formatUnits(value, decimals); // exact decimal string, e.g. "1006.870904111951303840"
  const [intPart, fracPart = ''] = s.split('.');
  const grouped = intPart.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  const frac = fracPart.slice(0, places).replace(/0+$/, '');
  return frac ? `${grouped}.${frac}` : grouped;
}

let toastTimer = null;
function toast(msg, kind = '') {
  const el = $('toast');
  el.textContent = msg;
  el.className = `toast ${kind}`.trim();
  if (toastTimer) clearTimeout(toastTimer);
  if (kind !== 'pending') {
    toastTimer = setTimeout(() => el.classList.add('hidden'), 6000);
  }
}

// Success toast that appends a "confirmed in block N" explorer link.
// Built via DOM nodes (not innerHTML) — message is controlled, block is numeric.
function toastConfirmed(message, blockNumber, kind = 'ok') {
  const el = $('toast');
  el.className = `toast ${kind}`.trim();
  if (blockNumber === undefined || blockNumber === null) {
    el.textContent = message;
  } else {
    el.textContent = `${message} · confirmed in `;
    const a = document.createElement('a');
    a.href = `${EXPLORER_BLOCK_URL}${blockNumber}`;
    a.target = '_blank';
    a.rel = 'noopener noreferrer';
    a.className = 'toast-link';
    a.textContent = `block ${blockNumber}`;
    el.appendChild(a);
  }
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.add('hidden'), 12000);
}

// Surface a readable message from MetaMask / RPC errors.
function errMessage(err) {
  return (
    err?.info?.error?.message ||
    err?.shortMessage ||
    err?.reason ||
    err?.data?.originalError?.message ||
    err?.error?.message ||
    err?.data?.message ||
    err?.message ||
    String(err)
  );
}

// ── Network ───────────────────────────────────────────────────────────────
async function ensureNetwork() {
  const eth = window.ethereum;
  try {
    await eth.request({
      method: 'wallet_switchEthereumChain',
      params: [{ chainId: CHAIN.chainIdHex }],
    });
  } catch (err) {
    const code = err?.code ?? err?.data?.originalError?.code;
    if (code === 4902) {
      await eth.request({
        method: 'wallet_addEthereumChain',
        params: [
          {
            chainId: CHAIN.chainIdHex,
            chainName: CHAIN.chainName,
            rpcUrls: [CHAIN.rpcUrl],
            nativeCurrency: CHAIN.nativeCurrency,
          },
        ],
      });
      // Adding a chain does not guarantee the wallet selects it — switch explicitly.
      await eth.request({
        method: 'wallet_switchEthereumChain',
        params: [{ chainId: CHAIN.chainIdHex }],
      });
    } else {
      throw err;
    }
  }
}

async function renderNetwork() {
  const pill = $('network');
  try {
    const net = await state.browserProvider.getNetwork();
    networkOk = Number(net.chainId) === CHAIN.chainIdNum;
    pill.textContent = networkOk
      ? `${CHAIN.chainName} (${CHAIN.chainIdNum})`
      : `Wrong network (${net.chainId})`;
    pill.className = networkOk ? 'pill' : 'pill warn';
  } catch {
    networkOk = false;
    pill.textContent = 'unknown';
    pill.className = 'pill warn';
  }
  applyNetworkGate();
}

// ── Balances ──────────────────────────────────────────────────────────────
let balanceSeq = 0;
async function refreshBalances() {
  if (!state.account) return;
  const account = state.account;
  const seq = ++balanceSeq;
  // Read all balances off-DOM first, then commit only if this is still the
  // latest refresh for the same account — avoids interleaving concurrent refreshes.
  const rows = [];
  for (const t of TOKENS) {
    let amt = 'error';
    let wei;
    try {
      const c = new Contract(t.address, ERC20_ABI, readProvider);
      wei = await c.balanceOf(account);
      amt = fmt(wei, t.decimals);
    } catch (e) {
      amt = 'error';
    }
    rows.push({ t, amt, wei });
  }
  if (seq !== balanceSeq || state.account !== account) return; // superseded
  const ul = $('balances');
  ul.innerHTML = '';
  for (const { t, amt, wei } of rows) {
    // Keep the cache in lockstep with the committed read — drop stale values on failure.
    if (wei !== undefined) tokenBalances[t.address.toLowerCase()] = wei;
    else delete tokenBalances[t.address.toLowerCase()];
    const li = document.createElement('li');
    li.className = 'bal';
    li.innerHTML = `<span class="sym">${t.symbol}<span class="name">${t.name}</span></span><span class="amt">${amt}</span>`;
    ul.appendChild(li);
  }
  updateSwapBalances();
}

// Reflect the cached balances inline in the swap card (and gate the Max button).
function updateSwapBalances() {
  const bi = tokenBalances[swap.in.address.toLowerCase()];
  const bo = tokenBalances[swap.out.address.toLowerCase()];
  $('balIn').textContent = bi === undefined ? '—' : `${fmt(bi, swap.in.decimals)} ${swap.in.symbol}`;
  $('balOut').textContent = bo === undefined ? '—' : `${fmt(bo, swap.out.decimals)} ${swap.out.symbol}`;
  $('maxBtn').disabled = !networkOk || swapInFlight || bi === undefined || bi <= 0n;
}

// ── Faucet (mint) ─────────────────────────────────────────────────────────
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
}

async function mintToken(token, btn) {
  if (!state.signer) {
    toast('Connect your wallet first.', 'err');
    return;
  }
  if (!networkOk) {
    toast('Switch to the Koinos EVM network in your wallet.', 'err');
    return;
  }
  const account = state.account; // pin the target across the async tx lifecycle
  const original = btn.textContent;
  btn.disabled = true;
  btn.textContent = 'Minting…';
  try {
    const c = new Contract(token.address, ERC20_ABI, state.signer);
    const amount = parseUnits(FAUCET_AMOUNT, token.decimals);
    toast(`Requesting ${FAUCET_AMOUNT} ${token.symbol} — confirm in your wallet…`, 'pending');
    const tx = await c.mint(account, amount, { gasLimit: GAS.mint });
    toast(`Submitted ${shortHash(tx.hash)} — waiting for confirmation…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    toastConfirmed(`Minted ${FAUCET_AMOUNT} ${token.symbol} ✓`, receipt?.blockNumber);
    // Only refresh if the active account is still the one we minted to.
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

// ── Swap ──────────────────────────────────────────────────────────────────
const swap = { in: TOKENS[0], out: TOKENS[1] };
let quoteTimer = null;
let swapInFlight = false; // blocks quotes/re-enable while a swap tx is pending
let quoteSeq = 0; // discards stale out-of-order quote responses
let networkOk = false; // wallet is on the Koinos EVM chain — gates all writes
const tokenBalances = {}; // token address (lowercase) -> bigint wei, for inline swap balances + Max

function tokenByKey(key) {
  return TOKENS.find((t) => t.key === key);
}

// Uniswap V2 constant-product output (mirrors UniswapV2Library.getAmountOut).
function getAmountOut(amountIn, reserveIn, reserveOut) {
  if (amountIn <= 0n || reserveIn <= 0n || reserveOut <= 0n) return 0n;
  const inWithFee = amountIn * FEE_NUM;
  return (inWithFee * reserveOut) / (reserveIn * FEE_DEN + inWithFee);
}

// Reserves mapped to [in, out] for the given tokens using token0 = smaller-address ordering.
// Tokens are passed explicitly so callers can pin a direction across async steps.
async function reservesInOut(tokenIn, tokenOut) {
  const pair = new Contract(CONTRACTS.pair, PAIR_ABI, readProvider);
  const [r0, r1] = await pair.getReserves();
  const inIsToken0 = BigInt(tokenIn.address) < BigInt(tokenOut.address);
  return inIsToken0 ? [r0, r1] : [r1, r0];
}

// Lock/unlock the direction controls while a swap is mid-flight.
function setSwapControlsDisabled(disabled) {
  $('tokenIn').disabled = disabled;
  $('tokenOut').disabled = disabled;
  $('flipBtn').disabled = disabled;
}

function parseAmountIn() {
  const raw = $('amountIn').value.trim();
  if (!raw) return null;
  try {
    return parseUnits(raw, swap.in.decimals);
  } catch {
    return null;
  }
}

function setSwapButton(text, disabled) {
  const btn = $('swapBtn');
  btn.textContent = text;
  btn.disabled = disabled || !state.signer || swapInFlight || !networkOk;
}

// Enable writes (faucet + swap) only while the wallet is on the Koinos EVM chain.
// Reads always come from localhost, so without this a write could hit whatever
// network MetaMask is actually on.
function applyNetworkGate() {
  if (swapInFlight) return; // doSwap owns the controls
  for (const b of $('faucetBtns').children) b.disabled = !networkOk;
  setSwapControlsDisabled(!networkOk);
  $('amountIn').disabled = !networkOk;
  updateSwapBalances(); // re-gates the Max button on network state
  if (networkOk) updateQuote();
  else setSwapButton('Wrong network', true);
}

async function updateQuote() {
  if (swapInFlight) return; // doSwap owns the UI while a swap is pending
  // Tag every invocation (including blank-input ones) so an earlier in-flight
  // read can't repaint a stale quote after the input changed or was cleared.
  const seq = ++quoteSeq;
  const amountIn = parseAmountIn();
  const out = $('amountOut');
  const meta = $('swapMeta');
  if (amountIn === null || amountIn <= 0n) {
    out.value = '';
    meta.innerHTML = '&nbsp;';
    setSwapButton('Enter an amount', true);
    return;
  }
  // Pin the tokens for this quote.
  const tokenIn = swap.in;
  const tokenOut = swap.out;
  try {
    const [reserveIn, reserveOut] = await reservesInOut(tokenIn, tokenOut);
    if (seq !== quoteSeq || swapInFlight) return; // superseded
    const amountOut = getAmountOut(amountIn, reserveIn, reserveOut);
    out.value = fmt(amountOut, tokenOut.decimals, 6);
    if (amountOut <= 0n) {
      meta.textContent = 'Insufficient liquidity';
      setSwapButton('Insufficient liquidity', true);
      return;
    }
    const minOut = (amountOut * (BPS_DEN - SLIPPAGE_BPS)) / BPS_DEN;
    if (minOut <= 0n) {
      meta.textContent = 'Amount too small for slippage protection';
      setSwapButton('Amount too small', true);
      return;
    }
    const rate = (amountOut * 10n ** BigInt(tokenIn.decimals)) / amountIn;
    meta.textContent =
      `1 ${tokenIn.symbol} ≈ ${fmt(rate, tokenOut.decimals, 6)} ${tokenOut.symbol}` +
      ` · min received ${fmt(minOut, tokenOut.decimals, 6)} ${tokenOut.symbol}`;
    setSwapButton(`Swap ${tokenIn.symbol} → ${tokenOut.symbol}`, false);
  } catch (err) {
    if (seq !== quoteSeq || swapInFlight) return;
    meta.textContent = `Quote error: ${errMessage(err)}`;
    setSwapButton('Swap', true);
  }
}

function onTokenChange(which) {
  // With the two tokens distinct, force the other selector off a collision.
  if ($('tokenIn').value === $('tokenOut').value) {
    const keep = which === 'in' ? $('tokenIn').value : $('tokenOut').value;
    const other = TOKENS.find((t) => t.key !== keep);
    if (which === 'in') $('tokenOut').value = other.key;
    else $('tokenIn').value = other.key;
  }
  swap.in = tokenByKey($('tokenIn').value);
  swap.out = tokenByKey($('tokenOut').value);
  updateSwapBalances();
  updateQuote();
}

function flipTokens() {
  [swap.in, swap.out] = [swap.out, swap.in];
  $('tokenIn').value = swap.in.key;
  $('tokenOut').value = swap.out.key;
  updateSwapBalances();
  updateQuote();
}

async function doSwap() {
  if (!state.signer) {
    toast('Connect your wallet first.', 'err');
    return;
  }
  if (!networkOk) {
    toast('Switch to the Koinos EVM network in your wallet.', 'err');
    return;
  }
  const amountIn = parseAmountIn();
  if (amountIn === null || amountIn <= 0n) {
    toast('Enter a valid amount.', 'err');
    return;
  }
  const account = state.account;
  const tokenIn = swap.in;
  const tokenOut = swap.out;
  // Take exclusive control of the swap UI: block quotes, drop any pending
  // debounced quote, and lock the direction controls so the pinned path can't
  // diverge from what the user sees.
  swapInFlight = true;
  if (quoteTimer) clearTimeout(quoteTimer);
  $('swapBtn').disabled = true;
  $('amountIn').disabled = true;
  setSwapControlsDisabled(true);
  updateSwapBalances(); // disables Max while the swap is in flight
  try {
    // 1. Approve the router if the current allowance is insufficient.
    const erc20 = new Contract(tokenIn.address, ERC20_ABI, state.signer);
    const allowance = await erc20.allowance(account, CONTRACTS.router);
    if (allowance < amountIn) {
      $('swapBtn').textContent = `Approving ${tokenIn.symbol}…`;
      toast(`Approve ${tokenIn.symbol} — confirm in your wallet…`, 'pending');
      const atx = await erc20.approve(CONTRACTS.router, MaxUint256, { gasLimit: GAS.approve });
      await atx.wait(1, TX_TIMEOUT_MS);
    }
    // 2. Re-quote against fresh reserves (pinned direction) to derive min-out at submit time.
    const [reserveIn, reserveOut] = await reservesInOut(tokenIn, tokenOut);
    const amountOut = getAmountOut(amountIn, reserveIn, reserveOut);
    if (amountOut <= 0n) throw new Error('insufficient liquidity');
    const minOut = (amountOut * (BPS_DEN - SLIPPAGE_BPS)) / BPS_DEN;
    if (minOut <= 0n) throw new Error('amount too small for slippage protection');
    // 3. Swap.
    $('swapBtn').textContent = 'Swapping…';
    toast(`Swap ${tokenIn.symbol} → ${tokenOut.symbol} — confirm in your wallet…`, 'pending');
    const router = new Contract(CONTRACTS.router, ROUTER_ABI, state.signer);
    const deadline = Math.floor(Date.now() / 1000) + DEADLINE_SECS;
    const tx = await router.swapExactTokensForTokens(
      amountIn,
      minOut,
      [tokenIn.address, tokenOut.address],
      account,
      deadline,
      { gasLimit: GAS.swap },
    );
    toast(`Submitted ${shortHash(tx.hash)} — waiting for confirmation…`, 'pending');
    const receipt = await tx.wait(1, TX_TIMEOUT_MS);
    toastConfirmed(`Swapped ${tokenIn.symbol} → ${tokenOut.symbol} ✓`, receipt?.blockNumber);
    $('amountIn').value = '';
    if (state.account === account) await refreshBalances();
  } catch (err) {
    if (err?.code === 'TIMEOUT') {
      toast('Confirmation timed out — the swap may still settle. Refresh shortly.', 'pending');
      if (state.account === account) await refreshBalances();
    } else {
      toast(`Swap failed: ${errMessage(err)}`, 'err');
    }
  } finally {
    swapInFlight = false;
    $('amountIn').disabled = false;
    setSwapControlsDisabled(false);
    updateSwapBalances(); // re-gates Max now that the swap is done
    await updateQuote();
  }
}

function buildSwap() {
  for (const sel of [$('tokenIn'), $('tokenOut')]) {
    sel.innerHTML = '';
    for (const t of TOKENS) {
      const opt = document.createElement('option');
      opt.value = t.key;
      opt.textContent = t.symbol;
      sel.appendChild(opt);
    }
  }
  $('tokenIn').value = swap.in.key;
  $('tokenOut').value = swap.out.key;
  $('tokenIn').addEventListener('change', () => onTokenChange('in'));
  $('tokenOut').addEventListener('change', () => onTokenChange('out'));
  $('flipBtn').addEventListener('click', flipTokens);
  $('maxBtn').addEventListener('click', () => {
    const bi = tokenBalances[swap.in.address.toLowerCase()];
    if (bi === undefined || bi <= 0n) return;
    $('amountIn').value = formatUnits(bi, swap.in.decimals);
    updateQuote();
  });
  $('amountIn').addEventListener('input', () => {
    if (quoteTimer) clearTimeout(quoteTimer);
    quoteTimer = setTimeout(updateQuote, 250);
  });
  $('swapBtn').addEventListener('click', doSwap);
}

// ── Connect ───────────────────────────────────────────────────────────────
async function connect() {
  if (!window.ethereum) {
    toast('MetaMask not detected — install the extension to continue.', 'err');
    return;
  }
  const btn = $('connectBtn');
  btn.disabled = true;
  try {
    state.browserProvider = new BrowserProvider(window.ethereum);
    await state.browserProvider.send('eth_requestAccounts', []);
    await ensureNetwork();
    // Re-create provider after a possible network switch so it reads fresh chain state.
    state.browserProvider = new BrowserProvider(window.ethereum);
    state.signer = await state.browserProvider.getSigner();
    state.account = await state.signer.getAddress();
    await onConnected();
  } catch (err) {
    toast(`Connect failed: ${errMessage(err)}`, 'err');
  } finally {
    btn.disabled = false;
  }
}

async function onConnected() {
  $('connectBtn').textContent = shortAddr(state.account);
  $('account').textContent = state.account;
  $('statusCard').classList.remove('hidden');
  $('balancesCard').classList.remove('hidden');
  $('faucetCard').classList.remove('hidden');
  $('swapCard').classList.remove('hidden');
  await renderNetwork();
  await refreshBalances();
  await updateQuote();
}

// ── Wallet events ─────────────────────────────────────────────────────────
function wireWalletEvents() {
  if (!window.ethereum?.on) return;
  window.ethereum.on('accountsChanged', async (accounts) => {
    // Drop the previous account's cached balances and invalidate any in-flight
    // refresh so the old account's balances can't repopulate during the awaits below.
    for (const k in tokenBalances) delete tokenBalances[k];
    balanceSeq++;
    updateSwapBalances();
    if (!accounts || accounts.length === 0) {
      // Disconnected.
      state.account = null;
      state.signer = null;
      $('connectBtn').textContent = 'Connect MetaMask';
      $('statusCard').classList.add('hidden');
      $('balancesCard').classList.add('hidden');
      $('faucetCard').classList.add('hidden');
      $('swapCard').classList.add('hidden');
      return;
    }
    // The account can change before this session pressed Connect (prior
    // permission), so (re)create the provider rather than assume it exists.
    state.browserProvider = new BrowserProvider(window.ethereum);
    state.signer = await state.browserProvider.getSigner(accounts[0]);
    state.account = accounts[0];
    await onConnected();
  });
  window.ethereum.on('chainChanged', async () => {
    state.browserProvider = new BrowserProvider(window.ethereum);
    if (state.account) {
      state.signer = await state.browserProvider.getSigner();
      await renderNetwork();
      await refreshBalances();
    }
  });
}

// ── Init ──────────────────────────────────────────────────────────────────
$('connectBtn').addEventListener('click', connect);
$('refreshBtn').addEventListener('click', refreshBalances);
buildFaucet();
buildSwap();
wireWalletEvents();

// Expose for later steps (K3 faucet, K4 swap) and debugging.
export { state, readProvider, refreshBalances, toast, toastConfirmed, errMessage, fmt };
