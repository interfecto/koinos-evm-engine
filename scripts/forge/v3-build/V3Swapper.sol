// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;

import "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import "@uniswap/v3-core/contracts/interfaces/callback/IUniswapV3SwapCallback.sol";
import "@uniswap/v3-core/contracts/interfaces/IERC20Minimal.sol";

// Minimal swapper for a single UniswapV3Pool. Pays the input token to the pool
// from its own balance inside the swap callback; output goes to `recipient`.
// msg.sender is checked against the bound pool so a funded helper cannot be
// drained by an arbitrary caller forging the callback.
contract V3Swapper is IUniswapV3SwapCallback {
    address public immutable pool;
    address public immutable token0;
    address public immutable token1;
    address public immutable owner;

    constructor(address _pool) {
        pool = _pool;
        owner = msg.sender;
        token0 = IUniswapV3Pool(_pool).token0();
        token1 = IUniswapV3Pool(_pool).token1();
    }

    // onlyOwner: the wrapper pays the input token from this contract's funded
    // balance, so leaving it open would let anyone swap out our funds to themselves.
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96)
        external
        returns (int256 amount0, int256 amount1)
    {
        require(msg.sender == owner, "not owner");
        (amount0, amount1) = IUniswapV3Pool(pool).swap(recipient, zeroForOne, amountSpecified, sqrtPriceLimitX96, "");
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external override {
        require(msg.sender == pool, "not pool");
        if (amount0Delta > 0) require(IERC20Minimal(token0).transfer(pool, uint256(amount0Delta)), "pay0");
        if (amount1Delta > 0) require(IERC20Minimal(token1).transfer(pool, uint256(amount1Delta)), "pay1");
    }
}
