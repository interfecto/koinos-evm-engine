#!/usr/bin/env bash
#
# One command to run the Koinos EVM swap demo locally: starts the JSON-RPC proxy
# and serves the UI, then prints the MetaMask setup steps. Ctrl+C stops both.
#
set -euo pipefail

UI_DIR="$(cd "$(dirname "$0")" && pwd)"
RPC_DIR="$(cd "$UI_DIR/../rpc" && pwd)"

# Proxy config. The operator wallet pays Koinos mana for ALL relayed txs (zero-gas
# for users). This is the foundation-testnet operator key — testnet only.
export OPERATOR_PRIVKEY_HEX="${OPERATOR_PRIVKEY_HEX:?set OPERATOR_PRIVKEY_HEX to a funded foundation-testnet operator key (64-hex, no 0x)}"
export ENGINE_CONTRACT="${ENGINE_CONTRACT:-1E8igxyDU3hjbqvcoWXGFG2pRR5xLcAaoE}"
# Fixed to match the RPC URL hard-coded in config.js / printed below — not overridable.
export LISTEN_ADDR="127.0.0.1:8545"
export RC_LIMIT_MANA="${RC_LIMIT_MANA:-1500000000}"
UI_PORT="${UI_PORT:-8080}"

BIN="$RPC_DIR/target/release/koinos-evm-rpc"
if [ ! -x "$BIN" ]; then
  echo "Proxy binary not found at $BIN" >&2
  echo "Build it first:  (cd \"$RPC_DIR\" && cargo build --release)" >&2
  exit 1
fi

PROXY_PID=""
UI_PID=""
cleanup() { [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null || true; [ -n "$UI_PID" ] && kill "$UI_PID" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

echo "Starting JSON-RPC proxy on $LISTEN_ADDR ..."
"$BIN" &
PROXY_PID=$!

# Wait for the proxy to actually answer before serving the UI — fail hard if it doesn't.
ready=0
for _ in $(seq 1 20); do
  if curl -s -m 2 -H 'Content-Type: application/json' "http://${LISTEN_ADDR}" \
       -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' | grep -q 0xa455; then
    ready=1; break
  fi
  if ! kill -0 "$PROXY_PID" 2>/dev/null; then echo "Proxy exited during startup." >&2; exit 1; fi
  sleep 0.5
done
[ "$ready" = 1 ] || { echo "Proxy did not become ready on ${LISTEN_ADDR}." >&2; exit 1; }

echo "Serving UI on http://localhost:${UI_PORT} ..."
( cd "$UI_DIR" && python3 -m http.server "$UI_PORT" >/dev/null 2>&1 ) &
UI_PID=$!
sleep 0.5
curl -s -m 2 -o /dev/null "http://localhost:${UI_PORT}/index.html" \
  || { echo "UI server failed to bind :${UI_PORT}." >&2; exit 1; }

cat <<EOF

  Koinos EVM swap demo is up.

  1. Open:  http://localhost:${UI_PORT}
  2. Click "Connect MetaMask" — it adds/switches to the Koinos EVM network:
       Network name : Koinos EVM Testnet
       RPC URL      : http://localhost:8545
       Chain ID     : 42069
       Symbol       : tKOIN
  3. Use the Faucet to mint test tokens, then Swap. All txs are zero-gas.

  Press Ctrl+C to stop.
EOF

# Supervise both children portably (macOS ships bash 3.2, no `wait -n`):
# if either exits, drop out and let the cleanup trap stop the other.
while kill -0 "$PROXY_PID" 2>/dev/null && kill -0 "$UI_PID" 2>/dev/null; do
  sleep 1
done
echo "A service exited — shutting down." >&2
