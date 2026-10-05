// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IToken {
  event Transfer(address indexed from, address indexed to, uint256 value);

  function transfer(address to, uint256 amount) external returns (bool);
  function balanceOf(address owner) external view returns (uint256);
}
