// Shared config for the Koinos EVM swap demo UI.
// Addresses are from scripts/shell/deploy_faucet_tokens.sh (foundation testnet).

// Local dev serves the proxy directly on :8545; any other host (the public
// deployment) reaches it same-origin under /evm-rpc behind nginx — which also
// gives MetaMask the https URL it requires for non-localhost networks, and
// makes the WebSocket derive to wss:// automatically.
const IS_LOCAL_HOST =
  typeof location !== 'undefined' &&
  (location.hostname === 'localhost' || location.hostname === '127.0.0.1');

export const CHAIN = {
  chainIdHex: '0xa455', // 42069
  chainIdNum: 42069,
  chainName: 'Koinos EVM Testnet',
  rpcUrl: IS_LOCAL_HOST ? 'http://localhost:8545' : `${location.origin}/evm-rpc`,
  nativeCurrency: { name: 'Test KOIN', symbol: 'tKOIN', decimals: 18 },
};

// Self-serve mintable test tokens (open mint() — testnet only).
export const TOKENS = [
  { key: 'TFA', address: '0x16DE2d12FA6110e38081662b09cCBf99019E46c8', symbol: 'TFA', name: 'Koinos Faucet A', decimals: 18 },
  { key: 'TFB', address: '0x595Fc4e25fb1Ed866d32aa644F08cbA7b9f19348', symbol: 'TFB', name: 'Koinos Faucet B', decimals: 18 },
];

// Reused live Uniswap V2 deployment (patched init hash).
export const CONTRACTS = {
  router: '0x884df96ebbb3ab489834e869b533ff049e59e65a',
  factory: '0x603983bf054EEED6eed4262ee696A7b5eA1A00dd',
  pair: '0xdbe58068f20ccebab16a6fbbbf811740436812c5',
};

export const ERC20_ABI = [
  'function balanceOf(address owner) view returns (uint256)',
  'function allowance(address owner, address spender) view returns (uint256)',
  'function approve(address spender, uint256 amount) returns (bool)',
  'function decimals() view returns (uint8)',
  'function symbol() view returns (string)',
  'function mint(address to, uint256 amount)',
];

export const PAIR_ABI = [
  'function getReserves() view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)',
  'function token0() view returns (address)',
  'function token1() view returns (address)',
];

export const ROUTER_ABI = [
  'function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) returns (uint256[] amounts)',
];

// Generous fixed EVM gas limits. The proxy applies its own Koinos RC ceiling; these
// just need to exceed actual EVM gas to avoid eth_estimateGas edge cases and the
// receipt-null-on-out-of-gas behavior. Gas price is 0, so the operator pays anyway.
export const GAS = {
  mint: 150000n,
  approve: 90000n,
  swap: 260000n,
};

// Uniswap V2 constant-product fee (0.3%): amountInWithFee = amountIn * 997 / 1000.
export const FEE_NUM = 997n;
export const FEE_DEN = 1000n;

// Swap defaults.
export const SLIPPAGE_BPS = 50n; // 0.5% (basis points out of 10000)
export const BPS_DEN = 10000n;
export const DEADLINE_SECS = 1200; // 20 minutes from submit

// Cap on tx.wait() so a reverted/out-of-gas tx that returns a null receipt
// can't leave a button stuck "…ing" forever. Generous vs ~3s block time.
export const TX_TIMEOUT_MS = 120000;

// Block-explorer base for the "confirmed in block N" link. The proxy reports the
// Koinos block height as the EVM blockNumber, so {EXPLORER_BLOCK_URL}{blockNumber}
// points at the right block. koinscan.io is set to the testnet, matching our chain.
export const EXPLORER_BLOCK_URL = 'https://www.koinscan.io/blocks/';

// ─────────────────────────────────────────────────────────────────────────
// EVM ACTIVITY EXPLORER (explorer.html / explorer.js)
//
// Two feed sources, same client-side rich decoding either way:
//
//  'eth' (default) — the proxy's standard Ethereum JSON-RPC index
//    (eth_blockNumber / eth_getBlockByNumber / eth_getBlockReceipts /
//    eth_getLogs). The proxy keeps a durable index of all engine history, so
//    this is one normal HTTP endpoint with normal eth shapes.
//
//  'account_history' — reads the Koinos foundation testnet DIRECTLY from the
//    browser (CORS is open). Every EVM tx is relayed as a Koinos call_contract
//    op to the ENGINE contract, entry_point 7 (submit_raw_tx), with args =
//    protobuf { bytes raw_tx = 1 }, so the engine contract's account_history
//    IS the EVM tx feed — one call returns each tx + its inline receipt
//    (evm.log + evm.result events). We decode the raw_tx with ethers.
//
// The explorer falls back from 'eth' to 'account_history' at runtime if the
// proxy errors (e.g. an older proxy without the index). Calldata + logs are
// ABI-decoded against the registry below in BOTH modes.
// ─────────────────────────────────────────────────────────────────────────

