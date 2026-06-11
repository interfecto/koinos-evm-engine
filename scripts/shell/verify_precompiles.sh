#!/usr/bin/env bash
#
# Precompile probe suite (ROADMAP §1 steps 2-3): prove precompiles 0x02-0x09
# produce byte-identical results on the Koinos EVM engine vs a reference EVM.
#
# Deploys scripts/forge/src/PrecompileProbe.sol, runs probeAll() (0x02-0x09,
# one committing tx) plus probePointEval() (0x0a, its OWN tx -- on the engine
# revm's fatal 0x0a stub aborts the whole transaction, which would wipe every
# other probe's events from a combined receipt), and reads the ProbeResult
# events out of the receipts. Results travel via EVENTS, never eth_call return
# data, because the public engine node cannot serve heavy reads (-1013).
#
# Mode A (default, REF=anvil): start a local anvil (chain-id 42069), probe it,
#   ASSERT every non-0x0a result against the hardcoded EIP test-vector
#   expectations below, and write scripts/forge/precompile-reference.json.
#     ./verify_precompiles.sh
#     ANVIL_URL=http://127.0.0.1:8545 ./verify_precompiles.sh   # reuse a node
#
# Mode B (MODE=engine RPC=<url>): probe the Koinos EVM proxy and compare the
#   events byte-for-byte against precompile-reference.json (0x0a informational,
#   never fatal). Exits non-zero on any 0x02-0x09 mismatch.
#     MODE=engine RPC=http://localhost:8545 DEPLOYER_PK=0x... ./verify_precompiles.sh
#
# Keys: the script never needs the Koinos operator key. It signs EVM txs with
# DEPLOYER_PK; gas is free on the engine, so ANY throwaway EVM key works
# (e.g. `cast wallet new`). Mode A defaults to anvil's well-known dev key #0.
#
set -euo pipefail

MODE="${MODE:-anvil}"                 # anvil (Mode A) | engine (Mode B)
GAS_LIMIT="${GAS_LIMIT:-5000000}"     # estimateGas is broken on the public node
ANVIL_PORT="${ANVIL_PORT:-18545}"     # off 8545 so a running proxy is untouched
ANVIL_CHAIN_ID=42069

FORGE_DIR="$(cd "$(dirname "$0")/../forge" && pwd)"
REF_JSON="$FORGE_DIR/precompile-reference.json"
# keccak256("ProbeResult(uint8,uint16,bool,bytes)")
TOPIC0="0xe94e562a89fdcbd58d2995a34af01ea48334cd7b7aa2c46357401a81ed5f1d10"

is_addr() { [[ "$1" =~ ^0x[0-9a-fA-F]{40}$ ]]; }
nap() { python3 -c 'import time,sys; time.sleep(float(sys.argv[1]))' "$1"; }

TMP="$(mktemp -d)"
ANVIL_PID=""
cleanup() {
  [[ -n "$ANVIL_PID" ]] && kill "$ANVIL_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

# ── mode / RPC / key resolution ──────────────────────────────────────────────
case "$MODE" in
  anvil)
    if [[ -n "${ANVIL_URL:-}" ]]; then
      RPC="$ANVIL_URL"
    else
      RPC="http://127.0.0.1:${ANVIL_PORT}"
      echo ">> starting anvil --chain-id $ANVIL_CHAIN_ID --port $ANVIL_PORT"
      anvil --chain-id "$ANVIL_CHAIN_ID" --port "$ANVIL_PORT" --silent &
      ANVIL_PID=$!
      for _ in $(seq 1 50); do
        cast chain-id --rpc-url "$RPC" >/dev/null 2>&1 && break
        nap 0.2
      done
      cast chain-id --rpc-url "$RPC" >/dev/null \
        || { echo "ERROR: anvil did not come up on $RPC" >&2; exit 1; }
    fi
    # anvil dev key #0 -- throwaway, publicly known, Mode A only.
    PK="${DEPLOYER_PK:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
    SEND_FLAGS=(--legacy --gas-limit "$GAS_LIMIT")
    ;;
  engine)
    RPC="${RPC:?set RPC to the Koinos EVM proxy URL for MODE=engine}"
    PK="${DEPLOYER_PK:?set DEPLOYER_PK (any throwaway EVM key; gas is free)}"
    # --legacy + explicit gas limit + zero gas price on every send:
    # estimateGas / EIP-1559 fee queries are broken on the public node.
    SEND_FLAGS=(--legacy --gas-price 0 --gas-limit "$GAS_LIMIT")
    [[ -f "$REF_JSON" ]] \
      || { echo "ERROR: $REF_JSON missing -- run Mode A (anvil) first" >&2; exit 1; }
    ;;
  *) echo "ERROR: MODE must be 'anvil' or 'engine' (got '$MODE')" >&2; exit 1 ;;
esac

echo ">> mode=$MODE rpc=$RPC"

# ── build + deploy ───────────────────────────────────────────────────────────
cd "$FORGE_DIR"
echo ">> forge build (default profile)"
forge build >/dev/null
BC="$(forge inspect PrecompileProbe bytecode)"

