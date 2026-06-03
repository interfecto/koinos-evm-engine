// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;

// L0d de-risk probe: replicates the ONLY novel resource path in createPool — an
// internal CREATE2 that deposits a pool-sized (22,142-byte) runtime — while
// carrying only ~6 bytes of calldata, isolating the state-deposit cost from the
// large-calldata deploy cost (already proven by V2 Router02). Child initcode
// 0x61567e6000f3 = PUSH2 0x567e; PUSH1 0; RETURN -> returns 22142 zero bytes.
contract CreateProbe {
    address public child;
    uint256 public childSize;

    constructor() {
        bytes memory code = hex"61567e6000f3";
        address c;
        assembly {
            c := create2(0, add(code, 0x20), mload(code), 1)
        }
        require(c != address(0), "create2 failed");
        uint256 sz;
        assembly { sz := extcodesize(c) }
        require(sz == 22142, "wrong child size");
        child = c;
        childSize = sz;
    }
}