export const EXPLORER = {
  // Feed source: 'eth' (proxy JSON-RPC index, preferred) or 'account_history'
  // (decode Koinos account_history client-side). Runtime falls back to
  // 'account_history' automatically when the eth path errors.
  dataSource: 'eth',
  // The EVM JSON-RPC proxy (same endpoint the wallet uses).
  ethRpcUrl: CHAIN.rpcUrl,
  // eth-feed tuning. Blocks are ~3s and almost all EMPTY, so the feed never
  // walks the chain block-by-block: the newest `ethTailBlocks` are fetched
  // directly (catches txs that emit no logs, e.g. plain transfers / reverts),
  // and older activity is located via chunked eth_getLogs windows of
  // `ethLogWindow` blocks (the proxy caps ranges at 10 000), going at most
  // `ethMaxLogWindows` deep or until the feed is full.
  ethTailBlocks: 30,
  ethLogWindow: 9900,
  ethMaxLogWindows: 12,
  // Foundation testnet endpoints (override via window.__KOINOS_RPC__ etc. if needed).
  koinosRpcUrl: 'https://testnet.koinosfoundation.org/jsonrpc',
  koinosRestUrl: 'https://testnet.koinosfoundation.org/v1',
  // The deployed revm engine contract (base58check). All EVM txs call entry_point 7 here.
  engineAddress: '1E8igxyDU3hjbqvcoWXGFG2pRR5xLcAaoE',
  // submit_raw_tx entry point (must match EP_SUBMIT_RAW_TX in the proxy / engine).
  submitRawTxEntryPoint: 7,
  // How many history entries to pull per refresh, and the poll cadence.
  feedLimit: 25,
  pollMs: 9000,
  // Native Koinos record links for a tx id (authoritative REST JSON + native explorer).
  // id is encoded defensively even though tx ids are hash-shaped (0x1220 + hex).
  koinosTxJsonUrl: (id) =>
    `https://testnet.koinosfoundation.org/v1/transaction/${encodeURIComponent(id)}?return_receipt=true`,
  // Our own Koinos-layer testnet tx viewer (ktx.html, same dir). koinosblocks.com
  // is mainnet-only, so external links there dead-end for this chain.
  koinosTxViewUrl: (id) => `ktx.html?tx=${encodeURIComponent(id)}`,
};

// ─────────────────────────────────────────────────────────────────────────
// PIXEL CANVAS QUEST (quest.html / quest.js)
//
// 64x64 shared canvas, 16-color palette, batched setPixels (max 128/call,
// 1024/block). Deployed 2026-06-11 from scripts/forge/src/PixelCanvas.sol.
// The palette lives client-side: PALETTE[i] renders color index i; index 0
// (storage default) is white. Live updates stream over the proxy's WebSocket
// (same port as HTTP — GET upgrades) via eth_subscribe("logs").
// ─────────────────────────────────────────────────────────────────────────
export const PIXEL = {
  address: '0x4157cCC46B6B328A1732527ceF7E52E9B7F261e0',
  deployBlock: 5714545, // first possible PixelsSet log — log-replay starts here
  width: 64,
  maxBatch: 128,
  // r/place 2017 palette; index 0 = white = untouched storage.
  palette: [
    '#FFFFFF', '#E4E4E4', '#888888', '#222222',
    '#FFA7D1', '#E50000', '#E59500', '#A06A42',
    '#E5D900', '#94E044', '#02BE01', '#00D3DD',
    '#0083C7', '#0000EA', '#CF6EE4', '#820080',
  ],
};

export const PIXEL_ABI = [
  'function setPixels(uint16[] positions, uint8[] colors)',
  'function getCanvas() view returns (bytes)',
  'function pixel(uint16 pos) view returns (uint8)',
  'function totalPixels() view returns (uint256)',
  'function pixelsBy(address artist) view returns (uint256)',
  'function paused() view returns (bool)',
  'event PixelsSet(address indexed artist, uint16[] positions, uint8[] colors)',
];

