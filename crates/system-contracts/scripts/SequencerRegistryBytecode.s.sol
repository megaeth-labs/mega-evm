// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {SequencerRegistry} from "../contracts/SequencerRegistry.sol";

/// @title SaveSequencerRegistryBytecode
/// @notice Script to deploy SequencerRegistry contract and save deployed bytecode to a file
contract SaveSequencerRegistryBytecode is Script {
    function run() public {
        vm.startBroadcast();

        SequencerRegistry contractImpl = new SequencerRegistry();

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
        vm.writeFile("artifacts/SequencerRegistry.json", json);
    }
}
