// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {MegaAccessControl} from "../contracts/MegaAccessControl.sol";

/// @title SaveMegaAccessControlBytecode
/// @notice Script to deploy MegaAccessControl contract and save its deployed bytecode to a file
/// @dev Run with: forge script scripts/MegaAccessControlBytecode.s.sol:SaveMegaAccessControlBytecode --sig "run()"
contract SaveMegaAccessControlBytecode is Script {
    function run() public {
        vm.startBroadcast();

        // Deploy the MegaAccessControl contract (no constructor arguments)
        MegaAccessControl megaLimitControl = new MegaAccessControl();

        vm.stopBroadcast();

        // Get the deployed bytecode and version
        bytes memory deployedBytecode = address(megaLimitControl).code;
        string memory version = megaLimitControl.version();

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
        vm.writeFile("artifacts/MegaAccessControl.json", json);
    }
}