send() { cast send --rpc-url "$RPC" --private-key "$PK" "${SEND_FLAGS[@]}" "$@"; }

echo ">> deploy PrecompileProbe"
PROBE="$(send --json --create "$BC" | jq -r .contractAddress)"
is_addr "$PROBE" || { echo "ERROR: deploy returned no address: '$PROBE'" >&2; exit 1; }
echo "   PROBE=$PROBE"

# ── probe txs ────────────────────────────────────────────────────────────────
echo ">> probeAll() (0x02-0x09, one tx)"
TX_ALL="$(send --json "$PROBE" "probeAll()" | jq -r .transactionHash)"
cast receipt --rpc-url "$RPC" --json "$TX_ALL" > "$TMP/receipt_all.json"
ST="$(jq -r .status "$TMP/receipt_all.json")"
[[ "$ST" == "0x1" || "$ST" == "1" ]] \
  || { echo "ERROR: probeAll() tx $TX_ALL failed (status=$ST)" >&2; exit 1; }
echo "   tx=$TX_ALL gasUsed=$(jq -r .gasUsed "$TMP/receipt_all.json")"

# 0x0a deliberately runs in its OWN tx and total tx failure is tolerated:
# on the engine revm's fatal stub aborts the whole transaction (or the proxy
# rejects it outright), which is itself the observation we want to record.
echo ">> probePointEval() (0x0a, separate tx, informational)"
PE_STATE="tx_failed"
echo '{}' > "$TMP/receipt_pe.json"
if PE_OUT="$(send --json "$PROBE" "probePointEval()" 2>"$TMP/pe_err.txt")"; then
  TX_PE="$(jq -r .transactionHash <<<"$PE_OUT")"
  if cast receipt --rpc-url "$RPC" --json "$TX_PE" > "$TMP/receipt_pe.json" 2>>"$TMP/pe_err.txt"; then
    PE_STATE="mined"
    echo "   tx=$TX_PE status=$(jq -r .status "$TMP/receipt_pe.json")"
  fi
fi
[[ "$PE_STATE" == "tx_failed" ]] \
  && echo "   0x0a tx did not mine (expected on the engine): $(head -c 200 "$TMP/pe_err.txt" | tr '\n' ' ')"

# ── decode events, assert / compare ──────────────────────────────────────────
MODE="$MODE" PROBE="$PROBE" TOPIC0="$TOPIC0" REF_JSON="$REF_JSON" \
RECEIPT_ALL="$TMP/receipt_all.json" RECEIPT_PE="$TMP/receipt_pe.json" \
PE_STATE="$PE_STATE" PE_ERR_FILE="$TMP/pe_err.txt" \
python3 <<'PYEOF'
import json, os, sys

mode      = os.environ["MODE"]
probe     = os.environ["PROBE"].lower()
topic0    = os.environ["TOPIC0"].lower()
ref_path  = os.environ["REF_JSON"]
pe_state  = os.environ["PE_STATE"]