// setPixels cost is dominated by how many DISTINCT 32-pixel storage words a
// batch touches (~22k gas per fresh word: cold SLOAD + SSTORE), not by pixel
// count: 128 CONSECUTIVE pixels = 4 words ≈ 371k gas (measured on-chain), but
// 128 SCATTERED pixels can hit 128 words ≈ ~3M gas — which exceeds both a flat
// UI gas limit and the relay's per-tx Koinos RC budget ("insufficient rc").
// The quest page therefore estimates per-batch gas word-aware and AUTO-CHUNKS
// a drawing into multiple txs so each stays within `budget` EVM gas (~2.7e8 rc,
// V3-swap-sized — proven to fit blocks). margin covers estimator error.
export const PIXEL_GAS = {
  base: 100000n,     // tx base + counters (totalPixels/pixelsBy/paintedInBlock) + log bases
  perPixel: 1200n,   // calldata + loop + event data, per pixel
  perWord: 23000n,   // cold SLOAD + SSTORE per distinct storage word
  budget: 800000n,   // max ESTIMATED gas per chunk (before margin)
  marginNum: 13n,    // gasLimit = estimate * 13/10
  marginDen: 10n,
};

// Per-address ABI-family hints. When a tx's `to` is a known contract, the decoder
// tries these families FIRST — this resolves selector collisions that share an exact
// signature (e.g. NFPM's ERC-721 `approve(to,tokenId)` vs ERC-20 `approve(spender,amount)`),
// and speeds up decoding. Keys lowercase. Falls back to the global order otherwise.
export const ADDRESS_FAMILIES = {
  '0xd6e62f045a84a77bd9a6176a0f61aa515c131c75': ['nfpm', 'erc721'], // NFPM is an ERC-721
  '0x4157ccc46b6b328a1732527cef7e52e9b7f261e0': ['pixel'],
  '0x16ae0402bbd80d251514095c4c0f27c1cd769c70': ['v3router'],
  '0x6cd554d8c841cd2a0cf297eb49118c68d6daf88a': ['quoter'],
  '0x884df96ebbb3ab489834e869b533ff049e59e65a': ['v2router'],
  '0x603983bf054eeed6eed4262ee696a7b5ea1a00dd': ['factory'],
  '0x7be78d086661587b6f62979bd2d5650c45e2eff1': ['factory'],
  '0xdbe58068f20ccebab16a6fbbbf811740436812c5': ['v2pair'],
  '0xaae4b5b92f78d758b2ff320dc5ad77480a9969ee': ['v3pool'],
  '0x4afd5b74ca5a6b159e8db6cd93ee8b2d24cf11f3': ['v3pool'],
  '0x64a2b174372ac3fb69b125eedec1d86ac14a2722': ['v3pool'],
  '0xf9a437835caaa3a109898db65deb408701e6c838': ['helpers'],
  '0x32a304966be2c20639a77e15260d6ff99d53ba69': ['helpers'],
  '0x36b6fecf048fb8fd03f491bcad75467061f663d3': ['helpers'],
  '0x8ed6a5e4a7821180b5ce2ba6945e81680b6049e3': ['helpers'],
  '0x1ee8b0bcab047b4513587497aa0ca595b5d7b0de': ['helpers'],
  // ERC-20 tokens (so token calls try erc20 first)
  '0x16de2d12fa6110e38081662b09ccbf99019e46c8': ['erc20'],
  '0x595fc4e25fb1ed866d32aa644f08cba7b9f19348': ['erc20'],
  '0x9fb64fb2ea0dac3762b987218d7241d3575d2946': ['erc20'],
  '0x0639322cbd5c8417ed7fae4dd4f6049f40d86381': ['erc20'],
  '0xc5d70df9046fa4845f6bc3e3062f9694b4928433': ['erc20'],
  '0x1bba4ff03261bf12f76a15de52fc2f35282428b4': ['erc20'],
};

