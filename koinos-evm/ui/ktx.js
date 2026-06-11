// Koinos testnet transaction viewer — our own explorer for the WRAPPER layer.
// koinosblocks.com only speaks mainnet, so the quest/explorer "view tx" links
// land here instead: fetch the tx + receipt from the foundation testnet REST
// (CORS-open), decode the engine's submit_raw_tx payload back into the EVM tx
// it carries, and decode evm.result / evm.log events. Same protobuf field
// layout as explorer.js (evm.result: 1=success varint, 2=gas varint,
// 3=created addr; evm.log: 1=address, 2=topics*, 3=data).

import { EXPLORER, ADDRESS_LABELS, ABI_FAMILIES } from './config.js';

const { ethers } = window;

const IFACES = Object.entries(ABI_FAMILIES).map(([f, frags]) => {
  try { return [f, new ethers.Interface(frags)]; } catch { return null; }
}).filter(Boolean);

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const short = (a) => (a ? `${a.slice(0, 6)}…${a.slice(-4)}` : '—');
const vKoin = (rc) => (Number(rc || 0) / 1e8).toFixed(3);
const toHex = (u8) => '0x' + [...u8].map((b) => b.toString(16).padStart(2, '0')).join('');

function label(a) {
  if (!a) return '—';
  const lab = ADDRESS_LABELS[String(a).toLowerCase()];
  return lab ? `${lab} (${short(a)})` : short(a);
}

