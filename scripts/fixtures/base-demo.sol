// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.30;
// Deliberately minimal local-EVM fixtures. These are not Circle or OP Stack implementations.
contract DemoUsdc {
    mapping(address => uint256) public balanceOf;
    event Transfer(address indexed from, address indexed to, uint256 value);
    function mint(address to, uint256 amount) external { balanceOf[to] += amount; }
    function transfer(address to, uint256 amount) external returns (bool) {
        require(balanceOf[msg.sender] >= amount, "balance");
        balanceOf[msg.sender] -= amount; balanceOf[to] += amount;
        emit Transfer(msg.sender, to, amount); return true;
    }
}
contract DemoOracle {
    function getL1Fee(bytes memory) external pure returns (uint256) { return 1000; }
    function getOperatorFee(uint256) external pure returns (uint256) { return 1; }
}