// Known EVM-side addresses → human labels. Keys are lowercase, no-checksum.
// Sourced from the V2/V3-core/V3-periphery deployments on the foundation testnet.
export const ADDRESS_LABELS = {
  // EOAs
  '0x017a12939ab9139518d4b31f58ebe701a5aa4b82': 'Deployer / Operator',
  // ERC-20 tokens
  '0x16de2d12fa6110e38081662b09ccbf99019e46c8': 'TFA · Faucet A',
  '0x595fc4e25fb1ed866d32aa644f08cba7b9f19348': 'TFB · Faucet B',
  '0x9fb64fb2ea0dac3762b987218d7241d3575d2946': 'KEVM (test ERC-20)',
  '0x0639322cbd5c8417ed7fae4dd4f6049f40d86381': 'V3A · token0',
  '0xc5d70df9046fa4845f6bc3e3062f9694b4928433': 'V3B · token1',
  '0x1bba4ff03261bf12f76a15de52fc2f35282428b4': 'WETH9 (V3 periphery)',
  // Uniswap V2
  '0x884df96ebbb3ab489834e869b533ff049e59e65a': 'Uniswap V2 Router02',
  '0x603983bf054eeed6eed4262ee696a7b5ea1a00dd': 'Uniswap V2 Factory',
  '0xdbe58068f20ccebab16a6fbbbf811740436812c5': 'V2 Pair · TFA/TFB',
  // Uniswap V3 core
  '0x7be78d086661587b6f62979bd2d5650c45e2eff1': 'Uniswap V3 Factory',
  '0xaae4b5b92f78d758b2ff320dc5ad77480a9969ee': 'V3 Pool · 0.30% (V3A/V3B)',
  '0x4afd5b74ca5a6b159e8db6cd93ee8b2d24cf11f3': 'V3 Pool · 0.05% (V3A/V3B)',
  '0x64a2b174372ac3fb69b125eedec1d86ac14a2722': 'V3 Pool · TFA/TFB 0.30%',
  '0xf9a437835caaa3a109898db65deb408701e6c838': 'V3Minter (helper)',
  '0x32a304966be2c20639a77e15260d6ff99d53ba69': 'V3Swapper (helper)',
  '0x1ee8b0bcab047b4513587497aa0ca595b5d7b0de': 'V3 MINTER2 (helper)',
  '0x36b6fecf048fb8fd03f491bcad75467061f663d3': 'V3 MINTER500 (helper)',
  '0x8ed6a5e4a7821180b5ce2ba6945e81680b6049e3': 'V3 SWAPPER500 (helper)',
  // Uniswap V3 periphery
  '0x16ae0402bbd80d251514095c4c0f27c1cd769c70': 'Uniswap V3 SwapRouter',
  '0xd6e62f045a84a77bd9a6176a0f61aa515c131c75': 'V3 PositionManager (NFPM)',
  '0x6cd554d8c841cd2a0cf297eb49118c68d6daf88a': 'V3 QuoterV2',
  // CREATE2 test contracts
  '0x03bf6b4b070217e47635cee7e412327bcd57980e': 'Test CREATE2 Factory',
  '0x2caef6a2783db0b7b87d854649ede7fd65790939': 'Test Child',
  // Quest
  '0x4157ccc46b6b328a1732527cef7e52e9b7f261e0': 'PixelCanvas · Quest',
};

