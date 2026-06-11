// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title PrecompileProbe — on-chain probe suite for precompiles 0x02–0x0a.
/// @notice ROADMAP §1 steps 2–3: prove that the Koinos EVM engine's precompiles
///         produce byte-identical results to a reference EVM (anvil).
///
///         The engine overrides 0x01–0x04 with Koinos crypto syscalls and inherits
///         0x05–0x09 from revm's Cancun set; 0x0a (KZG point evaluation) is revm's
///         fatal stub (built without c-kzg). Because the engine cannot serve heavy
///         READ calls on the public node, every probe runs inside a COMMITTING
///         transaction and reports its result via the `ProbeResult` EVENT, which is
///         observable in the receipt — never via eth_call return data.
///
///         Each probe `staticcall`s the precompile with a hardcoded test vector
///         (sources cited per vector below) and emits the raw output. No storage
///         is touched; events only.
///
///         IMPORTANT: the 0x0a probe is intentionally NOT part of `probeAll()`.
///         revm 19 registers 0x0a as `fatal_precompile` when built without c-kzg
///         (revm-precompile 16.2.0, src/lib.rs): calling it raises
///         `PrecompileErrors::Fatal`, which aborts the ENTIRE transaction —
///         uncatchable by staticcall. If 0x0a were inside `probeAll()`, the engine
///         would wipe all other probes' events from the receipt. It therefore
///         lives in its own transaction (`probePointEval()`), compared
///         informationally only.
contract PrecompileProbe {
    /// @notice Raw result of one probe vector.
    /// @param precompile precompile address (0x02..0x0a)
    /// @param vector     1-based vector index within that precompile
    /// @param success    staticcall success flag
    /// @param output     raw returndata
    event ProbeResult(uint8 indexed precompile, uint16 indexed vector, bool success, bytes output);

    // ── 0x02 SHA-256 ────────────────────────────────────────────────────────
    // Vector: SHA-256("abc"). Source: NIST FIPS 180-2 example B.1.
    // Expected: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
    // Exercises the Koinos-syscall-backed override (engine precompiles.rs).
    bytes internal constant SHA256_IN = "abc";

    // ── 0x03 RIPEMD-160 ─────────────────────────────────────────────────────
    // Vector: RIPEMD-160("abc"). Source: RIPEMD-160 reference (Dobbertin,
    // Bosselaers, Preneel 1996), test vector "abc".
    // Expected (precompile left-pads the 20-byte digest to 32 bytes):
    // 0000000000000000000000008eb208f7e05d987a9b044a8e98c6b087f15a0bfc
    // Exercises the Koinos-syscall-backed override.
    bytes internal constant RIPEMD160_IN = "abc";

    // ── 0x04 identity ───────────────────────────────────────────────────────
    // Vector: 36-byte ASCII round-trip (crosses a 32-byte word boundary).
    // Expected: output == input. Exercises the Koinos-syscall-backed override.
    bytes internal constant IDENTITY_IN = "Koinos EVM precompile identity probe";

    // ── 0x05 modexp (EIP-198) ───────────────────────────────────────────────
    // Vector 1: the EIP-198 example 3^2 mod 5 == 4, 32-byte padded operands.
    // Input layout: |len(B)=32|len(E)=32|len(M)=32|B=3|E=2|M=5|
    // Expected: 0x...04 (32 bytes).
    bytes internal constant MODEXP1_IN =
        hex"0000000000000000000000000000000000000000000000000000000000000020"
        hex"0000000000000000000000000000000000000000000000000000000000000020"
        hex"0000000000000000000000000000000000000000000000000000000000000020"
        hex"0000000000000000000000000000000000000000000000000000000000000003"
        hex"0000000000000000000000000000000000000000000000000000000000000002"
        hex"0000000000000000000000000000000000000000000000000000000000000005";

    // Vector 2: RSA-shaped, 64-byte (512-bit) base/exp/mod, e = 65537.
    // base = SHA-512("koinos-evm-engine precompile probe base")
    // mod  = SHA-512("koinos-evm-engine precompile probe mod") | 1   (odd)
    // Expected output = pow(base, 65537, mod), 64 bytes (computed with Python
    // pow(); cross-checked against anvil in scripts/shell/verify_precompiles.sh):
    // 671fda505b8b5d4e7a0436765556e7133b89742658a86f57cb00666b06198a0f
    // 4f79cd764b6c614760e5ddfda3e686291a35548d4edf38f61e789a7425081213
    bytes internal constant MODEXP2_IN =
        hex"0000000000000000000000000000000000000000000000000000000000000040"
        hex"0000000000000000000000000000000000000000000000000000000000000040"
        hex"0000000000000000000000000000000000000000000000000000000000000040"
        hex"89479b00f30be3bef767b3d75fb800e6586ef529de250e2d185734a2625b6b6b"
        hex"4574bb7aa271ce59295ef369ea9366ce6707a46203175adc8b6053e5d619ec72"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000010001"
        hex"805e35ea81d4fede066448930479d82b59179b7f5ab3cf60cd0e24e8ea825549"
        hex"44dad45727e4335f1f23605257141752128aa1f495067e1c41aadac1865a39bd";

    // ── 0x06 bn128 add (EIP-196) ────────────────────────────────────────────
    // Vector: the EIP-196 reference vector "chfast1"
    // (go-ethereum core/vm/testdata/precompiles/bn256Add.json).
    // Expected:
    // 2243525c5efd4b9c3d3c45ac0ca3fe4dd85e830a4ce6b65fa1eeaee202839703
    // 301d1d33be6da8e509df21cc35964723180eed7532537db9ae5e7d48f195c915
    bytes internal constant BN128ADD_IN =
        hex"18b18acfb4c2c30276db5411368e7185b311dd124691610c5d3b74034e093dc9"
        hex"063c909c4720840cb5134cb9f59fa749755796819658d32efc0d288198f37266"
        hex"07c2b7f58a84bd6145f00c9c2bc0bb1a187f20ff2c92963a88019e7c6a014eed"
        hex"06614e20c147e940f2d70da3f74c9a17df361706a4485c742bd6788478fa17d7";

    // ── 0x07 bn128 mul (EIP-196) ────────────────────────────────────────────
    // Vector: the EIP-196 reference vector "chfast1"
    // (go-ethereum core/vm/testdata/precompiles/bn256ScalarMul.json).
    // Expected:
    // 070a8d6a982153cae4be29d434e8faef8a47b274a053f5a4ee2a6c9c13c31e5c
    // 031b8ce914eba3a9ffb989f9cdd5b0f01943074bf4f0f315690ec3cec6981afc
    bytes internal constant BN128MUL_IN =
        hex"2bd3e6d0f3b142924f5ca7b49ce5b9d54c4703d7ae5648e61d02268b1a0a9fb7"
        hex"21611ce0a6af85915e2f1d70300909ce2e49dfad4a4619c8390cae66cefdb204"
        hex"00000000000000000000000000000000000000000000000011138ce750fa15c2";

    // ── 0x08 bn128 pairing (EIP-197) ────────────────────────────────────────
    // G2 generator encoding per EIP-197 (imaginary coefficient first):
    //   x_im 198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2
    //   x_re 1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed
    //   y_im 090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b
    //   y_re 12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa

    // Vector 1: single-pair SUCCESS — e(O, G2) with O the G1 point at infinity
    // (encoded (0,0) per EIP-197). The pairing of infinity with anything is the
    // identity in GT, so the check passes. Expected: 0x...01 (32 bytes).
    bytes internal constant PAIRING1_IN =
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2"
        hex"1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed"
        hex"090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b"
        hex"12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa";

    // Vector 2: single INVALID (non-matching) pair — e(G1, G2) with both group
    // generators; well-formed points, but the product is != 1 in GT, so the
    // pairing check FAILS (geth testdata "one_point_fail"). Expected: 0x...00.
    bytes internal constant PAIRING2_IN =
        hex"0000000000000000000000000000000000000000000000000000000000000001"
        hex"0000000000000000000000000000000000000000000000000000000000000002"
        hex"198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2"
        hex"1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed"
        hex"090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b"
        hex"12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa";

    // Vector 3: two-pair MATCH — e(P, Q) * e(-P, Q) == 1 with P = G1 generator,
    // -P = (1, p-2), Q = G2 generator. Unlike vector 1 this runs a full Miller
    // loop + final exponentiation over non-degenerate points. Expected: 0x...01.
    // (p = 21888242871839275222246405745257275088696311157297823662689037894645226208583)
    bytes internal constant PAIRING3_IN =
        hex"0000000000000000000000000000000000000000000000000000000000000001"
        hex"0000000000000000000000000000000000000000000000000000000000000002"
        hex"198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2"
        hex"1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed"
        hex"090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b"
        hex"12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa"
        hex"0000000000000000000000000000000000000000000000000000000000000001"
        hex"30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45"
        hex"198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2"
        hex"1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed"
        hex"090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b"
        hex"12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa";

    // ── 0x09 blake2f (EIP-152) ──────────────────────────────────────────────
    // Vector: EIP-152 test vector 5 — the canonical 12-round compression of the
    // BLAKE2b-512 state over the final "abc" block (rounds=0x0000000c, h = IV
    // xor 0x01010040 in word 0, m = "abc" padded to 128 bytes, t = (3, 0),
    // f = 0x01). 213-byte input.
    // Expected (== BLAKE2b-512("abc")):
    // ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1
    // 7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923
    bytes internal constant BLAKE2F_IN =
        hex"0000000c"
        hex"48c9bdf267e6096a3ba7ca8485ae67bb2bf894fe72f36e3cf1361d5f3af54fa5"
        hex"d182e6ad7f520e511f6c3e2b8c68059b6bbd41fbabd9831f79217e1319cde05b"
        hex"6162630000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0300000000000000"
        hex"0000000000000000"
        hex"01";

    // ── 0x0a KZG point evaluation (EIP-4844) ────────────────────────────────
    // Vector: 192 zero bytes — correct LENGTH, deliberately INVALID content
    // (versioned hash 0x00... cannot match sha256(commitment) with version byte
    // 0x01, so verification must fail everywhere).
    //
    // Behaviors are NOT expected to be equal and are recorded informationally:
    //  * anvil (revm with c-kzg): in-precompile validation error -> the
    //    staticcall returns success=false, empty output, tx succeeds.
    //  * Koinos engine (revm 19 without c-kzg): 0x0a is `fatal_precompile`;
    //    calling it raises PrecompileErrors::Fatal which aborts the WHOLE
    //    transaction (no receipt events at all). That is why this vector is in
    //    its own tx and excluded from probeAll().
    bytes internal constant POINTEVAL_IN =
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000"
        hex"0000000000000000000000000000000000000000000000000000000000000000";

    // ── probes ──────────────────────────────────────────────────────────────

    function probeSha256() external {
        _probe(0x02, 1, SHA256_IN);
    }

    function probeRipemd160() external {
        _probe(0x03, 1, RIPEMD160_IN);
    }

    function probeIdentity() external {
        _probe(0x04, 1, IDENTITY_IN);
    }

    function probeModexp() external {
        _probe(0x05, 1, MODEXP1_IN);
        _probe(0x05, 2, MODEXP2_IN);
    }

    function probeBn128Add() external {
        _probe(0x06, 1, BN128ADD_IN);
    }

    function probeBn128Mul() external {
        _probe(0x07, 1, BN128MUL_IN);
    }

    function probeBn128Pairing() external {
        _probe(0x08, 1, PAIRING1_IN);
        _probe(0x08, 2, PAIRING2_IN);
        _probe(0x08, 3, PAIRING3_IN);
    }

    function probeBlake2f() external {
        _probe(0x09, 1, BLAKE2F_IN);
    }

    /// @notice 0x0a probe. MUST be sent as its own transaction: on the Koinos
    ///         engine the fatal stub aborts the enclosing tx (see header note).
    function probePointEval() external {
        _probe(0x0a, 1, POINTEVAL_IN);
    }

    /// @notice Runs every comparable vector (0x02–0x09) in one transaction.
    ///         0x0a is deliberately excluded — see `probePointEval()`.
    function probeAll() external {
        _probe(0x02, 1, SHA256_IN);
        _probe(0x03, 1, RIPEMD160_IN);
        _probe(0x04, 1, IDENTITY_IN);
        _probe(0x05, 1, MODEXP1_IN);
        _probe(0x05, 2, MODEXP2_IN);
        _probe(0x06, 1, BN128ADD_IN);
        _probe(0x07, 1, BN128MUL_IN);
        _probe(0x08, 1, PAIRING1_IN);
        _probe(0x08, 2, PAIRING2_IN);
        _probe(0x08, 3, PAIRING3_IN);
        _probe(0x09, 1, BLAKE2F_IN);
    }

    /// @dev staticcall the precompile and emit the raw result. Never reverts on
    ///      precompile failure: failure is data, not an error.
    function _probe(uint8 precompile, uint16 vector, bytes memory input) internal {
        (bool ok, bytes memory out) = address(uint160(precompile)).staticcall(input);
        emit ProbeResult(precompile, vector, ok, out);
    }
}
