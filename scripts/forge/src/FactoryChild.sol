// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Child {
    uint256 public value;
    address public factory;
    constructor() {
        factory = msg.sender;
    }
    function setValue(uint256 v) external {
        value = v;
    }
}

contract Factory {
    event Deployed(address child, bytes32 salt);

    function deploy(bytes32 salt) external returns (address child) {
        bytes memory initcode = type(Child).creationCode;
        assembly {
            child := create2(0, add(initcode, 0x20), mload(initcode), salt)
            if iszero(child) { revert(0, 0) }
        }
        emit Deployed(child, salt);
    }

    function computeAddress(bytes32 salt) external view returns (address) {
        bytes32 codeHash = keccak256(type(Child).creationCode);
        bytes32 raw = keccak256(abi.encodePacked(bytes1(0xff), address(this), salt, codeHash));
        return address(uint160(uint256(raw)));
    }

    function callSetValue(address child, uint256 v) external {
        Child(child).setValue(v);
    }
}
