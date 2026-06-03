// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ERC20} from "openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";

contract MyToken is ERC20 {
    constructor(address initialHolder, uint256 initialSupply)
        ERC20("Koinos EVM Test", "KEVM")
    {
        _mint(initialHolder, initialSupply);
    }
}
