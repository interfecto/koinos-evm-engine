// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;

// Import both so forge emits standalone bytecode for each under the v3 profile
// (runs=800, bytecode_hash=none). Importing only the Factory leaves the Pool
// artifact empty (it'd exist only embedded in the Factory's creation code).
import "@uniswap/v3-core/contracts/UniswapV3Factory.sol";
import "@uniswap/v3-core/contracts/UniswapV3Pool.sol";
