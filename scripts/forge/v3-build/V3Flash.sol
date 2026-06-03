// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;

import "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import "@uniswap/v3-core/contracts/interfaces/callback/IUniswapV3FlashCallback.sol";
import "@uniswap/v3-core/contracts/interfaces/IERC20Minimal.sol";

// Minimal flash borrower for a single UniswapV3Pool. Borrows (amount0, amount1) and repays
// principal + fee inside the callback from this contract's own funded balance. msg.sender is
// checked against the bound pool so a funded helper cannot be drained by a forged callback.
contract V3Flash is IUniswapV3FlashCallback {
    address public immutable pool;
    address public immutable token0;
    address public immutable token1;
    address public immutable owner;

    // Borrowed principal for the in-flight flash, so the callback repays EXACTLY principal+fee
    // (not the whole balance — overpaying would distort the pool's measured fee delta).
    uint256 private _a0;
    uint256 private _a1;

    constructor(address _pool) {
        pool = _pool;
        owner = msg.sender;
        token0 = IUniswapV3Pool(_pool).token0();
        token1 = IUniswapV3Pool(_pool).token1();
    }

    // onlyOwner: the wrapper repays from this contract's funded balance, so leaving it open would
    // let anyone trigger a flash that spends our funds on the fee.
    function flash(uint256 amount0, uint256 amount1) external {
        require(msg.sender == owner, "not owner");
        _a0 = amount0;
        _a1 = amount1;
        IUniswapV3Pool(pool).flash(address(this), amount0, amount1, "");
        _a0 = 0;
        _a1 = 0;
    }

    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata) external override {
        require(msg.sender == pool, "not pool");
        uint256 owed0 = _a0 + fee0;
        uint256 owed1 = _a1 + fee1;
        if (owed0 > 0) require(IERC20Minimal(token0).transfer(pool, owed0), "repay0");
        if (owed1 > 0) require(IERC20Minimal(token1).transfer(pool, owed1), "repay1");
    }
}
