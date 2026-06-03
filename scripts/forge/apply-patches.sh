#!/usr/bin/env bash
# Apply local source patches to vendored submodules. Run ONCE after
# `git submodule update --init --recursive`, before building the Uniswap V2 profile.
#
# Currently one patch: v2-periphery's UniswapV2Library hard-codes the Pair init-code
# hash, and we use a *locally-built* Pair (compiled under [profile.uniswap]: solc 0.5.16,
# evm_version=istanbul, optimizer runs=999999), whose init-code hash differs from
# canonical mainnet. Without this patch, UniswapV2Router02.pairFor() computes the wrong
# pair address and every V2 router call fails. The hash is deterministic given the pinned
# build settings (foundry.toml + foundry.lock), so the patched value is stable.
#
# Uses POSIX `patch` (not `git apply`) so it works regardless of submodule git linkage.
set -euo pipefail
FORGE_DIR="$(cd "$(dirname "$0")" && pwd)"
PATCH="$FORGE_DIR/patches/v2-periphery-pair-init-hash.patch"
PERIPHERY="$FORGE_DIR/lib/v2-periphery"

[ -d "$PERIPHERY" ] || {
  echo "lib/v2-periphery missing — run: git submodule update --init --recursive" >&2
  exit 1
}

# Idempotent: if the patch reverse-applies cleanly, it's already in place.
if patch -p1 -R --dry-run -d "$PERIPHERY" < "$PATCH" >/dev/null 2>&1; then
  echo ">> v2-periphery init-hash patch already applied — skipping"
else
  patch -p1 -d "$PERIPHERY" < "$PATCH"
  echo ">> applied: v2-periphery Pair init-code hash"
fi
