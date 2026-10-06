// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;
import "./Token.sol";

contract Vault {
  struct Entry { uint amount; }
  uint public total;
  Token public token;

  constructor(address tokenAddress) {
    token = Token(payable(tokenAddress));
  }

  function helper(uint amount) private returns (uint) {
    return amount * 2;
  }

  function deposit(uint amount) public returns (bool) {
    uint doubled = helper(amount);
    total += doubled;
    token.mint(address(this), doubled);
    return doubled > 0;
  }

  function spawn() external returns (address) {
    Token fresh = new Token();
    return address(fresh);
  }
}
