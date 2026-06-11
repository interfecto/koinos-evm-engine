// Koinos EVM Quest — connect → faucet → swap → leave your pixel.
//
// Each step is verified ON-CHAIN, not just in page state: faucet via balanceOf,
// swap via the V3 pool's Swap event (recipient topic) or an in-session receipt,
// pixels via pixelsBy(). The canvas streams PixelsSet events over the proxy's
// WebSocket (same port as HTTP; GET upgrades) and falls back to polling.
//
// Uses the VENDORED UMD ethers (vendor/ethers.umd.min.js, loaded by quest.html)
// instead of a CDN so the page works offline / behind a tunnel.

import {
  CHAIN, TOKENS, V3, V3ROUTER_ABI, V3_GAS, ERC20_ABI, GAS,
  PIXEL, PIXEL_ABI, PIXEL_GAS, TX_TIMEOUT_MS, EXPLORER,
  ADDRESS_LABELS, ABI_FAMILIES,
} from './config.js';

const { ethers } = window;

const TFA = TOKENS[0];
const TFB = TOKENS[1];
const MINT_AMOUNT = ethers.parseUnits('1000', TFA.decimals);
const SWAP_AMOUNT = ethers.parseUnits('10', TFA.decimals);
const W = PIXEL.width; // 64
const CELL = 512 / W;  // 8px per cell at native canvas resolution

// ── providers / contracts ──────────────────────────────────────────────────
const readProvider = new ethers.JsonRpcProvider(CHAIN.rpcUrl, {
  chainId: CHAIN.chainIdNum,
  name: CHAIN.chainName,
});
const pixelRead = new ethers.Contract(PIXEL.address, PIXEL_ABI, readProvider);
const pixelIface = new ethers.Interface(PIXEL_ABI);

let provider = null;
let signer = null;
let account = null;

// ── DOM ─────────────────────────────────────────────────────────────────────
const $ = (id) => document.getElementById(id);
const els = {
  connectBtn: $('connectBtn'), mintBtn: $('mintBtn'), swapBtn: $('swapBtn'),
  paintBtn: $('paintBtn'), clearBtn: $('clearBtn'),
  tfaBal: $('tfaBal'), tfbBal: $('tfbBal'), swapInfo: $('swapInfo'),
  pendingInfo: $('pendingInfo'), statTotal: $('statTotal'), statMine: $('statMine'),
  liveDot: $('liveDot'), liveLabel: $('liveLabel'),
  activity: $('activity'), palette: $('palette'), questDone: $('questDone'),
  toast: $('toast'),
  steps: [$('step1'), $('step2'), $('step3'), $('step4')],
};
const canvasEl = $('pixels');
const ctx = canvasEl.getContext('2d');

// ── toast (ported from wallet.js; kept local to avoid its CDN ethers import) ─
let toastTimer = null;
function toast(msg, kind = '') {
  els.toast.textContent = msg;
  els.toast.className = `toast ${kind}`.trim();
  els.toast.classList.remove('hidden');
  if (toastTimer) clearTimeout(toastTimer);
  if (kind !== 'pending') toastTimer = setTimeout(() => els.toast.classList.add('hidden'), 6000);
}
function toastConfirmed(message, blockNumber) {
  els.toast.className = 'toast ok';
  els.toast.classList.remove('hidden');
  els.toast.textContent = `${message} · confirmed in `;
  const a = document.createElement('a');
  a.href = `${EXPLORER.koinosRestUrl}/block/${encodeURIComponent(blockNumber)}`;
  a.target = '_blank';
  a.rel = 'noopener noreferrer';
  a.className = 'toast-link';
  a.textContent = `block ${blockNumber}`;
  els.toast.appendChild(a);
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => els.toast.classList.add('hidden'), 12000);
}
function errMessage(err) {
  // Dig for the deepest JSON-RPC message first: MetaMask nests the real reason
  // under data / data.originalError, and ethers wraps THAT under info.error when
  // it can't classify the shape (its toast-worthless "could not coalesce error").
  const raw =
    err?.info?.error?.data?.originalError?.message ||
    err?.info?.error?.data?.message ||
    err?.data?.originalError?.message ||
    err?.info?.error?.message ||
    err?.shortMessage || err?.reason ||
    err?.error?.message || err?.data?.message ||
    err?.message || String(err);
  // Known relay-side conditions → actionable text.
  if (/insufficient pending account resources/i.test(raw)) {
    return 'The relay’s mana is briefly maxed out — each pending tx reserves some until finality (~3 min). Wait a minute, then try again.';
  }
  if (/invalid transaction nonce/i.test(raw)) {
    return 'The relay is re-syncing its tx sequence — try again in a few seconds.';
  }
  if (/could not coalesce error/i.test(raw)) {
    return 'RPC hiccup — the tx may not have gone through. Try again in a few seconds.';
  }
  return raw;
}
const shortAddr = (a) => (a ? `${a.slice(0, 6)}…${a.slice(-4)}` : '—');

