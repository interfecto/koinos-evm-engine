// Koinos EVM Explorer — pure client-side.
//
// FEED SOURCES (EXPLORER.dataSource in config.js):
//
//  'eth' (default) — the proxy's standard Ethereum JSON-RPC index. The head
//   comes from eth_blockNumber; the newest few blocks are walked with
//   eth_getBlockByNumber(h, true) + eth_getBlockReceipts(h) (catches txs that
//   emit no logs), and older activity is located via chunked eth_getLogs
//   windows — blocks are ~3s and almost all empty, so we never fetch
//   thousands of empty blocks. Falls back at runtime to:
//
//  'account_history' — every EVM tx on this chain is relayed as a Koinos
//   `call_contract` op to the engine contract (entry_point 7 = submit_raw_tx)
//   whose args carry the signed Ethereum RLP tx. So the engine contract's
//   account_history IS the EVM tx feed: one RPC call returns each tx + its
//   INLINE receipt (evm.log / evm.result events). We decode the raw tx with
//   ethers (recovering `from`) and the protobuf events by hand. The foundation
//   testnet sends `access-control-allow-origin: *`, so the browser reads it
//   directly — no proxy involved.
//
// BOTH sources produce the same normalized record and feed the same rich
// decoding: calldata + logs are ABI-decoded against the registry in config.js.

import {
  Transaction,
  Interface,
  keccak256,
} from 'https://esm.sh/ethers@6.13.4';

import { EXPLORER, ADDRESS_LABELS, ABI_FAMILIES, ADDRESS_FAMILIES } from './config.js';

// ── Build one ethers Interface per ABI family. Decoding tries families in a
//    fixed order and falls through on mismatch, which resolves selector/topic
//    collisions across families (e.g. ERC-20 vs ERC-721 `Transfer`). ──────────
const IFACES = {};
for (const [fam, frags] of Object.entries(ABI_FAMILIES)) {
  try { IFACES[fam] = new Interface(frags); }
  catch (e) { console.error(`bad ABI family ${fam}:`, e); }
}
// Calldata: specific protocols first, generic token ABIs last.
const CALL_ORDER = ['pixel', 'v3router', 'nfpm', 'quoter', 'v2router', 'v3pool', 'v2pair', 'factory', 'helpers', 'erc20', 'erc721'];
// Logs: event-bearing protocols first, then generic.
const LOG_ORDER = ['pixel', 'v3pool', 'v2pair', 'nfpm', 'factory', 'erc20', 'erc721'];

