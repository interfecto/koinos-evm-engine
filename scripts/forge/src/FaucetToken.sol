// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ERC20} from "openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";

/// @title FaucetToken — TESTNET-ONLY mintable ERC-20.
/// @notice `mint` is intentionally PUBLIC and UNRESTRICTED so anyone can self-serve
///         test tokens from the Koinos EVM swap demo (the relay operator pays gas).
///         NEVER deploy an open-mint token to a network where its supply has value.
contract FaucetToken is ERC20 {
    constructor(string memory name_, string memory symbol_, address initialHolder, uint256 initialSupply)
        ERC20(name_, symbol_)
    {
        if (initialSupply > 0) {
            _mint(initialHolder, initialSupply);
        }
    }

    /// @notice Public faucet mint. Testnet only — see contract notice.
    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}