// ── tx inspector ("Under the hood") ─────────────────────────────────────────
// After every confirmed quest tx, show the SAME action on both layers:
//  EVM side   — locally available (signed tx + receipt), calldata/logs decoded
//               against the ABI families the explorer uses.
//  Koinos side — resolved from the foundation REST API: the eth blockHash IS the
//               Koinos block id minus its '0x1220' multihash prefix (and the eth
//               blockNumber IS the Koinos height), so one /block fetch lets us
//               find the wrapping call_contract op whose embedded raw tx
//               keccak-matches our eth hash → Koinos tx id, payer, rc_used.

const INSP_FAMILIES = ['pixel', 'v3router', 'v3pool', 'erc20', 'erc721'];
const INSP_IFACES = INSP_FAMILIES.map((f) => [f, new ethers.Interface(ABI_FAMILIES[f])]);

function esc(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// Address → friendly label ('you', config label, or shortened hex).
function addrLabel(a) {
  if (!a) return '—';
  const low = String(a).toLowerCase();
  if (account && low === account.toLowerCase()) return 'you';
  return ADDRESS_LABELS[low] || shortAddr(a);
}

// Pretty one value for display: labeled addresses, 18-dec token amounts, arrays.
function fmtVal(v) {
  if (Array.isArray(v)) {
    const head = v.slice(0, 6).map(fmtVal).join(', ');
    return v.length > 6 ? `[${head}, … ${v.length} total]` : `[${head}]`;
  }
  if (typeof v === 'bigint') {
    // Heuristic: quest tokens are 18-decimals; big values read better as units.
    if (v >= 10n ** 15n) return `${fmt18(v)} ×10¹⁸`;
    return v.toString();
  }
  if (typeof v === 'string' && /^0x[0-9a-fA-F]{40}$/.test(v)) {
    const lab = addrLabel(v);
    return lab === shortAddr(v) ? lab : `${lab} (${shortAddr(v)})`;
  }
  return String(v);
}
function fmt18(v) {
  const s = ethers.formatUnits(v, 18);
  return s.endsWith('.0') ? s.slice(0, -2) : s;
}

function decodeCall(data) {
  if (!data || data === '0x') return null;
  for (const [family, iface] of INSP_IFACES) {
    try {
      const d = iface.parseTransaction({ data });
      if (d) return { family, name: d.name, fragment: d.fragment, args: d.args };
    } catch {}
  }
  return null;
}

function decodeLogLine(log) {
  for (const [, iface] of INSP_IFACES) {
    try {
      const d = iface.parseLog({ topics: [...log.topics], data: log.data });
      if (d) {
        const parts = d.fragment.inputs.map((inp, i) => `${inp.name || '#' + i}: ${fmtVal(d.args[i])}`);
        return `${d.name}(${parts.join(', ')})`;
      }
    } catch {}
  }
  return `unknown event ${log.topics[0]?.slice(0, 10) ?? ''}`;
}

// base64url (with or without padding) → bytes.
function b64uToBytes(s) {
  const std = s.replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(std);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

// Minimal protobuf walk: return the bytes of field 1 (wire type 2).
function protoField1Bytes(buf) {
  let i = 0;
  const varint = () => {
    let n = 0, shift = 0;
    for (;;) {
      const b = buf[i++];
      n += (b & 0x7f) * 2 ** shift;
      if (!(b & 0x80)) return n;
      shift += 7;
    }
  };
  while (i < buf.length) {
    const key = varint();
    const field = key >> 3, wt = key & 7;
    if (wt === 2) {
      const len = varint();
      if (field === 1) return buf.subarray(i, i + len);
      i += len;
    } else if (wt === 0) varint();
    else if (wt === 5) i += 4;
    else if (wt === 1) i += 8;
    else return null;
  }
  return null;
}

// Resolve the Koinos-side record for a confirmed EVM tx.
async function resolveKoinos(ethHash, blockNumber) {
  const res = await fetch(`${EXPLORER.koinosRestUrl}/block/${blockNumber}?return_receipt=true`);
  if (!res.ok) throw new Error(`Koinos REST /block/${blockNumber}: HTTP ${res.status}`);
  const blk = await res.json();
  const txs = blk?.block?.transactions || [];
  for (const t of txs) {
    for (const op of t.operations || []) {
      const cc = op.call_contract;
      if (!cc || Number(cc.entry_point) !== EXPLORER.submitRawTxEntryPoint) continue;
      if (cc.contract_id && cc.contract_id !== EXPLORER.engineAddress) continue;
      let raw;
      try { raw = protoField1Bytes(b64uToBytes(cc.args)); } catch { continue; }
      if (!raw) continue;
      if (ethers.keccak256(raw).toLowerCase() !== ethHash.toLowerCase()) continue;
      const rcpts = blk?.receipt?.transaction_receipts || [];
      const rec = rcpts.find((r) => r.id === t.id) || null;
      return {
        koinosTxId: t.id,
        payer: t.header?.payer || '—',
        rcLimit: t.header?.rc_limit || null,
        rcUsed: rec?.rc_used || null,
        compute: rec?.compute_bandwidth_used || null,
        network: rec?.network_bandwidth_used || null,
        disk: rec?.disk_storage_used || null,
        blockId: blk.block_id,
        height: blk.block_height,
      };
    }
  }
  throw new Error('wrapping Koinos tx not found in block (REST lag — reopen this entry in a moment)');
}

const inspRecords = []; // {label, ethHash, from, to, data, rcpt, koinos|null, koinosErr|null}
let inspSelected = -1;

function vKoin(rc) {
  return (Number(rc) / 1e8).toFixed(3);
}

function renderInspector() {
  if (!inspRecords.length) return;
  $('inspectorCard').classList.remove('hidden');
  const chips = $('inspChips');
  chips.innerHTML = '';
  inspRecords.forEach((r, i) => {
    const b = document.createElement('button');
    b.className = 'insp-chip' + (i === inspSelected ? ' sel' : '');
    b.textContent = r.label;
    b.addEventListener('click', () => { inspSelected = i; renderInspector(); });
    chips.appendChild(b);
  });

  const r = inspRecords[inspSelected];
  const dec = decodeCall(r.data);
  const callHtml = dec
    ? `<div class="insp-call"><span class="fn">${esc(dec.name)}</span>(\n${dec.fragment.inputs
        .map((inp, i) => `  ${esc(inp.name || '#' + i)}: ${esc(fmtVal(dec.args[i]))}`)
        .join('\n')}\n)</div>`
    : `<div class="insp-call">raw calldata ${esc((r.data || '0x').slice(0, 26))}…</div>`;
  const logsHtml = r.rcpt.logs.length
    ? r.rcpt.logs.map((l) => `<div class="insp-row"><span class="k">event</span><span class="v">${esc(decodeLogLine(l))}</span></div>`).join('')
    : '';

  $('inspEvm').innerHTML = `
    <div class="insp-row"><span class="k">tx hash</span><span class="v">${esc(r.ethHash)}</span></div>
    <div class="insp-row"><span class="k">from → to</span><span class="v"><span class="lbl">${esc(addrLabel(r.from))}</span> → <span class="lbl">${esc(addrLabel(r.to))}</span></span></div>
    ${callHtml}
    <div class="insp-row"><span class="k">status</span><span class="v">${r.rcpt.status === 1 ? '✓ success' : '✗ reverted'} · ${Number(r.rcpt.gasUsed).toLocaleString()} EVM gas</span></div>
    <div class="insp-row"><span class="k">your cost</span><span class="v">gas price 0 → <b>0 of anything</b></span></div>
    ${logsHtml}`;

  const k = r.koinos;
  $('inspKoinos').innerHTML = r.koinosErr
    ? `<div class="insp-row"><span class="k">error</span><span class="v">${esc(r.koinosErr)}</span></div>`
    : !k
      ? '<div class="insp-pending">resolving from Koinos REST…</div>'
      : `
    <div class="insp-row"><span class="k">koinos tx</span><span class="v">${esc(k.koinosTxId)}<br/>
      <a href="${esc(EXPLORER.koinosTxViewUrl(k.koinosTxId))}" target="_blank" rel="noopener noreferrer">view Koinos tx ↗</a> ·
      <a href="${esc(EXPLORER.koinosTxJsonUrl(k.koinosTxId))}" target="_blank" rel="noopener noreferrer">raw JSON ↗</a></span></div>
    <div class="insp-row"><span class="k">operation</span><span class="v">call_contract → <span class="lbl">engine ${esc(EXPLORER.engineAddress.slice(0, 7))}…</span> entry_point ${EXPLORER.submitRawTxEntryPoint} (submit_raw_tx, your RLP bytes inside)</span></div>
    <div class="insp-row"><span class="k">payer</span><span class="v">${esc(k.payer)} <span class="lbl">(relay operator — paid for you)</span></span></div>
    <div class="insp-row"><span class="k">mana</span><span class="v">${k.rcUsed ? `${Number(k.rcUsed).toLocaleString()} rc = <b>${vKoin(k.rcUsed)} vKOIN</b>` : '—'}${k.rcLimit ? ` (limit ${vKoin(k.rcLimit)})` : ''} · regenerates, never bought</span></div>
    <div class="insp-row"><span class="k">resources</span><span class="v">compute ${Number(k.compute || 0).toLocaleString()} · network ${Number(k.network || 0).toLocaleString()} · disk ${Number(k.disk || 0).toLocaleString()}</span></div>
    <div class="insp-row"><span class="k">block</span><span class="v">height ${esc(k.height)} (= EVM blockNumber) · ${esc(String(k.blockId).slice(0, 18))}…</span></div>`;
}

// Record + render a confirmed quest tx, then resolve its Koinos half async.
function inspectTx(label, tx, rcpt) {
  const rec = {
    label,
    ethHash: rcpt.hash || tx.hash,
    from: tx.from,
    to: tx.to,
    data: tx.data,
    rcpt,
    koinos: null,
    koinosErr: null,
  };
  inspRecords.push(rec);
  if (inspRecords.length > 8) inspRecords.shift();
  inspSelected = inspRecords.length - 1;
  renderInspector();
  resolveKoinos(rec.ethHash, rcpt.blockNumber)
    .then((k) => { rec.koinos = k; })
    .catch((e) => { rec.koinosErr = errMessage(e); })
    .finally(() => renderInspector());
}

// ── quest step state ─────────────────────────────────────────────────────────
const done = [false, false, false, false];

function setStep(i, state) {
  // state: 'locked' | 'active' | 'done'
  const li = els.steps[i];
  li.classList.remove('locked', 'active', 'done');
  li.classList.add(state);
  li.querySelector('.step-state').textContent = state === 'done' ? '✓' : '○';
  if (state === 'done') done[i] = true;
}

function refreshStepUi() {
  if (done.every(Boolean)) els.questDone.classList.remove('hidden');
}

const swapCacheKey = () => `koinosQuest.swap.${CHAIN.chainIdNum}.${account?.toLowerCase()}`;

// ── connect ──────────────────────────────────────────────────────────────────
async function ensureNetwork() {
  const eth = window.ethereum;
  try {
    await eth.request({ method: 'wallet_switchEthereumChain', params: [{ chainId: CHAIN.chainIdHex }] });
  } catch (err) {
    const code = err?.code ?? err?.data?.originalError?.code;
    if (code === 4902) {
      await eth.request({
        method: 'wallet_addEthereumChain',
        params: [{
          chainId: CHAIN.chainIdHex,
          chainName: CHAIN.chainName,
          rpcUrls: [CHAIN.rpcUrl],
          nativeCurrency: CHAIN.nativeCurrency,
        }],
      });
      await eth.request({ method: 'wallet_switchEthereumChain', params: [{ chainId: CHAIN.chainIdHex }] });
    } else {
      throw err;
    }
  }
}

async function connect() {
  if (!window.ethereum) {
    toast('MetaMask not detected — install the extension to start the quest.', 'err');
    return;
  }
  els.connectBtn.disabled = true;
  try {
    provider = new ethers.BrowserProvider(window.ethereum);
    await provider.send('eth_requestAccounts', []);
    await ensureNetwork();
    provider = new ethers.BrowserProvider(window.ethereum);
    signer = await provider.getSigner();
    account = await signer.getAddress();

    els.connectBtn.textContent = shortAddr(account);
    setStep(0, 'done');
    toast('Connected — chain 42069 added. Verifying your quest progress on-chain…', 'ok');
    window.ethereum.on?.('accountsChanged', () => location.reload());
    window.ethereum.on?.('chainChanged', () => location.reload());

    await verifyProgress();
  } catch (err) {
    toast(errMessage(err), 'err');
    els.connectBtn.disabled = false;
  }
}

// Re-derive steps 2–4 from chain state so a returning visitor resumes where they left off.
async function verifyProgress() {
  const tfa = new ethers.Contract(TFA.address, ERC20_ABI, readProvider);
  const tfb = new ethers.Contract(TFB.address, ERC20_ABI, readProvider);

  const [balA, balB, mine] = await Promise.all([
    tfa.balanceOf(account),
    tfb.balanceOf(account),
    pixelRead.pixelsBy(account),
  ]);
  els.tfaBal.textContent = ethers.formatUnits(balA, TFA.decimals).replace(/\.?0+$/, '') || '0';
  els.tfbBal.textContent = ethers.formatUnits(balB, TFB.decimals).replace(/\.?0+$/, '') || '0';
  els.statMine.textContent = mine.toString();

  // Step 2: any TFA at all counts (they may have spent some swapping).
  if (balA > 0n || balB > 0n) setStep(1, 'done');
  else { setStep(1, 'active'); els.mintBtn.disabled = false; }
  els.mintBtn.disabled = false; // re-mint always allowed (open faucet)

  // Step 3: in-session/cached receipt, else scan recent pool Swap events for us.
  if (localStorage.getItem(swapCacheKey())) {
    setStep(2, 'done');
    els.swapInfo.textContent = '(already swapped ✓)';
  } else {
    const hash = await findRecentSwap(account).catch(() => null);
    if (hash) {
      localStorage.setItem(swapCacheKey(), hash);
      setStep(2, 'done');
      els.swapInfo.textContent = '(swap found on-chain ✓)';
    } else if (done[1]) {
      setStep(2, 'active');
    }
  }
  els.swapBtn.disabled = !(balA >= SWAP_AMOUNT);
  if (balA < SWAP_AMOUNT && !done[2]) els.swapInfo.textContent = '(mint TFA first)';

  // Step 4: pixelsBy is the on-chain proof.
  if (mine > 0n) setStep(3, 'done');
  else if (done[2]) setStep(3, 'active');

  refreshStepUi();
}

// Scan the last ~3 eth_getLogs windows (proxy caps ranges at 10k blocks) for a
// V3 pool Swap whose indexed recipient is `addr`.
async function findRecentSwap(addr) {
  const swapTopic = ethers.id('Swap(address,address,int256,int256,uint160,uint128,int24)');
  const recipient = ethers.zeroPadValue(addr, 32);
  const latest = await readProvider.getBlockNumber();
  for (let i = 0; i < 3; i++) {
    const to = latest - i * 9900;
    if (to <= 0) break;
    const from = Math.max(0, to - 9899);
    const logs = await readProvider.send('eth_getLogs', [{
      address: V3.pool,
      fromBlock: '0x' + from.toString(16),
      toBlock: '0x' + to.toString(16),
      topics: [swapTopic, null, recipient],
    }]);
    if (logs.length) return logs[0].transactionHash;
  }
  return null;
}

// ── step 2: faucet mint ──────────────────────────────────────────────────────
async function mint() {
  els.mintBtn.disabled = true;
  try {
    toast('Minting 1,000 TFA… confirm in MetaMask (no gas — the relay pays).', 'pending');
    const c = new ethers.Contract(TFA.address, ERC20_ABI, signer);
    const tx = await c.mint(account, MINT_AMOUNT, { gasLimit: GAS.mint });
    const rcpt = await tx.wait(1, TX_TIMEOUT_MS);
    toastConfirmed('Minted 1,000 TFA', rcpt.blockNumber);
    inspectTx('② mint', tx, rcpt);
    setStep(1, 'done');
    await verifyProgress();
  } catch (err) {
    toast(errMessage(err), 'err');
  } finally {
    els.mintBtn.disabled = false;
    refreshStepUi();
  }
}

// ── step 3: V3 swap ──────────────────────────────────────────────────────────
async function swap() {
  els.swapBtn.disabled = true;
  try {
    const tfa = new ethers.Contract(TFA.address, ERC20_ABI, signer);
    const allowance = await tfa.allowance(account, V3.swapRouter);
    if (allowance < SWAP_AMOUNT) {
      toast('Step 1/2 — approve the SwapRouter to spend TFA…', 'pending');
      const txA = await tfa.approve(V3.swapRouter, ethers.MaxUint256, { gasLimit: V3_GAS.approve });
      const rA = await txA.wait(1, TX_TIMEOUT_MS);
      inspectTx('③ approve', txA, rA);
    }
    toast('Step 2/2 — swapping 10 TFA → TFB on Uniswap V3…', 'pending');
    const router = new ethers.Contract(V3.swapRouter, V3ROUTER_ABI, signer);
    // amountOutMinimum 0: testnet quest with an open-mint token — slippage is theater here.
    const tx = await router.exactInputSingle({
      tokenIn: TFA.address,
      tokenOut: TFB.address,
      fee: V3.fee,
      recipient: account,
      deadline: Math.floor(Date.now() / 1000) + 1200,
      amountIn: SWAP_AMOUNT,
      amountOutMinimum: 0n,
      sqrtPriceLimitX96: 0n,
    }, { gasLimit: V3_GAS.swap });
    const rcpt = await tx.wait(1, TX_TIMEOUT_MS);
    if (rcpt.status !== 1) throw new Error('swap reverted');
    localStorage.setItem(swapCacheKey(), rcpt.hash);
    toastConfirmed('Swapped on real Uniswap V3', rcpt.blockNumber);
    inspectTx('③ swap', tx, rcpt);
    setStep(2, 'done');
    els.swapInfo.textContent = '(swapped ✓)';
    if (!done[3]) setStep(3, 'active');
    await verifyProgress();
  } catch (err) {
    toast(errMessage(err), 'err');
    els.swapBtn.disabled = false;
  } finally {
    refreshStepUi();
  }
}

// ── canvas state + rendering ────────────────────────────────────────────────
const base = new Uint8Array(W * W);     // confirmed on-chain colors
const pending = new Map();              // pos -> color, not yet submitted
let selectedColor = 5;                  // default: red

function render() {
  for (let p = 0; p < base.length; p++) {
    ctx.fillStyle = PIXEL.palette[base[p]];
    ctx.fillRect((p % W) * CELL, Math.floor(p / W) * CELL, CELL, CELL);
  }
  for (const [p, c] of pending) {
    ctx.fillStyle = PIXEL.palette[c];
    ctx.fillRect((p % W) * CELL, Math.floor(p / W) * CELL, CELL, CELL);
    ctx.strokeStyle = 'rgba(91,140,255,.9)'; // pending = uncommitted marker
    ctx.strokeRect((p % W) * CELL + 0.5, Math.floor(p / W) * CELL + 0.5, CELL - 1, CELL - 1);
  }
}

function refreshPaintUi() {
  const n = pending.size;
  const k = n ? chunkPending().length : 0;
  els.paintBtn.textContent = !n
    ? 'Paint'
    : k > 1
      ? `Paint ${n} pixels (${k} txs)`
      : `Paint ${n} pixel${n > 1 ? 's' : ''}`;
  els.paintBtn.disabled = n === 0 || !signer;
  els.clearBtn.disabled = n === 0;
  els.pendingInfo.textContent = !signer
    ? 'Connect MetaMask to paint (viewing is live for everyone)'
    : !n
      ? 'Pick a color, then click or drag on the canvas'
      : k > 1
        ? `${n}/${PIXEL.maxBatch} pending — scattered pixels split into ${k} txs (clusters are cheaper)`
        : `${n}/${PIXEL.maxBatch} pending — one tx paints them all`;
}

function paletteUi() {
  PIXEL.palette.forEach((hex, i) => {
    const b = document.createElement('button');
    b.className = 'swatch' + (i === selectedColor ? ' sel' : '');
    b.style.background = hex;
    b.title = `color ${i}`;
    b.addEventListener('click', () => {
      selectedColor = i;
      els.palette.querySelectorAll('.swatch').forEach((s, j) => s.classList.toggle('sel', j === i));
    });
    els.palette.appendChild(b);
  });
}

// pointer → grid cell painting (click or drag)
let painting = false;
function cellFromEvent(ev) {
  const r = canvasEl.getBoundingClientRect();
  const x = Math.floor(((ev.clientX - r.left) / r.width) * W);
  const y = Math.floor(((ev.clientY - r.top) / r.height) * W);
  if (x < 0 || y < 0 || x >= W || y >= W) return -1;
  return y * W + x;
}
function paintCell(ev) {
  const pos = cellFromEvent(ev);
  if (pos < 0) return;
  if (!pending.has(pos) && pending.size >= PIXEL.maxBatch) {
    toast(`Batch cap is ${PIXEL.maxBatch} pixels per tx — paint these first, then keep going.`, 'err');
    return;
  }
  pending.set(pos, selectedColor);
  render();
  refreshPaintUi();
}
canvasEl.addEventListener('pointerdown', (ev) => {
  painting = true;
  try { canvasEl.setPointerCapture(ev.pointerId); } catch {} // synthetic events have no active pointer
  paintCell(ev);
});
canvasEl.addEventListener('pointermove', (ev) => { if (painting) paintCell(ev); });
canvasEl.addEventListener('pointerup', () => { painting = false; });
canvasEl.addEventListener('pointercancel', () => { painting = false; });

// ── step 4: submit pixels ────────────────────────────────────────────────────
// Gas estimate for one chunk: word-aware (see PIXEL_GAS in config.js).
function estimateGas(nPixels, nWords) {
  return PIXEL_GAS.base + PIXEL_GAS.perPixel * BigInt(nPixels) + PIXEL_GAS.perWord * BigInt(nWords);
}

// Split the pending set into chunks that each fit the per-tx gas budget.
// Sorting by position groups same-word pixels, which BOTH minimizes chunk
// count and matches the contract's consecutive-word cache.
function chunkPending() {
  const entries = [...pending.entries()].sort((a, b) => a[0] - b[0]);
  const chunks = [];
  let cur = [];
  let words = new Set();
  for (const [p, c] of entries) {
    const w = p >> 5;
    const nextWords = words.has(w) ? words.size : words.size + 1;
    const fits =
      cur.length < PIXEL.maxBatch &&
      estimateGas(cur.length + 1, nextWords) <= PIXEL_GAS.budget;
    if (cur.length && !fits) {
      chunks.push({ pixels: cur, words: words.size });
      cur = [];
      words = new Set();
    }
    cur.push([p, c]);
    words.add(w);
  }
  if (cur.length) chunks.push({ pixels: cur, words: words.size });
  return chunks;
}

async function paint() {
  if (!signer || pending.size === 0) return;
  els.paintBtn.disabled = true;
  const chunks = chunkPending();
  const total = pending.size;
  let painted = 0;
  try {
    const c = new ethers.Contract(PIXEL.address, PIXEL_ABI, signer);
    for (let i = 0; i < chunks.length; i++) {
      const { pixels, words } = chunks[i];
      const positions = pixels.map(([p]) => p);
      const colors = pixels.map(([, col]) => col);
      toast(
        chunks.length > 1
          ? `Painting ${total} pixels — tx ${i + 1}/${chunks.length} (${positions.length} px)…`
          : `Painting ${positions.length} pixels in one zero-gas tx…`,
        'pending',
      );
      const gasLimit =
        (estimateGas(positions.length, words) * PIXEL_GAS.marginNum) / PIXEL_GAS.marginDen;
      const tx = await c.setPixels(positions, colors, { gasLimit });
      const rcpt = await tx.wait(1, TX_TIMEOUT_MS);
      if (rcpt.status !== 1) throw new Error('setPixels reverted');
      inspectTx(chunks.length > 1 ? `④ paint ${i + 1}/${chunks.length}` : '④ paint', tx, rcpt);
      // Settle this chunk locally; keep the rest pending so a mid-run failure
      // loses nothing (the WS event will confirm — it's idempotent).
      for (const [p, col] of pixels) {
        base[p] = col;
        pending.delete(p);
      }
      painted += positions.length;
      render();
      if (i === chunks.length - 1) toastConfirmed(`Painted ${painted} pixels`, rcpt.blockNumber);
    }
    setStep(3, 'done');
    refreshStats();
  } catch (err) {
    const msg = errMessage(err);
    toast(
      painted > 0
        ? `Painted ${painted}/${total} — the rest stayed selected. ${msg}`
        : msg,
      'err',
    );
    if (painted > 0) {
      setStep(3, 'done');
      refreshStats();
    }
  } finally {
    refreshPaintUi();
    refreshStepUi();
  }
}

function clearPending() {
  pending.clear();
  render();
  refreshPaintUi();
}

// ── chain reads: canvas + stats ──────────────────────────────────────────────
async function loadCanvas() {
  try {
    const bytes = ethers.getBytes(await pixelRead.getCanvas());
    base.set(bytes.subarray(0, W * W));
  } catch {
    // A default public Koinos node caps read compute (-32005), which kills the
    // getCanvas() view. The proxy serves eth_getLogs from its OWN index with no
    // node compute, so replay every PixelsSet since deploy — same end state
    // (logs arrive ordered by block/logIndex; last write wins).
    await loadCanvasFromLogs();
  }
  render();
}

async function loadCanvasFromLogs() {
  const latest = await readProvider.getBlockNumber();
  const topic = pixelIface.getEvent('PixelsSet').topicHash;
  for (let from = PIXEL.deployBlock; from <= latest; from += 9900) {
    const to = Math.min(from + 9899, latest);
    const logs = await readProvider.send('eth_getLogs', [{
      address: PIXEL.address,
      topics: [topic],
      fromBlock: '0x' + from.toString(16),
      toBlock: '0x' + to.toString(16),
    }]);
    for (const lg of logs) {
      try {
        const p = pixelIface.parseLog({ topics: lg.topics, data: lg.data });
        p.args[1].forEach((pos, j) => { base[Number(pos)] = Number(p.args[2][j]); });
      } catch {}
    }
  }
}

async function refreshStats() {
  const [total, mine] = await Promise.all([
    pixelRead.totalPixels(),
    account ? pixelRead.pixelsBy(account) : Promise.resolve(null),
  ]);
  els.statTotal.textContent = total.toString();
  if (mine !== null) els.statMine.textContent = mine.toString();
}

// ── live feed: WebSocket eth_subscribe("logs"), polling fallback ────────────
let ws = null;
let pollTimer = null;

function setLive(on, label) {
  els.liveDot.classList.toggle('off', !on);
  els.liveLabel.textContent = label;
}

function startPolling() {
  if (pollTimer) return;
  pollTimer = setInterval(() => { loadCanvas().catch(() => {}); refreshStats().catch(() => {}); }, 9000);
}
function stopPolling() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

function addActivity(artist, positions, colors, blockNumber) {
  if (els.activity.firstElementChild?.classList.contains('hint')) els.activity.innerHTML = '';
  const li = document.createElement('li');
  const who = document.createElement('code');
  who.textContent = artist && account && artist.toLowerCase() === account.toLowerCase()
    ? 'you' : shortAddr(artist);
  li.appendChild(who);
  li.appendChild(document.createTextNode(` painted ${positions.length} px · block ${blockNumber}`));
  const chips = document.createElement('span');
  chips.className = 'chips';
  colors.slice(0, 8).forEach((c) => {
    const chip = document.createElement('span');
    chip.className = 'chip';
    chip.style.background = PIXEL.palette[Number(c)];
    chips.appendChild(chip);
  });
  li.appendChild(chips);
  els.activity.prepend(li);
  while (els.activity.children.length > 12) els.activity.lastChild.remove();
}

function applyPixelsSetLog(log) {
  let parsed;
  try {
    parsed = pixelIface.parseLog({ topics: log.topics, data: log.data });
  } catch { return; }
  if (parsed?.name !== 'PixelsSet') return;
  const [artist, positions, colors] = parsed.args;
  positions.forEach((p, k) => { base[Number(p)] = Number(colors[k]); });
  render();
  const bn = typeof log.blockNumber === 'string' ? parseInt(log.blockNumber, 16) : log.blockNumber;
  addActivity(artist, positions, colors, bn);
  refreshStats().catch(() => {});
}

function connectWs() {
  const wsUrl = CHAIN.rpcUrl.replace(/^http/, 'ws');
  try { ws = new WebSocket(wsUrl); } catch { setLive(false, 'polling'); startPolling(); return; }

  ws.onopen = () => {
    ws.send(JSON.stringify({
      jsonrpc: '2.0', id: 1, method: 'eth_subscribe',
      params: ['logs', { address: PIXEL.address }],
    }));
    setLive(true, 'live');
    stopPolling();
  };
  ws.onmessage = (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    if (msg.method === 'eth_subscription' && msg.params?.result) applyPixelsSetLog(msg.params.result);
  };
  ws.onclose = () => {
    setLive(false, 'polling (ws lost — retrying)');
    startPolling();
    setTimeout(connectWs, 5000);
  };
  ws.onerror = () => { try { ws.close(); } catch {} };
}

// ── boot ─────────────────────────────────────────────────────────────────────
paletteUi();
render();
refreshPaintUi();
setStep(0, 'active');
$('rpcUrlFoot').textContent = CHAIN.rpcUrl;
els.connectBtn.addEventListener('click', connect);
els.mintBtn.addEventListener('click', mint);
els.swapBtn.addEventListener('click', swap);
els.paintBtn.addEventListener('click', paint);
els.clearBtn.addEventListener('click', clearPending);

loadCanvas().then(refreshStats).catch((err) => toast(errMessage(err), 'err'));
connectWs();
