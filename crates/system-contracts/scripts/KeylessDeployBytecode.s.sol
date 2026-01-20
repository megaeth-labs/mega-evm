// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {KeylessDeploy} from "../contracts/KeylessDeploy.sol";

/// @title SaveKeylessDeployBytecode
/// @notice Script to deploy KeylessDeploy contract and save its deployed bytecode to a file
/// @dev Run with: forge script scripts/KeylessDeployBytecode.s.sol:SaveKeylessDeployBytecode --sig "run()"
contract SaveKeylessDeployBytecode is Script {
    function run() public {
        vm.startBroadcast();

        // Deploy the KeylessDeploy contract (no constructor arguments)
        KeylessDeploy keylessDeploy = new KeylessDeploy();

        vm.stopBroadcast();

        // Get the deployed bytecode and version
        bytes memory deployedBytecode = address(keylessDeploy).code;
        string memory version = keylessDeploy.version();

        // Calculate code hash
        bytes32 codeHash = keccak256(deployedBytecode);
        string memory bytecodeHex = vm.toString(deployedBytecode);

        // Write to a JSON file with metadata
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
        vm.writeFile("artifacts/KeylessDeploy.json", json);
    }
}