// ── Byte / base64 / protobuf helpers ─────────────────────────────────────────
function b64uToBytes(s) {
  if (!s) return new Uint8Array(0);
  let t = s.replace(/-/g, '+').replace(/_/g, '/');
  while (t.length % 4) t += '=';
  const bin = atob(t);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
function toHex(bytes) {
  let h = '0x';
  for (const b of bytes) h += b.toString(16).padStart(2, '0');
  return h;
}

// Iterate protobuf fields. Yields { field, wtype, varint (BigInt), payload (Uint8Array) }.
function* protoFields(buf) {
  let pos = 0;
  const n = buf.length;
  while (pos < n) {
    // tag (varint, small — fits a JS number). Accumulate with multiplication so a
    // high bit can't flip `tag` negative (signed `<<`); bail on absurd width.
    let tag = 0, shift = 0;
    while (true) {
      if (pos >= n) return;
      const b = buf[pos++];
      tag += (b & 0x7f) * 2 ** shift;
      if (!(b & 0x80)) break;
      shift += 7;
      if (shift > 35) return; // tag too large — bail defensively
    }
    const field = Math.floor(tag / 8);
    const wtype = tag % 8;
    if (wtype === 0) {
      let val = 0n, sh = 0n;
      while (true) {
        if (pos >= n) return;
        const b = buf[pos++];
        val |= BigInt(b & 0x7f) << sh;
        if (!(b & 0x80)) break;
        sh += 7n;
      }
      yield { field, wtype, varint: val };
    } else if (wtype === 2) {
      // Accumulate the length with multiplication (exact to 2^53), NOT signed `<<`
      // — a `<<` on a length with bit 31 set goes negative and can spin pos backwards.
      let len = 0, sh = 0;
      while (true) {
        if (pos >= n) return;
        const b = buf[pos++];
        len += (b & 0x7f) * 2 ** sh;
        if (!(b & 0x80)) break;
        sh += 7;
        if (sh > 49) return; // absurd length — bail
      }
      if (len < 0 || pos + len > n) return; // truncated / malformed
      const payload = buf.subarray(pos, pos + len);
      pos += len;
      yield { field, wtype, payload };
    } else if (wtype === 5) { pos += 4; }
    else if (wtype === 1) { pos += 8; }
    else { return; } // groups / unknown — stop
  }
}

// ── Koinos RPC / REST ─────────────────────────────────────────────────────────
async function koinosRpc(method, params) {
  const res = await fetch(EXPLORER.koinosRpcUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  const j = await res.json();
  if (j.error) throw new Error(j.error.message || JSON.stringify(j.error));
  return j.result;
}
async function koinosRest(path) {
  const res = await fetch(EXPLORER.koinosRestUrl + path);
  if (!res.ok) throw new Error(`REST ${path} → ${res.status}`);
  return res.json();
}

// account_history of the engine contract = the EVM tx feed.
// seqNum (optional) → start there going down (ascending:false).
async function fetchHistory(limit, seqNum) {
  const params = { address: EXPLORER.engineAddress, limit, ascending: false };
  if (seqNum != null) params.seq_num = String(seqNum);
  const r = await koinosRpc('account_history.get_account_history', params);
  return (r && r.values) || [];
}

// ── Ethereum JSON-RPC (the proxy's durable index) ─────────────────────────────
async function ethRpc(method, params) {
  const res = await fetch(EXPLORER.ethRpcUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  const j = await res.json();
  if (j.error) throw new Error(`${method}: ${j.error.message || JSON.stringify(j.error)}`);
  return j.result;
}
const hexNum = (h) => parseInt(h, 16);
const hexTag = (n) => '0x' + n.toString(16);

// ── Address labelling ───────────────────────────────────────────────────────
function short(addr) {
  if (!addr) return '—';
  return addr.slice(0, 6) + '…' + addr.slice(-4);
}
function labelOf(addr) {
  if (!addr) return null;
  return ADDRESS_LABELS[addr.toLowerCase()] || null;
}
// Returns safe HTML for an address with optional label tag.
function addrHtml(addr) {
  if (!addr) return '<span class="muted">—</span>';
  const lab = labelOf(addr);
  const lo = addr.toLowerCase();
  if (lab) return `<span class="addr" title="${esc(addr)}"><span class="tag">${esc(lab)}</span></span>`;
  return `<span class="addr mono" title="${esc(addr)}">${esc(short(lo))}</span>`;
}

// ── Calldata / log decoding via ethers ────────────────────────────────────────
function decodeCalldata(to, data) {
  if (!to) return { kind: 'create' };
  if (!data || data === '0x') return { kind: 'transfer' };
  // Try the `to`-address's known families first (resolves exact-signature collisions
  // like NFPM ERC-721 `approve` vs ERC-20 `approve`), then the global order.
  const hint = ADDRESS_FAMILIES[to.toLowerCase()] || [];
  const order = [...hint, ...CALL_ORDER.filter((f) => !hint.includes(f))];
  for (const fam of order) {
    const iface = IFACES[fam];
    if (!iface) continue;
    try {
      const d = iface.parseTransaction({ data });
      if (d) return { kind: 'call', family: fam, name: d.name, fragment: d.fragment, args: d.args, signature: d.signature };
    } catch (_) { /* selector mismatch / decode error → next family */ }
  }
  return { kind: 'unknown', selector: data.slice(0, 10) };
}
function decodeLog(topics, data) {
  if (!topics || !topics.length) return null;
  for (const fam of LOG_ORDER) {
    const iface = IFACES[fam];
    if (!iface) continue;
    try {
      const d = iface.parseLog({ topics, data });
      if (d) return { family: fam, name: d.name, fragment: d.fragment, args: d.args, signature: d.signature };
    } catch (_) { /* topic mismatch / wrong indexed count → next family */ }
  }
  return null;
}

// Pretty-print a decoded arg value against its ParamType (recursive).
function fmtArg(value, pt) {
  try {
    if (pt.baseType === 'array' && pt.arrayChildren) {
      return '[' + Array.from(value).map((v) => fmtArg(v, pt.arrayChildren)).join(', ') + ']';
    }
    if (pt.baseType === 'tuple' && pt.components) {
      return '{ ' + pt.components.map((c, i) => `${c.name || i}: ${fmtArg(value[i], c)}`).join(', ') + ' }';
    }
    const t = pt.type || '';
    if (t === 'address') {
      const lab = labelOf(value);
      return lab ? `${value} (${lab})` : value;
    }
    if (t === 'bool') return String(value);
    if (t.startsWith('uint') || t.startsWith('int')) return value.toString();
    return String(value); // bytes / string
  } catch (_) {
    return String(value);
  }
}
function argLines(fragment, args) {
  if (!fragment || !fragment.inputs || !fragment.inputs.length) return '(no args)';
  return fragment.inputs.map((inp, i) => `  ${inp.name || `arg${i}`}: ${fmtArg(args[i], inp)}`).join('\n');
}

// ── Decode a single history entry into a normalized record ────────────────────
function decodeEntry(entry) {
  const seq = Number(entry.seq_num);
  const tx = entry.trx && entry.trx.transaction;
  const hasReceipt = !!(entry.trx && entry.trx.receipt);
  const rec = (entry.trx && entry.trx.receipt) || {};
  const out = { seq, koinosTxId: tx && tx.id, kind: 'non-evm' };
  if (!tx) return out;

  // Find the engine call_contract op (don't assume it's operations[0]); require the
  // submit_raw_tx entry point and, when the contract_id is present, the engine address.
  const cc = (tx.operations || [])
    .map((o) => o && o.call_contract)
    .find((c) => c && c.entry_point === EXPLORER.submitRawTxEntryPoint && c.args
      && (!c.contract_id || c.contract_id === EXPLORER.engineAddress));
  if (!cc) return out;

  // args proto { bytes raw_tx = 1 } → raw signed eth tx
  let rawTx = null;
  for (const f of protoFields(b64uToBytes(cc.args))) {
    if (f.field === 1 && f.wtype === 2) rawTx = f.payload;
  }
  if (!rawTx || !rawTx.length) { out.kind = 'undecodable'; return out; }

  let etx;
  try { etx = Transaction.from(toHex(rawTx)); }
  catch (e) { out.kind = 'undecodable'; out.error = String(e && e.message || e); return out; }

  out.kind = 'evm';
  out.from = etx.from;
  out.to = etx.to;            // null for contract creation
  out.value = etx.value;      // BigInt
  out.nonce = etx.nonce;
  out.data = etx.data;
  out.gasLimit = etx.gasLimit;
  out.chainId = etx.chainId;
  out.txType = etx.type;
  out.ethHash = etx.hash;
  out.call = decodeCalldata(etx.to, etx.data);

  // receipt events (inline in account_history). No receipt at all → pending/unknown,
  // NOT a false "success" (matters for the manual-decode box on an un-included tx).
  const ev = decodeReceiptEvents(rec.events || []);
  out.pending = !hasReceipt;
  out.status = hasReceipt ? ev.status : 'unknown';  // '0x1' | '0x0' | 'unknown'
  out.gasUsed = hasReceipt ? ev.gasUsed : null;
  out.createdAddress = ev.createdAddress;
  out.logs = ev.logs;
  out.rcUsed = rec.rc_used;
  return out;
}

function decodeReceiptEvents(events) {
  let status = '0x1', gasUsed = 0n, createdAddress = null;
  let sawResult = false;
  const logs = [];
  for (const e of events) {
    const name = e && e.name;
    if (name !== 'evm.log' && name !== 'evm.result') continue;
    const bytes = b64uToBytes(e.data || '');
    if (name === 'evm.result') {
      sawResult = true;
      for (const f of protoFields(bytes)) {
        if (f.field === 1 && f.wtype === 0) status = f.varint === 0n ? '0x0' : '0x1';
        else if (f.field === 2 && f.wtype === 0) gasUsed = f.varint;
        else if (f.field === 3 && f.wtype === 2 && f.payload.length === 20) createdAddress = toHex(f.payload);
      }
    } else {
      let addr = null; const topics = []; let ldata = '0x';
      for (const f of protoFields(bytes)) {
        if (f.field === 1 && f.wtype === 2 && f.payload.length === 20) addr = toHex(f.payload);
        else if (f.field === 2 && f.wtype === 2) topics.push(toHex(f.payload));
        else if (f.field === 3 && f.wtype === 2) ldata = toHex(f.payload);
      }
      logs.push({ address: addr, topics, data: ldata, decoded: decodeLog(topics, ldata) });
    }
  }
  // No evm.result event but the chain accepted the tx → treat as success unless receipt says reverted.
  if (!sawResult) status = '0x1';
  return { status, gasUsed, createdAddress, logs };
}

// ── eth data source: normalize a tx object (+ optional receipt) into the same
//    record shape decodeEntry produces. `blockTs` is the block's unix seconds. ──
function recFromEthTx(tx, receipt, blockTs) {
  const blockNumber = tx.blockNumber != null ? hexNum(tx.blockNumber) : null;
  const txIndex = tx.transactionIndex != null ? hexNum(tx.transactionIndex) : 0;
  const out = {
    // Sortable, stable key: block-global position (Koinos blocks hold few txs).
    seq: blockNumber != null ? blockNumber * 10000 + txIndex : -1,
    blockNumber,
    txIndex,
    blockTimestampMs: blockTs != null ? blockTs * 1000 : null,
    // The proxy's EVM blockHash is the Koinos block id sans its 0x1220 multihash
    // prefix — re-prefixing gives the REST-addressable Koinos block id.
    koinosBlockId: tx.blockHash ? '0x1220' + tx.blockHash.slice(2) : null,
    koinosTxId: null, // not carried by the eth APIs — resolved lazily on expand
    kind: 'evm',
    from: tx.from,
    to: tx.to || null,        // null for contract creation
    value: BigInt(tx.value || '0x0'),
    nonce: hexNum(tx.nonce || '0x0'),
    data: tx.input || '0x',
    gasLimit: BigInt(tx.gas || '0x0'),
    chainId: tx.chainId != null ? hexNum(tx.chainId) : null,
    txType: tx.type != null ? hexNum(tx.type) : 0,
    ethHash: tx.hash,
    call: decodeCalldata(tx.to || null, tx.input || '0x'),
  };
  if (receipt) {
    out.pending = false;
    out.status = receipt.status === '0x0' ? '0x0' : '0x1';
    out.gasUsed = BigInt(receipt.gasUsed || '0x0');
    out.createdAddress = receipt.contractAddress || null;
    out.logs = (receipt.logs || []).map((l) => ({
      address: l.address,
      topics: l.topics || [],
      data: l.data || '0x',
      decoded: decodeLog(l.topics || [], l.data || '0x'),
    }));
  } else {
    // Mined-but-unindexed or not yet included → pending, NOT a false "success".
    out.pending = true;
    out.status = 'unknown';
    out.gasUsed = null;
    out.createdAddress = null;
    out.logs = [];
  }
  return out;
}

// Fetch one block's records: full tx objects + the whole block's receipts.
// Empty block → [] for the price of a single call (no receipts round-trip).
async function ethBlockRecords(num) {
  const blk = await ethRpc('eth_getBlockByNumber', [hexTag(num), true]);
  if (!blk || !blk.transactions || !blk.transactions.length) return [];
  const ts = blk.timestamp != null ? hexNum(blk.timestamp) : null;
  const receipts = (await ethRpc('eth_getBlockReceipts', [hexTag(num)])) || [];
  const byHash = new Map(receipts.map((r) => [r.transactionHash.toLowerCase(), r]));
  return blk.transactions.map((tx) => recFromEthTx(tx, byHash.get(tx.hash.toLowerCase()) || null, ts));
}

// Fetch many block numbers in bounded-parallel chunks (local proxy, but be polite).
async function ethBlocksRecords(nums) {
  const out = [];
  for (let i = 0; i < nums.length; i += 25) {
    const part = await Promise.all(nums.slice(i, i + 25).map(ethBlockRecords));
    for (const rs of part) out.push(...rs);
  }
  return out;
}

// eth feed state: records newest-first + the next unscanned height. Blocks are
// immutable here (no reorg handling in this PoC), so scanned ranges never repeat.
const ethFeed = { records: [], nextBlock: null, head: null };

// Full scan: walk the newest `ethTailBlocks` directly (catches log-less txs near
// the head), then chunked eth_getLogs windows below that to FIND active blocks
// without touching the thousands of empty ones in between.
async function ethInitialScan(head) {
  const tailFrom = Math.max(0, head - (EXPLORER.ethTailBlocks - 1));
  const tailNums = [];
  for (let b = head; b >= tailFrom; b--) tailNums.push(b);
  const records = await ethBlocksRecords(tailNums);

  const activeBlocks = [];
  const seenTx = new Set(records.map((r) => r.ethHash)); // early-stop heuristic
  let hi = tailFrom - 1;
  for (let w = 0; w < EXPLORER.ethMaxLogWindows && hi >= 0 && seenTx.size < EXPLORER.feedLimit; w++) {
    const lo = Math.max(0, hi - (EXPLORER.ethLogWindow - 1));
    const logs = await ethRpc('eth_getLogs', [{ fromBlock: hexTag(lo), toBlock: hexTag(hi) }]);
    const blocks = new Set();
    for (const l of logs || []) { blocks.add(hexNum(l.blockNumber)); seenTx.add(l.transactionHash); }
    activeBlocks.push(...[...blocks].sort((a, b) => b - a)); // newest first
    hi = lo - 1;
  }
  // Every active block holds ≥1 tx, so fetching `need` of them fills the feed.
  const need = Math.max(0, EXPLORER.feedLimit - records.length);
  records.push(...await ethBlocksRecords(activeBlocks.slice(0, need)));

  records.sort((a, b) => b.seq - a.seq);
  ethFeed.records = records.slice(0, EXPLORER.feedLimit);
  ethFeed.nextBlock = head + 1;
  ethFeed.head = head;
}

// Per-poll refresh: walk only the blocks minted since the last poll (~3 per 9s).
// A large gap (tab slept / first run) triggers a full rescan instead.
async function ethRefresh() {
  const head = hexNum(await ethRpc('eth_blockNumber', []));
  if (ethFeed.nextBlock == null || head - ethFeed.nextBlock > 300 || head < ethFeed.nextBlock - 1) {
    await ethInitialScan(head);
    return ethFeed.records;
  }
  if (head >= ethFeed.nextBlock) {
    const nums = [];
    for (let b = ethFeed.nextBlock; b <= head; b++) nums.push(b);
    const fresh = await ethBlocksRecords(nums);
    if (fresh.length) {
      fresh.sort((a, b) => b.seq - a.seq);
      ethFeed.records = [...fresh, ...ethFeed.records].slice(0, EXPLORER.feedLimit);
    }
    ethFeed.nextBlock = head + 1;
  }
  ethFeed.head = head;
  return ethFeed.records;
}

// ── Rendering ─────────────────────────────────────────────────────────────────
function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}
function fmtEth(weiBig) {
  // 18-decimal format, trimmed, BigInt-safe.
  const neg = weiBig < 0n;
  let v = neg ? -weiBig : weiBig;
  const whole = v / 10n ** 18n;
  const frac = (v % 10n ** 18n).toString().padStart(18, '0').replace(/0+$/, '');
  return (neg ? '-' : '') + whole.toString() + (frac ? '.' + frac : '');
}

function methodPill(rec) {
  const c = rec.call;
  if (!c) return '<span class="method-pill unknown">—</span>';
  if (c.kind === 'create') return '<span class="method-pill create">contract creation</span>';
  if (c.kind === 'transfer') return '<span class="method-pill">tKOIN transfer</span>';
  if (c.kind === 'call') return `<span class="method-pill" title="${esc(c.signature || c.name)}">${esc(c.name)}</span>`;
  return `<span class="method-pill unknown" title="selector ${esc(c.selector || '')}">unknown</span>`;
}

function rowHtml(rec) {
  if (rec.kind !== 'evm') {
    const note = rec.kind === 'undecodable' ? 'undecodable EVM tx' : 'non-EVM op';
    return `<tr class="row" data-seq="${rec.seq}"><td class="mono">${rec.seq}</td>`
      + `<td colspan="5" class="muted">${esc(note)} · <span class="mono">${esc(short(rec.koinosTxId || ''))}</span></td></tr>`;
  }
  const toCell = rec.to
    ? addrHtml(rec.to)
    : (rec.createdAddress ? `<span class="status-ok">⊕ ${esc(short(rec.createdAddress))}</span>` : '<span class="muted">new contract</span>');
  const statusCell = rec.status === '0x0'
    ? '<span class="status-fail">✗ failed</span>'
    : rec.status === 'unknown'
      ? '<span class="muted">… pending</span>'
      : '<span class="status-ok">✓</span>';
  const val = rec.value > 0n ? `<span class="val-num">${esc(fmtEth(rec.value))}</span>` : '<span class="muted">0</span>';
  // eth source → show the (Koinos) block height; account_history → the seq num.
  const seqCell = rec.blockNumber != null
    ? `<td class="mono" title="tx ${rec.txIndex} in block ${rec.blockNumber}">${rec.blockNumber}</td>`
    : `<td class="mono">${rec.seq}</td>`;
  return `<tr class="row" data-seq="${rec.seq}">`
    + seqCell
    + `<td>${addrHtml(rec.from)} <span class="arrow">→</span> ${toCell}</td>`
    + `<td>${methodPill(rec)}</td>`
    + `<td>${val}</td>`
    + `<td>${statusCell}</td>`
    + `<td class="mono" title="${esc(rec.ethHash)}">${esc(short(rec.ethHash))}</td>`
    + `</tr>`;
}

function logHtml(log) {
  const d = log.decoded;
  const head = d
    ? `<span class="ev-pill">${esc(d.name)}</span> <span class="muted mono">${esc(short(log.address))}</span>`
    : `<span class="ev-pill" style="background:rgba(138,150,173,.12);color:var(--muted);border-color:var(--border)">log</span> <span class="muted mono">${esc(short(log.address))}</span>`;
  let body;
  if (d) {
    body = `<div class="codeblock">${esc(argLines(d.fragment, d.args))}</div>`;
  } else {
    const topics = log.topics.map((t, i) => `  topic[${i}]: ${t}`).join('\n');
    body = `<div class="codeblock">${esc(topics + '\n  data: ' + log.data)}</div>`;
  }
  return `<div class="log-item"><div class="log-head">${head}</div>${body}</div>`;
}

function koinosLinksHtml(koinosTxId) {
  if (!koinosTxId) return '';
  return `<a href="${EXPLORER.koinosTxViewUrl(koinosTxId)}" target="_blank" rel="noopener">view Koinos tx ↗</a>`
    + `<a href="${EXPLORER.koinosTxJsonUrl(koinosTxId)}" target="_blank" rel="noopener">raw JSON ↗</a>`;
}

async function detailHtml(rec) {
  // Lazily filled in by fillBlock() (and, for eth records, fillKoinosTxId()).
  let blockLine = '<span class="muted">fetching block…</span>';

  const callBlock = (() => {
    const c = rec.call;
    if (!c) return '';
    if (c.kind === 'call') {
      return `<div><h3>Decoded call</h3><div class="codeblock">${esc(c.signature || c.name)}\n${esc(argLines(c.fragment, c.args))}</div></div>`;
    }
    if (c.kind === 'create') {
      const sz = rec.data ? (rec.data.length - 2) / 2 : 0;
      return `<div><h3>Decoded call</h3><div class="codeblock">contract creation · ${sz} bytes init code${rec.createdAddress ? '\ndeployed → ' + rec.createdAddress : ''}</div></div>`;
    }
    if (c.kind === 'transfer') {
      return `<div><h3>Decoded call</h3><div class="codeblock">native value transfer (no calldata)</div></div>`;
    }
    return `<div><h3>Decoded call</h3><div class="codeblock">unknown selector ${esc(c.selector || '')} (${rec.data ? (rec.data.length - 2) / 2 : 0} bytes calldata)</div></div>`;
  })();

  const logsBlock = rec.logs && rec.logs.length
    ? `<div><h3>Events (${rec.logs.length})</h3>${rec.logs.map(logHtml).join('')}</div>`
    : '<div><h3>Events</h3><div class="muted">none</div></div>';

  const kv = `
    <div class="kv">
      <div class="k">Status</div><div class="v">${rec.status === '0x0' ? '<span class="status-fail">✗ reverted</span>' : rec.status === 'unknown' ? '<span class="muted">… pending / not yet included</span>' : '<span class="status-ok">✓ success</span>'}</div>
      <div class="k">From</div><div class="v mono">${esc(rec.from)}${labelOf(rec.from) ? ' (' + esc(labelOf(rec.from)) + ')' : ''}</div>
      <div class="k">To</div><div class="v mono">${rec.to ? esc(rec.to) + (labelOf(rec.to) ? ' (' + esc(labelOf(rec.to)) + ')' : '') : '<span class="muted">contract creation</span>'}</div>
      ${rec.createdAddress ? `<div class="k">Created</div><div class="v mono">${esc(rec.createdAddress)}</div>` : ''}
      <div class="k">Value</div><div class="v val-num">${esc(fmtEth(rec.value))} tKOIN</div>
      <div class="k">Nonce</div><div class="v">${rec.nonce}</div>
      <div class="k">EVM gas used</div><div class="v val-num">${rec.gasUsed != null ? rec.gasUsed.toString() : '—'}</div>
      <div class="k">Tx type</div><div class="v">${rec.txType === 0 ? 'legacy (EIP-155)' : 'EIP-1559 (type 2)'} · chainId ${rec.chainId}</div>
      <div class="k">EVM tx hash</div><div class="v mono">${esc(rec.ethHash)}</div>
      <div class="k">Koinos tx id</div><div class="v mono" id="ktx-${rec.seq}">${esc(rec.koinosTxId || '—')}</div>
      <div class="k">Block</div><div class="v" id="blk-${rec.seq}">${blockLine}</div>
    </div>`;

  return `<div class="detail">
      ${kv}
      ${callBlock}
      ${logsBlock}
      <div class="links-row" id="links-${rec.seq}">${koinosLinksHtml(rec.koinosTxId)}</div>
    </div>`;
}

// Fill in the block height/time after the detail row is in the DOM.
async function fillBlock(rec) {
  const el = document.getElementById(`blk-${rec.seq}`);
  if (!el) return;
  // eth-source records carry the block inline — no extra hop. Kick off the lazy
  // Koinos-tx-id resolution (best-effort) while we're at it.
  if (rec.blockNumber != null) {
    const when = rec.blockTimestampMs ? new Date(rec.blockTimestampMs).toLocaleString() : '';
    el.innerHTML = `#${esc(rec.blockNumber)} ${when ? '· <span class="muted">' + esc(when) + '</span>' : ''}`;
    fillKoinosTxId(rec);
    return;
  }
  if (!rec.koinosTxId) {
    el.innerHTML = `<span class="muted">${rec.pending ? 'pending / not yet in a block' : '—'}</span>`;
    return;
  }
  try {
    const txr = await koinosRest(`/transaction/${rec.koinosTxId}`);
    const bId = txr.containing_blocks && txr.containing_blocks[0];
    if (!bId) { el.innerHTML = '<span class="muted">pending / not yet in a block</span>'; return; }
    const blk = await koinosRest(`/block/${bId}`);
    const h = blk.block && blk.block.header;
    const height = (h && h.height) || (blk.block_height) || '?';
    const tsMs = h && h.timestamp ? Number(h.timestamp) : 0;
    const when = tsMs ? new Date(tsMs).toLocaleString() : '';
    el.innerHTML = `#${esc(height)} ${when ? '· <span class="muted">' + esc(when) + '</span>' : ''}`;
  } catch (e) {
    el.innerHTML = '<span class="muted">block lookup failed</span>';
  }
}

// The eth APIs don't carry the Koinos tx id. But the EVM blockHash IS the Koinos
// block id (sans 0x1220), so one REST block fetch + matching keccak256(raw_tx)
// against the eth hash recovers it — best-effort, only on expand.
async function fillKoinosTxId(rec) {
  if (!rec.koinosTxId && rec.koinosBlockId && rec.ethHash) {
    try {
      const blk = await koinosRest(`/block/${rec.koinosBlockId}`);
      const want = rec.ethHash.toLowerCase();
      outer:
      for (const t of (blk.block && blk.block.transactions) || []) {
        for (const o of t.operations || []) {
          const c = o && o.call_contract;
          if (!c || c.entry_point !== EXPLORER.submitRawTxEntryPoint || !c.args) continue;
          if (c.contract_id && c.contract_id !== EXPLORER.engineAddress) continue;
          let raw = null;
          for (const f of protoFields(b64uToBytes(c.args))) {
            if (f.field === 1 && f.wtype === 2) raw = f.payload;
          }
          // eth tx hash = keccak256 of the raw signed tx bytes (incl. type prefix).
          if (raw && raw.length && keccak256(raw).toLowerCase() === want) {
            rec.koinosTxId = t.id;
            break outer;
          }
        }
      }
    } catch (_) { /* best-effort — leave '—' */ }
  }
  if (!rec.koinosTxId) return;
  const ktxEl = document.getElementById(`ktx-${rec.seq}`);
  if (ktxEl) ktxEl.textContent = rec.koinosTxId;
  const linksEl = document.getElementById(`links-${rec.seq}`);
  if (linksEl) linksEl.innerHTML = koinosLinksHtml(rec.koinosTxId);
}

// ── Feed state + polling ────────────────────────────────────────────────────
const feedBody = document.getElementById('feedBody');
const feedDot = document.getElementById('feedDot');
const feedInfo = document.getElementById('feedInfo');
document.getElementById('engineFoot').textContent = EXPLORER.engineAddress;

const state = {
  records: [],          // decoded records, newest first
  bySeq: new Map(),
  expanded: null,       // seq currently expanded
  maxSeq: -1,
};

function renderFeed() {
  if (!state.records.length) {
    feedBody.innerHTML = '<tr><td colspan="6" class="empty-feed">No EVM activity found.</td></tr>';
    return;
  }
  const rows = [];
  for (const rec of state.records) {
    rows.push(rowHtml(rec));
    if (state.expanded === rec.seq && rec.kind === 'evm') {
      rows.push(`<tr class="detail-row"><td colspan="6"><div id="detail-${rec.seq}">loading…</div></td></tr>`);
    }
  }
  feedBody.innerHTML = rows.join('');
  // Wire row clicks
  feedBody.querySelectorAll('tr.row').forEach((tr) => {
    tr.addEventListener('click', () => onRowClick(Number(tr.dataset.seq)));
  });
  // Render any expanded detail
  if (state.expanded != null) {
    const rec = state.bySeq.get(state.expanded);
    const host = document.getElementById(`detail-${state.expanded}`);
    if (rec && rec.kind === 'evm' && host) {
      detailHtml(rec).then((html) => { host.innerHTML = html; fillBlock(rec); });
    }
  }
}

function onRowClick(seq) {
  const rec = state.bySeq.get(seq);
  if (!rec || rec.kind !== 'evm') return;
  state.expanded = (state.expanded === seq) ? null : seq;
  renderFeed();
}

function setStatus(kind, text) {
  feedDot.className = 'dot' + (kind === 'live' ? ' live' : kind === 'err' ? ' err' : '');
  feedInfo.textContent = text;
}

// Active feed source. Starts at the configured one; a failing eth path demotes
// to account_history for the rest of the session (e.g. an older proxy).
let activeSource = EXPLORER.dataSource === 'account_history' ? 'account_history' : 'eth';
let firstLoad = true;

async function fetchFeedRecords() {
  if (activeSource === 'eth') {
    try {
      return await ethRefresh();
    } catch (e) {
      console.warn('eth feed failed — falling back to account_history:', e);
      activeSource = 'account_history';
      ethFeed.records = []; ethFeed.nextBlock = null; ethFeed.head = null;
    }
  }
  const entries = await fetchHistory(EXPLORER.feedLimit);
  return entries.map(decodeEntry);
}

async function refreshFeed() {
  try {
    const decoded = (await fetchFeedRecords()).slice().sort((a, b) => b.seq - a.seq);
    const newMaxSeq = decoded.length ? decoded[0].seq : -1;
    const changed = newMaxSeq !== state.maxSeq || decoded.length !== state.records.length;
    // Only re-render when the feed actually changed — avoids a 9s flicker that
    // would collapse/reload an open detail row and re-hit REST for it.
    if (changed || firstLoad) {
      state.records = decoded;
      state.bySeq = new Map(decoded.map((r) => [r.seq, r]));
      state.maxSeq = newMaxSeq;
      // Drop the expanded row if it scrolled out of the window.
      if (state.expanded != null && !state.bySeq.has(state.expanded)) state.expanded = null;
      renderFeed();
      firstLoad = false;
    }
    const evmCount = state.records.filter((r) => r.kind === 'evm').length;
    const headLabel = activeSource === 'eth'
      ? `block ≤ ${ethFeed.head != null ? ethFeed.head : '?'}`
      : `seq ≤ ${state.maxSeq}`;
    setStatus('live', `live · ${activeSource} · ${evmCount} EVM txs · ${headLabel} · refreshes every ${Math.round(EXPLORER.pollMs / 1000)}s`);
  } catch (e) {
    setStatus('err', `error: ${e.message || e}`);
  }
}

// ── Manual decode box ─────────────────────────────────────────────────────────
const decodeInput = document.getElementById('decodeInput');
const decodeBtn = document.getElementById('decodeBtn');
const decodeResult = document.getElementById('decodeResult');

function normalizeId(s) {
  s = (s || '').trim();
  if (!s) return null;
  if (!s.startsWith('0x')) s = '0x' + s;
  return s;
}

async function findEntryForKoinosTxId(koinosTxId) {
  // REST /transaction/<id> returns the tx (+ optional receipt) directly.
  const txr = await koinosRest(`/transaction/${koinosTxId}?return_receipt=true`);
  // Shape it like an account_history value so decodeEntry can consume it.
  const transaction = txr.transaction || (txr.transactions && txr.transactions[0]);
  if (!transaction) throw new Error('transaction not found');
  // Pass the receipt through as-is (null when the tx isn't included yet) so decodeEntry
  // marks it pending rather than reporting a false "success".
  const receipt = txr.receipt || (txr.receipts && txr.receipts[0]) || null;
  return decodeEntry({ seq_num: 'manual', trx: { transaction, receipt } });
}

// Direct lookup against the proxy's restart-safe index. Returns null when the
// proxy doesn't know the hash; a pending tx (no receipt yet) renders as pending.
async function findEntryViaEthRpc(ethHash) {
  const tx = await ethRpc('eth_getTransactionByHash', [ethHash]);
  if (!tx) return null;
  const receipt = await ethRpc('eth_getTransactionReceipt', [ethHash]);
  let ts = null;
  if (tx.blockNumber != null) {
    try {
      const blk = await ethRpc('eth_getBlockByNumber', [tx.blockNumber, false]);
      if (blk && blk.timestamp != null) ts = hexNum(blk.timestamp);
    } catch (_) { /* timestamp is cosmetic */ }
  }
  return recFromEthTx(tx, receipt, ts);
}

async function findEntryForEthHash(ethHash) {
  // No reverse index eth-hash → koinos tx. Scan recent engine history and match
  // the computed eth hash. Bounded; only "recent" txs resolve this way.
  const want = ethHash.toLowerCase();
  const SCAN = 400, PAGE = 100;
  let seq = null, scanned = 0;
  while (scanned < SCAN) {
    const entries = await fetchHistory(PAGE, seq);
    if (!entries.length) break;
    for (const e of entries) {
      const rec = decodeEntry(e);
      if (rec.kind === 'evm' && rec.ethHash && rec.ethHash.toLowerCase() === want) return rec;
    }
    scanned += entries.length;
    const last = Number(entries[entries.length - 1].seq_num);
    if (!Number.isFinite(last) || last <= 0) break;
    seq = last - 1;
  }
  return null;
}

async function doDecode() {
  const raw = normalizeId(decodeInput.value);
  if (!raw) return;
  decodeResult.innerHTML = '<p class="hint">Decoding…</p>';
  decodeBtn.disabled = true;
  try {
    let rec;
    if (raw.startsWith('0x1220') && raw.length === 70) {
      rec = await findEntryForKoinosTxId(raw);          // 34-byte koinos multihash
    } else if (raw.length === 66) {
      // 32-byte eth hash: ask the proxy's index first (covers all history, incl.
      // pending txs); fall back to scanning recent engine account_history.
      try { rec = await findEntryViaEthRpc(raw); }
      catch (e) { console.warn('eth lookup failed — scanning account_history:', e); }
      if (!rec) rec = await findEntryForEthHash(raw);
      if (!rec) throw new Error('not found via the proxy or in recent engine history — paste the Koinos tx id (0x1220…) instead');
      rec.seq = 'manual'; // keep DOM ids distinct from a feed row of the same tx
    } else if (raw.startsWith('0x1220')) {
      rec = await findEntryForKoinosTxId(raw);
    } else {
      throw new Error('expected a Koinos tx id (0x1220… 66 hex) or an EVM tx hash (0x… 64 hex)');
    }
    if (!rec || rec.kind !== 'evm') throw new Error(rec && rec.error ? rec.error : 'not a decodable EVM transaction');
    decodeResult.innerHTML = `<div id="manual-detail" style="margin-top:12px"></div>`;
    const host = document.getElementById('manual-detail');
    host.innerHTML = await detailHtml(rec);
    fillBlock(rec);
  } catch (e) {
    decodeResult.innerHTML = `<p class="toast err" style="position:static;width:auto;margin-top:12px">${esc(e.message || e)}</p>`;
  } finally {
    decodeBtn.disabled = false;
  }
}

decodeBtn.addEventListener('click', doDecode);
decodeInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') doDecode(); });

// ── Boot ──────────────────────────────────────────────────────────────────────
setStatus('', 'connecting…');
refreshFeed();
setInterval(refreshFeed, EXPLORER.pollMs);
