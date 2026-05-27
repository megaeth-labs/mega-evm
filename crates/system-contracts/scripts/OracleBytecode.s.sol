// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {Oracle} from "../contracts/Oracle.sol";

/// @title SaveOracleBytecode
/// @notice Script to deploy Oracle contract and save its deployed bytecode to a file.
/// @dev v2.0.0 has no constructor parameters — the SequencerRegistry address is a constant.
contract SaveOracleBytecode is Script {
    function run() public {
        vm.startBroadcast();

        Oracle oracle = new Oracle();

        vm.stopBroadcast();

        bytes memory deployedBytecode = address(oracle).code;
        string memory version = oracle.version();
        bytes32 codeHash = keccak256(deployedBytecode);
        string memory bytecodeHex = vm.toString(deployedBytecode);

        string memory json = string.concat(
            "{\n",
            '  "version": "',
            version,
            '",\n',
            '  "bytecodeLength": ',
            vm.toString(deployedBytecode.length),
            ",\n",
            '  "codeHash": "',
            vm.toString(codeHash),
            '",\n',
            '  "deployedBytecode": "',
            bytecodeHex,
            '"\n',
            "}"
        );
        vm.writeFile("artifacts/Oracle.json", json);
    }
}
