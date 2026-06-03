#!/bin/bash
# Step 1 on-chain verification (run AFTER engine redeploy).
# Proves: (a) V3/V2 state intact, (b) bad-nonce tx now reports status 0x0,
#         (c) a correct tx still reports status 0x1 (no regression).
set -uo pipefail
RPC="${RPC:-http://localhost:8545}"
call() { curl -s -m 25 "$RPC" -H 'Content-Type: application/json' -d "$1"; }
ethcall() { call "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$1\",\"data\":\"$2\"},\"latest\"]}"; }
send() { call "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_sendRawTransaction\",\"params\":[\"$1\"]}"; }
receipt() { call "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getTransactionReceipt\",\"params\":[\"$1\"]}"; }

POOL3000=0xAAE4B5b92F78d758B2ff320Dc5aD77480a9969ee
EXPECT_LIQ_HEX="ec04070811870f2b50"   # 4353721810967816252240, post-P3

echo "════════ A. STATE INTACT CHECK (must be unchanged after redeploy) ════════"
LIQ=$(ethcall $POOL3000 0x1a686502 | sed -E 's/.*"result":"0x0*([0-9a-fA-F]+)".*/\1/')
echo "  V3 pool-3000 liquidity = 0x$LIQ  (expect ...$EXPECT_LIQ_HEX)"
echo "$LIQ" | grep -qi "$EXPECT_LIQ_HEX" && echo "  ✅ state intact" || echo "  ❌ STATE CHANGED — investigate before proceeding"
echo -n "  slot0 (tick/feeProtocol): "; ethcall $POOL3000 0x3850c7bd | sed -E 's/.*"result":"(0x[0-9a-f]{200}).*/\1/' | cut -c1-90; echo

# fresh throwaway EVM account (on-chain nonce 0)
KEY="${TEST_KEY:?set TEST_KEY to a throwaway privkey}"
ADDR=$(cast wallet address --private-key "$KEY")
echo ""
echo "════════ B. BAD-NONCE TX → expect status 0x0 ════════"
echo "  test account $ADDR (on-chain nonce 0; sending nonce 5)"
BADTX=$(cast mktx --private-key "$KEY" --chain 42069 --nonce 5 --gas-limit 100000 --gas-price 0 --legacy --value 0 "$ADDR" 2>/dev/null)
H=$(send "$BADTX" | sed -E 's/.*"result":"(0x[0-9a-f]+)".*/\1/')
echo "  submitted, eth hash = $H"
echo -n "  waiting for receipt"
for i in $(seq 1 40); do
  R=$(receipt "$H"); ST=$(echo "$R" | sed -E 's/.*"status":"(0x[0-9a-f]+)".*/\1/')
  if echo "$R" | grep -q '"status"'; then echo ""; break; fi
  echo -n "."; sleep 3
done
echo "  receipt.status = ${ST:-<none>}"
[ "$ST" = "0x0" ] && echo "  ✅ FIX CONFIRMED: rejected tx reports FAILURE (0x0)" || echo "  ❌ status not 0x0 (got ${ST:-null}) — fix not working"

echo ""
echo "════════ C. CORRECT TX → expect status 0x1 (no regression) ════════"
echo "  sending nonce 0 from $ADDR (engine expects 0 → executes)"
OKTX=$(cast mktx --private-key "$KEY" --chain 42069 --nonce 0 --gas-limit 100000 --gas-price 0 --legacy --value 0 "$ADDR" 2>/dev/null)
H2=$(send "$OKTX" | sed -E 's/.*"result":"(0x[0-9a-f]+)".*/\1/')
echo "  submitted, eth hash = $H2"
echo -n "  waiting for receipt"
for i in $(seq 1 40); do
  R2=$(receipt "$H2"); ST2=$(echo "$R2" | sed -E 's/.*"status":"(0x[0-9a-f]+)".*/\1/')
  if echo "$R2" | grep -q '"status"'; then echo ""; break; fi
  echo -n "."; sleep 3
done
echo "  receipt.status = ${ST2:-<none>}"
[ "$ST2" = "0x1" ] && echo "  ✅ CONTROL OK: valid tx still reports SUCCESS (0x1)" || echo "  ❌ valid tx not 0x1 (got ${ST2:-null}) — REGRESSION"

echo ""
echo "════════ SUMMARY ════════"
echo "  bad-nonce status:  ${ST:-null}  (want 0x0)"
echo "  correct-tx status: ${ST2:-null}  (want 0x1)"
