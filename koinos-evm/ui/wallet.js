// Shared, DOM-agnostic wallet + network + UX helpers for the dApp's secondary pages
// (pool.html, and reusable elsewhere). Ported from the proven app.js swap path so the
// swap UI itself stays untouched. Reads go to the local proxy; writes via MetaMask.

import {
  BrowserProvider,
  JsonRpcProvider,
  formatUnits,
} from 'https://esm.sh/ethers@6.13.4';

import { CHAIN, EXPLORER } from './config.js';

// Direct read provider — view calls (pool state, balances) don't need MetaMask and
// work before connect. The proxy sends permissive CORS so the browser can call it.
export const readProvider = new JsonRpcProvider(CHAIN.rpcUrl, {
  chainId: CHAIN.chainIdNum,
  name: CHAIN.chainName,
});

export function shortAddr(a) {
  return a ? `${a.slice(0, 6)}…${a.slice(-4)}` : '—';
}
export function shortHash(h) {
  return h ? `${h.slice(0, 10)}…` : '';
}

// Format a uint256 amount WITHOUT Number() (which loses precision on large balances).
export function fmt(value, decimals, places = 4) {
  const s = formatUnits(value, decimals); // exact decimal string
  const [intPart, fracPart = ''] = s.split('.');
  const grouped = intPart.replace(/\B(?=(\d{3})+(?!\d))/g, ',');
  const frac = fracPart.slice(0, places).replace(/0+$/, '');
  return frac ? `${grouped}.${frac}` : grouped;
}

// Surface a readable message from MetaMask / RPC errors.
export function errMessage(err) {
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

// Ensure the wallet is on the Koinos EVM chain — add it (4902) then switch.
export async function ensureNetwork() {
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
      // Adding doesn't guarantee selection — switch explicitly.
      await eth.request({
        method: 'wallet_switchEthereumChain',
        params: [{ chainId: CHAIN.chainIdHex }],
      });
    } else {
      throw err;
    }
  }
}

// Connect MetaMask, ensure the network, and return a fresh provider/signer/account.
export async function connect() {
  if (!window.ethereum) throw new Error('MetaMask not detected — install the extension to continue.');
  let provider = new BrowserProvider(window.ethereum);
  await provider.send('eth_requestAccounts', []);
  await ensureNetwork();
  // Re-create after the possible switch so it reads fresh chain state.
  provider = new BrowserProvider(window.ethereum);
  const signer = await provider.getSigner();
  const account = await signer.getAddress();
  return { provider, signer, account };
}

// Is `provider` currently on the Koinos EVM chain?
export async function chainOk(provider) {
  try {
    const net = await provider.getNetwork();
    return Number(net.chainId) === CHAIN.chainIdNum;
  } catch {
    return false;
  }
}

// Build a toast pair bound to a given toast element. `toastConfirmed` appends a link to
// the authoritative foundation-testnet block JSON (always resolves, unlike a mainnet explorer).
export function createToaster(el) {
  let timer = null;
  function toast(msg, kind = '') {
    el.textContent = msg;
    el.className = `toast ${kind}`.trim();
    el.classList.remove('hidden');
    if (timer) clearTimeout(timer);
    if (kind !== 'pending') timer = setTimeout(() => el.classList.add('hidden'), 6000);
  }
  function toastConfirmed(message, blockNumber, kind = 'ok') {
    el.className = `toast ${kind}`.trim();
    el.classList.remove('hidden');
    if (blockNumber === undefined || blockNumber === null) {
      el.textContent = message;
    } else {
      el.textContent = `${message} · confirmed in `;
      const a = document.createElement('a');
      a.href = `${EXPLORER.koinosRestUrl}/block/${encodeURIComponent(blockNumber)}`;
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
      a.className = 'toast-link';
      a.textContent = `block ${blockNumber}`;
      el.appendChild(a);
    }
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => el.classList.add('hidden'), 12000);
  }
  return { toast, toastConfirmed };
}
