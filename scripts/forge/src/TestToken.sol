// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ERC20} from "openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";

contract TestToken is ERC20 {
    constructor(string memory name_, string memory symbol_, address initialHolder, uint256 initialSupply)
        ERC20(name_, symbol_)
    {
        _mint(initialHolder, initialSupply);
    }
}
