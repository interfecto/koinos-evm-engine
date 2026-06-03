// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;

import "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import "@uniswap/v3-core/contracts/interfaces/callback/IUniswapV3MintCallback.sol";
import "@uniswap/v3-core/contracts/interfaces/IERC20Minimal.sol";

// Minimal liquidity provider for a single UniswapV3Pool. Holds token0/token1 and
// pays the pool from its own balance inside the mint callback. msg.sender is
// checked against the bound pool so a funded helper cannot be drained by an
// arbitrary caller forging the callback.
contract V3Minter is IUniswapV3MintCallback {
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

    // onlyOwner: the wrapper spends this contract's funded balance, so leaving it
    // open would let anyone mint a position to themselves on our funds.
    function mint(address recipient, int24 tickLower, int24 tickUpper, uint128 amount)
        external
        returns (uint256 amount0, uint256 amount1)
    {
        require(msg.sender == owner, "not owner");
        (amount0, amount1) = IUniswapV3Pool(pool).mint(recipient, tickLower, tickUpper, amount, "");
    }

    // burn reduces the position's liquidity and credits the principal (+ any accrued
    // fees) to the position's tokensOwed; nothing is transferred until collect().
    // onlyOwner: the position is keyed by THIS contract (msg.sender at mint), so only
    // this contract can burn it; we still gate the wrapper to the deployer.
    function burn(int24 tickLower, int24 tickUpper, uint128 amount)
        external
        returns (uint256 amount0, uint256 amount1)
    {
        require(msg.sender == owner, "not owner");
        (amount0, amount1) = IUniswapV3Pool(pool).burn(tickLower, tickUpper, amount);
    }

    // collect withdraws up to (amount0Requested, amount1Requested) of the position's
    // tokensOwed to `recipient`. onlyOwner so an arbitrary caller can't redirect our
    // owed balance to themselves.
    function collect(
        address recipient,
        int24 tickLower,
        int24 tickUpper,
        uint128 amount0Requested,
        uint128 amount1Requested
    ) external returns (uint128 amount0, uint128 amount1) {
        require(msg.sender == owner, "not owner");
        (amount0, amount1) = IUniswapV3Pool(pool).collect(
            recipient, tickLower, tickUpper, amount0Requested, amount1Requested
        );
    }

    function uniswapV3MintCallback(uint256 amount0Owed, uint256 amount1Owed, bytes calldata) external override {
        require(msg.sender == pool, "not pool");
        if (amount0Owed > 0) require(IERC20Minimal(token0).transfer(pool, amount0Owed), "pay0");
        if (amount1Owed > 0) require(IERC20Minimal(token1).transfer(pool, amount1Owed), "pay1");
    }
}