// ABI fragments grouped by protocol family. The explorer builds one ethers
// Interface per family and tries each in turn when decoding calldata / logs —
// so selector/topic collisions across families (e.g. ERC-20 vs ERC-721
// `Transfer`) are resolved by falling through to the next family rather than
// throwing. Human-readable signatures; ethers v6 parses these directly.
export const ABI_FAMILIES = {
  erc20: [
    'function transfer(address to, uint256 amount) returns (bool)',
    'function transferFrom(address from, address to, uint256 amount) returns (bool)',
    'function approve(address spender, uint256 amount) returns (bool)',
    'function mint(address to, uint256 amount)',
    'function mint(uint256 amount)',
    'function burn(uint256 amount)',
    'event Transfer(address indexed from, address indexed to, uint256 value)',
    'event Approval(address indexed owner, address indexed spender, uint256 value)',
  ],
  erc721: [
    'function approve(address to, uint256 tokenId)',
    'function setApprovalForAll(address operator, bool approved)',
    'function transferFrom(address from, address to, uint256 tokenId)',
    'function safeTransferFrom(address from, address to, uint256 tokenId)',
    'event Transfer(address indexed from, address indexed to, uint256 indexed tokenId)',
    'event Approval(address indexed owner, address indexed approved, uint256 indexed tokenId)',
    'event ApprovalForAll(address indexed owner, address indexed operator, bool approved)',
  ],
  v2router: [
    'function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) returns (uint256[])',
    'function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) returns (uint256[])',
    'function addLiquidity(address tokenA, address tokenB, uint256 amountADesired, uint256 amountBDesired, uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline) returns (uint256, uint256, uint256)',
    'function removeLiquidity(address tokenA, address tokenB, uint256 liquidity, uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline) returns (uint256, uint256)',
  ],
  v2pair: [
    'function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes data)',
    'function mint(address to) returns (uint256 liquidity)',
    'function burn(address to) returns (uint256 amount0, uint256 amount1)',
    'function sync()',
    'event Swap(address indexed sender, uint256 amount0In, uint256 amount1In, uint256 amount0Out, uint256 amount1Out, address indexed to)',
    'event Sync(uint112 reserve0, uint112 reserve1)',
    'event Mint(address indexed sender, uint256 amount0, uint256 amount1)',
    'event Burn(address indexed sender, uint256 amount0, uint256 amount1, address indexed to)',
  ],
  v3pool: [
    'function initialize(uint160 sqrtPriceX96)',
    'function mint(address recipient, int24 tickLower, int24 tickUpper, uint128 amount, bytes data) returns (uint256, uint256)',
    'function burn(int24 tickLower, int24 tickUpper, uint128 amount) returns (uint256, uint256)',
    'function collect(address recipient, int24 tickLower, int24 tickUpper, uint128 amount0Requested, uint128 amount1Requested) returns (uint128, uint128)',
    'function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes data) returns (int256, int256)',
    'function flash(address recipient, uint256 amount0, uint256 amount1, bytes data)',
    'function setFeeProtocol(uint8 feeProtocol0, uint8 feeProtocol1)',
    'event Initialize(uint160 sqrtPriceX96, int24 tick)',
    'event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)',
    'event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)',
    'event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)',
    'event Collect(address indexed owner, address recipient, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount0, uint128 amount1)',
    'event Flash(address indexed sender, address indexed recipient, uint256 amount0, uint256 amount1, uint256 paid0, uint256 paid1)',
  ],
  v3router: [
    'function exactInputSingle((address tokenIn, address tokenOut, uint24 fee, address recipient, uint256 deadline, uint256 amountIn, uint256 amountOutMinimum, uint160 sqrtPriceLimitX96) params) payable returns (uint256 amountOut)',
    'function exactInput((bytes path, address recipient, uint256 deadline, uint256 amountIn, uint256 amountOutMinimum) params) payable returns (uint256 amountOut)',
    'function exactOutputSingle((address tokenIn, address tokenOut, uint24 fee, address recipient, uint256 deadline, uint256 amountOut, uint256 amountInMaximum, uint160 sqrtPriceLimitX96) params) payable returns (uint256 amountIn)',
    'function exactOutput((bytes path, address recipient, uint256 deadline, uint256 amountOut, uint256 amountInMaximum) params) payable returns (uint256 amountIn)',
    'function multicall(bytes[] data) payable returns (bytes[])',
  ],
  nfpm: [
    'function mint((address token0, address token1, uint24 fee, int24 tickLower, int24 tickUpper, uint256 amount0Desired, uint256 amount1Desired, uint256 amount0Min, uint256 amount1Min, address recipient, uint256 deadline) params) payable returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)',
    'function increaseLiquidity((uint256 tokenId, uint256 amount0Desired, uint256 amount1Desired, uint256 amount0Min, uint256 amount1Min, uint256 deadline) params) payable returns (uint128, uint256, uint256)',
    'function decreaseLiquidity((uint256 tokenId, uint128 liquidity, uint256 amount0Min, uint256 amount1Min, uint256 deadline) params) payable returns (uint256, uint256)',
    'function collect((uint256 tokenId, address recipient, uint128 amount0Max, uint128 amount1Max) params) payable returns (uint256, uint256)',
    'function burn(uint256 tokenId) payable',
    'event IncreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)',
    'event DecreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)',
    'event Collect(uint256 indexed tokenId, address recipient, uint256 amount0, uint256 amount1)',
  ],
  quoter: [
    'function quoteExactInputSingle((address tokenIn, address tokenOut, uint256 amountIn, uint24 fee, uint160 sqrtPriceLimitX96) params) returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate)',
  ],
  factory: [
    'function createPair(address tokenA, address tokenB) returns (address pair)',
    'function createPool(address tokenA, address tokenB, uint24 fee) returns (address pool)',
    'event PairCreated(address indexed token0, address indexed token1, address pair, uint256)',
    'event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool)',
  ],
  pixel: [
    'function setPixels(uint16[] positions, uint8[] colors)',
    'function setPaused(bool p)',
    'event PixelsSet(address indexed artist, uint16[] positions, uint8[] colors)',
  ],
  // Our owner-guarded V3 test helpers (V3Minter / V3Swapper / V3Flash).
  helpers: [
    'function mint(int24 tickLower, int24 tickUpper, uint128 amount)',
    'function burn(int24 tickLower, int24 tickUpper, uint128 amount)',
    'function collect(address recipient, int24 tickLower, int24 tickUpper, uint128 amount0Requested, uint128 amount1Requested)',
    'function swap(bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96)',
    'function flash(uint256 amount0, uint256 amount1)',
  ],
};