# Hardcoded expectations for the EIP test vectors baked into PrecompileProbe.sol.
# Mode A asserts anvil against THESE (so anvil itself is checked against the
# EIPs, not merely recorded). Sources are cited in PrecompileProbe.sol.
EXPECTED = {
    # 0x02 SHA-256("abc") -- NIST FIPS 180-2 B.1
    "0x02:1": (True, "0xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
    # 0x03 RIPEMD-160("abc"), left-padded to 32 bytes -- RIPEMD-160 reference vectors
    "0x03:1": (True, "0x0000000000000000000000008eb208f7e05d987a9b044a8e98c6b087f15a0bfc"),
    # 0x04 identity round-trip of the 36-byte ASCII probe string
    "0x04:1": (True, "0x" + b"Koinos EVM precompile identity probe".hex()),
    # 0x05 EIP-198 example: 3^2 mod 5 == 4 (32-byte padded)
    "0x05:1": (True, "0x" + "04".rjust(64, "0")),
    # 0x05 RSA-shaped 64-byte case: pow(base, 65537, mod) -- see contract comment
    "0x05:2": (True, "0x671fda505b8b5d4e7a0436765556e7133b89742658a86f57cb00666b06198a0f"
                     "4f79cd764b6c614760e5ddfda3e686291a35548d4edf38f61e789a7425081213"),
    # 0x06 EIP-196 add reference vector ("chfast1")
    "0x06:1": (True, "0x2243525c5efd4b9c3d3c45ac0ca3fe4dd85e830a4ce6b65fa1eeaee202839703"
                     "301d1d33be6da8e509df21cc35964723180eed7532537db9ae5e7d48f195c915"),
    # 0x07 EIP-196 mul reference vector ("chfast1")
    "0x07:1": (True, "0x070a8d6a982153cae4be29d434e8faef8a47b274a053f5a4ee2a6c9c13c31e5c"
                     "031b8ce914eba3a9ffb989f9cdd5b0f01943074bf4f0f315690ec3cec6981afc"),
    # 0x08 EIP-197: e(O, G2) == 1; e(G1, G2) != 1; e(P,Q)*e(-P,Q) == 1
    "0x08:1": (True, "0x" + "01".rjust(64, "0")),
    "0x08:2": (True, "0x" + "00".rjust(64, "0")),
    "0x08:3": (True, "0x" + "01".rjust(64, "0")),
    # 0x09 EIP-152 test vector 5 (12 rounds) == BLAKE2b-512("abc")
    "0x09:1": (True, "0xba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1"
                     "7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"),
}

def decode(receipt_path):
    with open(receipt_path) as f:
        rc = json.load(f)
    out = {}
    for lg in rc.get("logs") or []:
        if lg["address"].lower() != probe:
            continue
        topics = lg["topics"]
        if not topics or topics[0].lower() != topic0:
            continue
        pc, vec = int(topics[1], 16), int(topics[2], 16)
        data = bytes.fromhex(lg["data"][2:])
        ok  = int.from_bytes(data[0:32], "big") != 0
        off = int.from_bytes(data[32:64], "big")
        ln  = int.from_bytes(data[off:off + 32], "big")
        out[f"0x{pc:02x}:{vec}"] = {
            "success": ok,
            "output": "0x" + data[off + 32:off + 32 + ln].hex(),
        }
    return out

results = decode(os.environ["RECEIPT_ALL"])

# 0x0a -- informational only (see script header / contract comment for why).
if pe_state == "mined":
    pe = decode(os.environ["RECEIPT_PE"])
    if pe:
        results["0x0a:1"] = {**pe["0x0a:1"], "informational": True}
    else:
        results["0x0a:1"] = {"tx_failed": True, "informational": True,
                             "note": "tx mined but reverted/emitted nothing (fatal stub)"}
else:
    err = open(os.environ["PE_ERR_FILE"]).read().strip()[:300]
    results["0x0a:1"] = {"tx_failed": True, "informational": True, "note": err}

def short(s, n=20):
    return s if len(s) <= 2 + 2 * n else s[:2 + 2 * n] + ".."

def table(rows):
    print(f"{'precompile:vector':<18} {'success':<8} {'output':<46} verdict")
    for key, got_s, got_o, verdict in rows:
        print(f"{key:<18} {str(got_s):<8} {short(got_o):<46} {verdict}")

fails = 0
rows = []
if mode == "anvil":
    for key, (exp_s, exp_o) in sorted(EXPECTED.items()):
        got = results.get(key)
        if got is None:
            rows.append((key, "-", "(missing event)", "FAIL")); fails += 1
        elif got["success"] == exp_s and got["output"].lower() == exp_o.lower():
            rows.append((key, got["success"], got["output"], "PASS"))
        else:
            rows.append((key, got["success"], got["output"], f"FAIL (expected {short(exp_o)})"))
            fails += 1
    pe = results["0x0a:1"]
    rows.append(("0x0a:1", pe.get("success", "tx_failed"), pe.get("output", pe.get("note", "")), "INFO"))
    table(rows)
    if fails:
        print(f"\nFAIL: {fails} anvil result(s) diverge from the EIP vectors; "
              f"reference NOT written", file=sys.stderr)
        sys.exit(1)
    ref = {
        "_meta": {
            "contract": "scripts/forge/src/PrecompileProbe.sol",
            "event": "ProbeResult(uint8,uint16,bool,bytes)",
            "reference": "anvil (foundry), chain-id 42069, EIP test vectors asserted",
            "note": "0x0a is informational: engine has revm's fatal 0x0a stub (no c-kzg); "
                    "anvil fails the invalid input inside the staticcall instead.",
        },
        "results": results,
    }
    with open(ref_path, "w") as f:
        json.dump(ref, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"\nPASS: all {len(EXPECTED)} vectors match the EIP expectations")
    print(f"reference written: {ref_path}")
else:  # engine: byte-for-byte against the anvil reference
    with open(ref_path) as f:
        ref = json.load(f)["results"]
    keys = sorted(set(ref) | set(results))
    for key in keys:
        if key.startswith("0x0a"):
            continue
        want, got = ref.get(key), results.get(key)
        if want is None:
            rows.append((key, got["success"], got["output"], "FAIL (not in reference)")); fails += 1
        elif got is None:
            rows.append((key, "-", "(missing event)", "FAIL")); fails += 1
        elif got["success"] == want["success"] and got["output"].lower() == want["output"].lower():
            rows.append((key, got["success"], got["output"], "PASS"))
        else:
            rows.append((key, got["success"], got["output"],
                         f"FAIL (reference {want['success']} {short(want['output'])})"))
            fails += 1
    pe_ref, pe_got = ref.get("0x0a:1", {}), results.get("0x0a:1", {})
    rows.append(("0x0a:1", pe_got.get("success", "tx_failed"),
                 pe_got.get("output", pe_got.get("note", "")),
                 f"INFO (reference: {pe_ref.get('success', 'tx_failed')} "
                 f"{short(pe_ref.get('output', ''))})"))
    table(rows)
    if fails:
        print(f"\nFAIL: {fails} engine result(s) diverge from the anvil reference",
              file=sys.stderr)
        sys.exit(1)
    print("\nPASS: engine results are byte-identical to the anvil reference (0x02-0x09)")
PYEOF
