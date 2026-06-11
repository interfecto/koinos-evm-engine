#!/bin/bash
set -euo pipefail

# Build the Koinos EVM Engine WASM contract
# Usage: ./build.sh [--evm]  (pass --evm to include revm EVM interpreter)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

FEATURES=""
if [[ "${1:-}" == "--evm" ]]; then
    FEATURES="--features evm"
    echo "Building with EVM interpreter..."
else
    echo "Building Phase 0 (test contract only)..."
fi

# Build (--locked: the artifact is consensus-critical; dependency drift must be
# a deliberate Cargo.lock change, never an implicit resolution at build time)
cargo build --release --locked $FEATURES

# Get the output path
TARGET_DIR="target/wasm32v1-none/release"
WASM_FILE="$TARGET_DIR/koinos_evm_engine.wasm"

if [ ! -f "$WASM_FILE" ]; then
    echo "ERROR: WASM file not found at $WASM_FILE"
    exit 1
fi

RAW_SIZE=$(wc -c < "$WASM_FILE" | tr -d ' ')
echo "Raw WASM size: ${RAW_SIZE} bytes"

# Optimize with wasm-opt — MVP-only (matches Koinos Fizzy WASM feature set)
# --mvp-features disables all post-MVP features; defaults to add them otherwise (sign-ext, bulk-mem, ...)
OPT_FILE="target/koinos_evm_engine.wasm"
wasm-opt -Oz --mvp-features "$WASM_FILE" -o "$OPT_FILE"

# Strip debug info
wasm-strip "$OPT_FILE"

OPT_SIZE=$(wc -c < "$OPT_FILE" | tr -d ' ')
echo "Optimized WASM size: ${OPT_SIZE} bytes"

# Check against 1MB limit
MAX_SIZE=$((1024 * 1024))
if [ "$OPT_SIZE" -gt "$MAX_SIZE" ]; then
    echo "ERROR: Binary exceeds 1MB Koinos max_object_size limit!"
    exit 1
fi

# MVP-WASM compliance check — Fizzy interpreter rejects post-MVP opcodes.
# Disassemble and grep for forbidden instructions. Fail if any are present.
echo "Checking MVP-WASM compliance..."
if ! command -v wasm-objdump >/dev/null 2>&1; then
    echo "WARNING: wasm-objdump not found, skipping MVP-WASM compliance check"
    echo "         (install wabt: brew install wabt)"
else
    FORBIDDEN_PATTERN='i32\.extend8_s|i32\.extend16_s|i64\.extend8_s|i64\.extend16_s|i64\.extend32_s|memory\.copy|memory\.fill|memory\.init|data\.drop|table\.copy|table\.init|elem\.drop|atomic\.|v128\.|i8x16\.|i16x8\.|i32x4\.|i64x2\.|f32x4\.|f64x2\.'
    OFFENDERS=$(wasm-objdump -d "$OPT_FILE" 2>/dev/null | grep -E "$FORBIDDEN_PATTERN" | head -5 || true)
    if [ -n "$OFFENDERS" ]; then
        echo "ERROR: Optimized WASM contains non-MVP opcodes Fizzy will reject:"
        echo "$OFFENDERS"
        exit 1
    fi
    echo "  ok: no non-MVP opcodes detected"
fi

echo ""
echo "Build complete: $OPT_FILE"
echo "Ready for deployment to Koinos."