// ─────────────────────────────────────────────────────────────────────────
// V3 POOL MANAGER (pool.html / pool.js)
//
// Open-mint TFA/TFB Uniswap V3 0.3% pool created + seeded by
// scripts/shell/deploy_v3_faucet_pool.sh. token0 = TFA (numerically-smaller
// address), token1 = TFB. Any wallet can faucet-mint TFA/TFB (TOKENS above) and
// then add liquidity / swap / collect against this pool, zero-gas.
// ─────────────────────────────────────────────────────────────────────────
export const V3 = {
  factory: '0x7be78d086661587b6f62979bd2d5650c45e2eff1',
  nfpm: '0xd6e62f045a84a77bd9a6176a0f61aa515c131c75',          // NonfungiblePositionManager
  swapRouter: '0x16ae0402bbd80d251514095c4c0f27c1cd769c70',
  pool: '0x64a2B174372AC3fb69b125EEdEC1D86AC14a2722',          // TFA/TFB 0.3% pool
  fee: 3000,
  tickSpacing: 60,
  minTick: -887220,   // full-range usable ticks for spacing 60 (887272 floored to a multiple of 60)
  maxTick: 887220,
  token0: TOKENS[0],  // TFA
  token1: TOKENS[1],  // TFB
};

// Callable ABIs (full signatures w/ return types) for pool.js. The explorer's
// ABI_FAMILIES above are decode-only; these are for ethers Contract calls.
export const NFPM_ABI = [
  'function mint((address token0,address token1,uint24 fee,int24 tickLower,int24 tickUpper,uint256 amount0Desired,uint256 amount1Desired,uint256 amount0Min,uint256 amount1Min,address recipient,uint256 deadline)) payable returns (uint256 tokenId,uint128 liquidity,uint256 amount0,uint256 amount1)',
  'function increaseLiquidity((uint256 tokenId,uint256 amount0Desired,uint256 amount1Desired,uint256 amount0Min,uint256 amount1Min,uint256 deadline)) payable returns (uint128 liquidity,uint256 amount0,uint256 amount1)',
  'function decreaseLiquidity((uint256 tokenId,uint128 liquidity,uint256 amount0Min,uint256 amount1Min,uint256 deadline)) payable returns (uint256 amount0,uint256 amount1)',
  'function collect((uint256 tokenId,address recipient,uint128 amount0Max,uint128 amount1Max)) payable returns (uint256 amount0,uint256 amount1)',
  'function burn(uint256 tokenId) payable',
  'function multicall(bytes[] data) payable returns (bytes[] results)',
  'function totalSupply() view returns (uint256)',
  'event IncreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)',
  'event DecreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)',
  'event Collect(uint256 indexed tokenId, address recipient, uint256 amount0, uint256 amount1)',
];

export const V3POOL_ABI = [
  'function slot0() view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked)',
  'function liquidity() view returns (uint128)',
  'function fee() view returns (uint24)',
  'function tickSpacing() view returns (int24)',
  'function token0() view returns (address)',
  'function token1() view returns (address)',
];

export const V3ROUTER_ABI = [
  'function exactInputSingle((address tokenIn,address tokenOut,uint24 fee,address recipient,uint256 deadline,uint256 amountIn,uint256 amountOutMinimum,uint160 sqrtPriceLimitX96)) payable returns (uint256 amountOut)',
];

// Generous fixed EVM gas limits for V3 ops (gas price is 0; the proxy applies its
// own Koinos RC ceiling). These just need to exceed actual EVM gas.
export const V3_GAS = {
  approve: 90000n,
  mint: 1000000n,
  increase: 700000n,
  decrease: 500000n,
  collect: 400000n,
  burn: 300000n,
  swap: 500000n,
};

