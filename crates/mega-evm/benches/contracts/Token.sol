// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// A minimal ERC-20 transfer path for `benches/block.rs`: two balance reads, two balance writes
/// and one `Transfer` log, the shape of OpenZeppelin's `_update` without allowances or hooks.
///
/// The bench embeds the runtime code this source compiles to:
///
///     solc --optimize --optimize-runs 200 --metadata-hash none --bin-runtime Token.sol
///
/// with solc 0.8.30. Without a metadata hash the output does not depend on the source's path.
contract Token {
    mapping(address => uint256) public balanceOf;

    event Transfer(address indexed from, address indexed to, uint256 value);

    function transfer(address to, uint256 value) external returns (bool) {
        uint256 balance = balanceOf[msg.sender];
        require(balance >= value, "balance");
        unchecked {
            balanceOf[msg.sender] = balance - value;
        }
        balanceOf[to] += value;
        emit Transfer(msg.sender, to, value);
        return true;
    }
}
