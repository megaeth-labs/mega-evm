// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {RemainingComputeGas} from "../contracts/RemainingComputeGas.sol";

/// @title SaveRemainingComputeGasBytecode
/// @notice Script to deploy RemainingComputeGas contract and save deployed bytecode to a file
contract SaveRemainingComputeGasBytecode is Script {
    function run() public {
        vm.startBroadcast();

        RemainingComputeGas contractImpl = new RemainingComputeGas();

        vm.stopBroadcast();

        bytes memory deployedBytecode = address(contractImpl).code;
        string memory version = contractImpl.version();
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
        vm.writeFile("artifacts/RemainingComputeGas.json", json);
    }
}
