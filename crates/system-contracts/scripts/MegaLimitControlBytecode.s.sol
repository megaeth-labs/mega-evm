// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

import {Script} from "forge-std/Script.sol";
import {MegaLimitControl} from "../contracts/MegaLimitControl.sol";

/// @title SaveMegaLimitControlBytecode
/// @notice Script to deploy MegaLimitControl contract and save deployed bytecode to a file
contract SaveMegaLimitControlBytecode is Script {
    function run() public {
        vm.startBroadcast();

        MegaLimitControl contractImpl = new MegaLimitControl();

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
        vm.writeFile("artifacts/MegaLimitControl.json", json);
    }
}
