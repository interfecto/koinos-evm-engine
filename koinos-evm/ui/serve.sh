#!/usr/bin/env bash
#
# Serve the Koinos EVM swap UI over http (MetaMask injects window.ethereum on
# http://localhost). The JSON-RPC proxy must be running separately on :8545.
#
set -euo pipefail
PORT="${PORT:-8080}"
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"
echo "Koinos EVM swap UI → http://localhost:${PORT}"
echo "(proxy must be running on http://localhost:8545)"
exec python3 -m http.server "$PORT"
