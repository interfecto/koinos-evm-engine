// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity =0.7.6;
pragma abicoder v2;
// Build target: the REAL NonfungiblePositionManager. The NFT SVG descriptor is only used by
// tokenURI(); minting/liquidity don't need it, so we can deploy with a dummy descriptor address.
import "@uniswap/v3-periphery/contracts/NonfungiblePositionManager.sol";
