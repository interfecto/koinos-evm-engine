#!/bin/bash
# Step 2 on-chain verification (run AFTER rebuilding + restarting the proxy).
# Proves the pending-nonce fix: simulate exactly what MetaMask does — query the
# "pending" nonce, send, query again, send — and assert BOTH back-to-back txs land 0x1.
# Before the fix, the 2nd "pending" query returned the stale on-chain nonce (0), the
# wallet reused nonce 0, and the engine rejected the 2nd tx.
set -uo pipefail
RPC="${RPC:-http://localhost:8545}"
call() { curl -s -m 25 "$RPC" -H 'Content-Type: application/json' -d "$1"; }
txcount() { # addr, tag
  call "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionCount\",\"params\":[\"$1\",\"$2\"]}" \
    | sed -E 's/.*"result":"(0x[0-9a-f]+)".*/\1/'
}
send() { call "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendRawTransaction\",\"params\":[\"$1\"]}"; }
receipt() { call "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$1\"]}"; }
hex2dec() { printf "%d" "$1"; }
wait_status() { # hash -> echoes status
  for i in $(seq 1 40); do
    R=$(receipt "$1"); if echo "$R" | grep -q '"status"'; then echo "$R" | sed -E 's/.*"status":"(0x[0-9a-f]+)".*/\1/'; return; fi
    sleep 3
  done; echo "TIMEOUT"
}

KEY="${TEST_KEY:?set TEST_KEY to a throwaway privkey}"
ADDR=$(cast wallet address --private-key "$KEY")
echo "test account: $ADDR (fresh, on-chain nonce 0)"
echo ""

echo "════ 1. pending nonce BEFORE any send ════"
P0=$(txcount "$ADDR" pending); echo "  getTransactionCount(pending) = $P0  (expect 0x0)"

echo "════ 2. send tx #1 using that nonce ($(hex2dec ${P0:-0x0})) ════"
TX1=$(cast mktx --private-key "$KEY" --chain 42069 --nonce "$(hex2dec ${P0:-0x0})" --gas-limit 100000 --gas-price 0 --legacy --value 0 "$ADDR" 2>/dev/null)
H1=$(send "$TX1" | sed -E 's/.*"result":"(0x[0-9a-f]+)".*/\1/'); echo "  tx#1 hash = $H1"

echo "════ 3. pending nonce immediately AFTER send #1 (THE FIX) ════"
P1=$(txcount "$ADDR" pending)
L1=$(txcount "$ADDR" latest)
echo "  getTransactionCount(pending) = $P1   (expect 0x1 — was 0x0 before fix)"
echo "  getTransactionCount(latest)  = $L1   (expect 0x0 — tx not yet committed)"
if [ "$(hex2dec ${P1:-0x0})" -eq "$(( $(hex2dec ${P0:-0x0}) + 1 ))" ]; then
  echo "  ✅ pending advanced by 1 while in-flight"
else
  echo "  ❌ pending did NOT advance ($P1) — a wallet would reuse the nonce"
fi

echo "════ 4. send tx #2 using the new pending nonce ($(hex2dec ${P1:-0x1})) ════"
TX2=$(cast mktx --private-key "$KEY" --chain 42069 --nonce "$(hex2dec ${P1:-0x1})" --gas-limit 100000 --gas-price 0 --legacy --value 0 "$ADDR" 2>/dev/null)
H2=$(send "$TX2" | sed -E 's/.*"result":"(0x[0-9a-f]+)".*/\1/'); echo "  tx#2 hash = $H2"

echo "════ 5. both receipts must be 0x1 (both back-to-back txs landed) ════"
S1=$(wait_status "$H1"); echo "  tx#1 status = $S1"
S2=$(wait_status "$H2"); echo "  tx#2 status = $S2"

echo "════ 6. final nonces after commit (stale pending entry should self-heal) ════"
LF=$(txcount "$ADDR" latest); PF=$(txcount "$ADDR" pending)
echo "  latest = $LF , pending = $PF  (expect both 0x2)"

echo ""
echo "════ SUMMARY ════"
echo "  pending after 1 send: $P1 (want 0x1)"
echo "  tx#1 status: $S1 ; tx#2 status: $S2 (want 0x1 / 0x1)"
echo "  final latest/pending: $LF / $PF (want 0x2 / 0x2)"
if [ "$P1" = "0x1" ] && [ "$S1" = "0x1" ] && [ "$S2" = "0x1" ]; then
  echo "  ✅ STEP 2 VERIFIED: back-to-back txs both land; pending nonce tracks in-flight"
else
  echo "  ❌ STEP 2 FAILED — inspect above"
fi
