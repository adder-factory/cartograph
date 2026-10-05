// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import "./Ownable.sol";
import {IToken} from "./interfaces/IToken.sol";
import * as Math from "./lib/SafeMath.sol";
import "./lib/SafeMath.sol" as SM;

contract Token is Ownable, IToken {
  using SafeMath for uint256;

  enum State { Active, Paused, Closed }

  struct Holder {
    address account;
    uint256 balance;
  }

  uint256 public total;
  State private state;
  mapping(address => uint256) private balances;
  Holder[] internal holders;

  event Minted(address indexed to, uint256 amount);

  modifier whenActive() {
    require(state == State.Active, "inactive");
    _;
  }

  function transfer(address to, uint256 amount) external override whenActive returns (bool) {
    _move(msg.sender, to, amount);
    emit Transfer(msg.sender, to, amount);
    return true;
  }

  function balanceOf(address who) external view override returns (uint256) {
    return balances[who];
  }

  function mint(address to, uint256 amount) public onlyOwner {
    uint256 next = total.add(amount);
    total = next;
    balances[to] = SafeMath.add(balances[to], amount);
    holders.push(Holder(to, amount));
    emit Minted(to, amount);
  }

  function _move(address from, address to, uint256 amount) private {
    balances[from] = balances[from] - amount;
    balances[to] = balances[to] + amount;
  }

  function pause() internal {
    state = State.Paused;
  }

  receive() external payable {}

  fallback() external payable {}
}
