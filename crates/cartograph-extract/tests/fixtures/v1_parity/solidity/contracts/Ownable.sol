// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

abstract contract Ownable {
  address internal owner;

  error NotOwner(address caller);

  constructor() {
    owner = msg.sender;
  }

  modifier onlyOwner() {
    require(msg.sender == owner, "not owner");
    _;
  }

  function transferOwnership(address next) public virtual onlyOwner {
    owner = next;
  }
}
