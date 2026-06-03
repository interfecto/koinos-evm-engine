// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;
pragma abicoder v2;
// Thin build target: pull in the REAL Uniswap V3 periphery SwapRouter (+ its dep graph)
// so forge emits its artifact/bytecode for deployment. No custom logic.
import "@uniswap/v3-periphery/contracts/SwapRouter.sol";
