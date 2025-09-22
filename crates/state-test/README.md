# MegaEVM state test

The `state-test` is a fork of `revme` in the official `revm` repository. 

The changes made to run `execution-specification-tests` on MegaEVM is: 
- `MegaTransaction`'s `enveloped_tx` is always set to `Some(vec![].into())` so that there is no L1 data fee induced. 
- State changes to the `BaseFeeVault` (`0x4200000000000000000000000000000000000019`) are pruned after transaction execution. 
- The EVM spec of all Ethereum's official test cases are forced to be `MegaSpecId::EQUIVALENCE`, which is equivalent to `SpecId::PRAGUE`. 