function b64uToBytes(s) {
  const std = s.replace(/-/g, '+').replace(/_/g, '/');
  const bin = atob(std);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

// Defensive protobuf walker (same accumulation rules as explorer.js protoFields).
function* protoFields(buf) {
  let pos = 0;
  const n = buf.length;
  while (pos < n) {
    let tag = 0, shift = 0;
    for (;;) {
      if (pos >= n) return;
      const b = buf[pos++];
      tag += (b & 0x7f) * 2 ** shift;
      if (!(b & 0x80)) break;
      shift += 7;
      if (shift > 35) return;
    }
    const field = Math.floor(tag / 8);
    const wtype = tag % 8;
    if (wtype === 0) {
      let val = 0n, sh = 0n;
      for (;;) {
        if (pos >= n) return;
        const b = buf[pos++];
        val |= BigInt(b & 0x7f) << sh;
        if (!(b & 0x80)) break;
        sh += 7n;
      }
      yield { field, wtype, varint: val };
    } else if (wtype === 2) {
      let len = 0, sh = 0;
      for (;;) {
        if (pos >= n) return;
        const b = buf[pos++];
        len += (b & 0x7f) * 2 ** sh;
        if (!(b & 0x80)) break;
        sh += 7;
        if (sh > 49) return;
      }
      if (len < 0 || pos + len > n) return;
      const payload = buf.subarray(pos, pos + len);
      pos += len;
      yield { field, wtype, payload };
    } else if (wtype === 5) pos += 4;
    else if (wtype === 1) pos += 8;
    else return;
  }
}

function decodeCall(data) {
  if (!data || data === '0x') return null;
  for (const [, iface] of IFACES) {
    try {
      const d = iface.parseTransaction({ data });
      if (d) return d;
    } catch {}
  }
  return null;
}

function decodeLogLine(topics, data) {
  for (const [, iface] of IFACES) {
    try {
      const d = iface.parseLog({ topics, data });
      if (d) {
        const parts = d.fragment.inputs.map((inp, i) => {
          const v = d.args[i];
          const s = typeof v === 'bigint' && v >= 10n ** 15n
            ? `${ethers.formatUnits(v, 18)} ×10¹⁸`
            : Array.isArray(v)
              ? `[${v.slice(0, 5).join(', ')}${v.length > 5 ? `, … ${v.length}` : ''}]`
              : /^0x[0-9a-fA-F]{40}$/.test(String(v)) ? label(String(v)) : String(v);
          return `${inp.name || '#' + i}: ${s}`;
        });
        return `${d.name}(${parts.join(', ')})`;
      }
    } catch {}
  }
  return `topic ${topics[0]?.slice(0, 10) ?? '—'}`;
}

const row = (k, v) =>
  `<div class="insp-row"><span class="k">${esc(k)}</span><span class="v">${v}</span></div>`;

async function rest(path) {
  const res = await fetch(`${EXPLORER.koinosRestUrl}${path}`);
  if (!res.ok) throw new Error(`REST ${path}: HTTP ${res.status}`);
  return res.json();
}

async function load(txId) {
  const out = $('out');
  out.innerHTML = '<section class="card"><div class="insp-pending">loading from testnet REST…</div></section>';
  try {
    const j = await rest(`/transaction/${encodeURIComponent(txId)}?return_receipt=true`);
    const tx = j.transaction || {};
    const rec = j.receipt || {};
    const header = tx.header || {};

    // Containing block: id from the tx response; height needs one block fetch.
    let height = null;
    const blockId = (j.containing_blocks || [])[0] || null;
    if (blockId) {
      try { height = (await rest(`/block/${encodeURIComponent(blockId)}`)).block_height; } catch {}
    }

    // ── transaction card ──
    let html = `<section class="card">
      <div class="card-head"><h2>⛓ Koinos transaction</h2>
        <span class="hint"><a href="${esc(EXPLORER.koinosTxJsonUrl(txId))}" target="_blank" rel="noopener noreferrer">raw JSON ↗</a></span></div>
      <div class="insp-body">
        ${row('tx id', esc(tx.id || txId))}
        ${tx.timestamp ? row('time', esc(new Date(Number(tx.timestamp)).toUTCString())) : ''}
        ${row('block', `${height ? `height ${esc(height)} · ` : ''}${blockId ? esc(String(blockId).slice(0, 20)) + '…' : '—'}`)}
        ${row('payer', `${esc(header.payer || '—')}${header.payer === '14kajHeACgFC9aqAvRTkeG7eC349h1mMqo' ? ' <span class="lbl">(EVM relay operator)</span>' : ''}`)}
        ${row('mana', `${rec.rc_used ? `${Number(rec.rc_used).toLocaleString()} rc used = <b>${vKoin(rec.rc_used)} vKOIN</b>` : '—'} (limit ${vKoin(header.rc_limit)})`)}
        ${row('resources', `compute ${Number(rec.compute_bandwidth_used || 0).toLocaleString()} · network ${Number(rec.network_bandwidth_used || 0).toLocaleString()} · disk ${Number(rec.disk_storage_used || 0).toLocaleString()}`)}
      </div>
    </section>`;

    // ── operations ──
    for (const [i, op] of (tx.operations || []).entries()) {
      const cc = op.call_contract;
      if (!cc) {
        html += `<section class="card"><div class="card-head"><h2>operation ${i}</h2></div>
          <div class="insp-body">${row('type', esc(Object.keys(op).join(', ') || 'unknown'))}</div></section>`;
        continue;
      }
      const isEngine =
        Number(cc.entry_point) === EXPLORER.submitRawTxEntryPoint &&
        (!cc.contract_id || cc.contract_id === EXPLORER.engineAddress);
      let body =
        row('contract', `${esc(cc.contract_id || '—')}${cc.contract_id === EXPLORER.engineAddress ? ' <span class="lbl">(EVM engine — revm in WASM)</span>' : ''}`) +
        row('entry point', `${esc(String(cc.entry_point))}${isEngine ? ' <span class="lbl">= submit_raw_tx</span>' : ''}`);

      if (isEngine) {
        try {
          let raw = null;
          for (const f of protoFields(b64uToBytes(cc.args))) {
            if (f.field === 1 && f.wtype === 2) { raw = f.payload; break; }
          }
          if (raw) {
            const etx = ethers.Transaction.from(toHex(raw));
            const dec = decodeCall(etx.data);
            body += row('evm tx hash', `${esc(ethers.keccak256(raw))}`);
            body += row('evm from → to', `<span class="lbl">${esc(label(etx.from))}</span> → <span class="lbl">${esc(label(etx.to || 'contract creation'))}</span>`);
            body += row('evm nonce / gas', `${etx.nonce} · limit ${Number(etx.gasLimit).toLocaleString()} · gas price ${etx.gasPrice ?? 0n} <span class="lbl">(zero-gas)</span>`);
            body += dec
              ? `<div class="insp-call"><span class="fn">${esc(dec.name)}</span>(\n${dec.fragment.inputs
                  .map((inp, k2) => `  ${esc(inp.name || '#' + k2)}: ${esc(String(dec.args[k2]).slice(0, 120))}`)
                  .join('\n')}\n)</div>`
              : `<div class="insp-call">calldata ${esc((etx.data || '0x').slice(0, 80))}…</div>`;
          }
        } catch (e) {
          body += row('embedded evm tx', `could not decode: ${esc(e.message || e)}`);
        }
      } else {
        body += row('args', `${b64uToBytes(cc.args || '').length} bytes`);
      }
      html += `<section class="card"><div class="card-head"><h2>operation ${i} · call_contract${isEngine ? ' → EVM' : ''}</h2></div>
        <div class="insp-body">${body}</div></section>`;
    }

    // ── events ──
    const events = rec.events || [];
    if (events.length) {
      let evHtml = '';
      for (const e of events) {
        if (e.name === 'evm.result') {
          let ok = true, gas = 0n, created = null;
          for (const f of protoFields(b64uToBytes(e.data || ''))) {
            if (f.field === 1 && f.wtype === 0) ok = f.varint !== 0n;
            else if (f.field === 2 && f.wtype === 0) gas = f.varint;
            else if (f.field === 3 && f.wtype === 2 && f.payload.length === 20) created = toHex(f.payload);
          }
          evHtml += row('evm.result', `${ok ? '✓ success' : '✗ reverted'} · ${gas.toLocaleString()} EVM gas${created ? ` · created ${esc(short(created))}` : ''} <span class="lbl">(this becomes the EVM receipt)</span>`);
        } else if (e.name === 'evm.log') {
          let addr = null; const topics = []; let ldata = '0x';
          for (const f of protoFields(b64uToBytes(e.data || ''))) {
            if (f.field === 1 && f.wtype === 2 && f.payload.length === 20) addr = toHex(f.payload);
            else if (f.field === 2 && f.wtype === 2) topics.push(toHex(f.payload));
            else if (f.field === 3 && f.wtype === 2) ldata = toHex(f.payload);
          }
          evHtml += row('evm.log', `${esc(label(addr))}: ${esc(decodeLogLine(topics, ldata))}`);
        } else {
          evHtml += row(esc(e.name || 'event'), `source ${esc(short(e.source || ''))}${e.impacted?.length ? ` · impacted ${e.impacted.map((x) => esc(short(x))).join(', ')}` : ''}`);
        }
      }
      html += `<section class="card"><div class="card-head"><h2>events</h2><span class="hint">from the Koinos receipt</span></div>
        <div class="insp-body">${evHtml}</div></section>`;
    }

    out.innerHTML = html;
  } catch (e) {
    out.innerHTML = `<section class="card"><div class="insp-body">${row('error', esc(e.message || String(e)))}</div></section>`;
  }
}

// boot: ?tx=0x1220… auto-loads
const params = new URLSearchParams(location.search);
const initial = params.get('tx');
if (initial) {
  $('txIn').value = initial;
  load(initial);
}
$('loadBtn').addEventListener('click', () => {
  const v = $('txIn').value.trim();
  if (v) {
    history.replaceState(null, '', `?tx=${encodeURIComponent(v)}`);
    load(v);
  }
});
$('txIn').addEventListener('keypress', (e) => {
  if (e.key === 'Enter') $('loadBtn').click();
});
