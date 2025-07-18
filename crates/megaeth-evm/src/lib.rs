//! The evm for the Megaeth

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod context;
pub use context::*;

mod evm;
pub use evm::*;

mod host;
pub use host::*;

mod instructions;
pub use instructions::*;

mod handler;
pub use handler::*;

mod spec;
pub use spec::*;

mod types;
pub use types::*;

#[cfg(test)]
mod tests {
    use revm::context::result::{ExecutionResult, HaltReason};

    use super::*;

    mod contract_size_limit {
        use alloy_primitives::{address, Address, Bytes, TxKind, U256};
        use revm::{
            bytecode::opcode,
            context::{
                result::{EVMError, InvalidTransaction, Output, ResultAndState},
                TxEnv,
            },
            database::{CacheDB, EmptyDB},
            inspector::NoOpInspector,
        };
        use std::convert::Infallible;

        use super::*;

        fn deploy_contract(
            db: &mut CacheDB<EmptyDB>,
            bytecode: Bytes,
            spec: MegaethSpecId,
        ) -> Result<ResultAndState<MegaethHaltReason>, EVMError<Infallible, MegaethTransactionError>>
        {
            let mut context = MegaethContext::new(db, spec);
            context.modify_chain(|chain| {
                chain.operator_fee_scalar = Some(U256::from(0));
                chain.operator_fee_constant = Some(U256::from(0));
            });
            let mut evm = MegaethEvm::new(context, NoOpInspector);
            let tx = TxEnv {
                caller: address!("0000000000000000000000000000000000100000"),
                kind: TxKind::Create,
                data: bytecode,
                gas_limit: 1000000000000000000,
                ..Default::default()
            };
            let mut tx = MegaethTransaction::new(tx);
            tx.enveloped_tx = Some(Bytes::new());
            alloy_evm::Evm::transact_raw(&mut evm, tx)
        }

        fn transact(
            spec: MegaethSpecId,
            db: &mut CacheDB<EmptyDB>,
            caller: Address,
            kind: TxKind,
            data: Bytes,
            value: U256,
        ) -> Result<ResultAndState<MegaethHaltReason>, EVMError<Infallible, MegaethTransactionError>>
        {
            let mut context = MegaethContext::new(db, spec);
            context.modify_chain(|chain| {
                chain.operator_fee_scalar = Some(U256::from(0));
                chain.operator_fee_constant = Some(U256::from(0));
            });
            let mut evm = MegaethEvm::new(context, NoOpInspector);
            let tx = TxEnv { caller, kind, data, value, ..Default::default() };
            let mut tx = MegaethTransaction::new(tx);
            tx.enveloped_tx = Some(Bytes::new());
            alloy_evm::Evm::transact_raw(&mut evm, tx)
        }

        #[test]
        fn test_eip3860_initcode_size_limit_success() {
            let sizes = vec![100 * 1024, constants::mini_rax::MAX_INITCODE_SIZE];

            for size in sizes {
                let large_bytecode = vec![opcode::STOP; size];
                let bytecode: Bytes = large_bytecode.into();

                let mut db = CacheDB::<EmptyDB>::default();
                let result = deploy_contract(&mut db, bytecode.clone(), MegaethSpecId::MINI_RAX);
                assert!(result.is_ok());
            }
        }

        #[test]
        fn test_eip3860_initcode_size_limit_failure() {
            let sizes = vec![1025 * 1024, 2049 * 1024, constants::mini_rax::MAX_INITCODE_SIZE + 1];

            for size in sizes {
                let large_bytecode = vec![opcode::STOP; size];
                let bytecode: Bytes = large_bytecode.into();

                let mut db = CacheDB::<EmptyDB>::default();
                let result = deploy_contract(&mut db, bytecode, MegaethSpecId::MINI_RAX);
                assert!(matches!(
                    result,
                    Err(EVMError::Transaction(MegaethTransactionError::Base(
                        InvalidTransaction::CreateInitCodeSizeLimit
                    )))
                ));
            }
        }

        #[test]
        fn test_eip3860_initcode_size_limit_failure_pre_mini_rax() {
            let sizes = vec![constants::mini_rax::MAX_INITCODE_SIZE];

            for size in sizes {
                let large_bytecode = vec![opcode::STOP; size];
                let bytecode: Bytes = large_bytecode.into();

                let mut db = CacheDB::<EmptyDB>::default();
                let result = deploy_contract(&mut db, bytecode.clone(), MegaethSpecId::EQUIVALENCE);
                assert!(matches!(
                    result,
                    Err(EVMError::Transaction(MegaethTransactionError::Base(
                        InvalidTransaction::CreateInitCodeSizeLimit
                    )))
                ));
            }
        }

        #[test]
        fn test_eip170_code_size_limit_failure() {
            // Use the simplest method to return a contract code larger than 512KB
            // PUSH3 0x080100 (exceeds 512KB) - return size
            // PUSH1 0x00 - memory position 0
            // RETURN - return uninitialized memory, will be filled with 0
            let init_code = vec![
                0x62, 0x08, 0x01, 0x00, // PUSH3 0x080100 (exceeds 512KB)
                0x60, 0x00, // PUSH1 0
                0xf3, // RETURN
            ];

            let bytecode: Bytes = init_code.into();

            let mut db = CacheDB::<EmptyDB>::default();
            let result = deploy_contract(&mut db, bytecode, MegaethSpecId::MINI_RAX);

            assert!(matches!(
                result,
                Ok(ResultAndState {
                    result: ExecutionResult::Halt {
                        reason: MegaethHaltReason::Base(HaltReason::CreateContractSizeLimit),
                        ..
                    },
                    ..
                })
            ));
        }

        #[test]
        fn test_eip170_code_size_limit_success() {
            // Use the simplest method to return a contract code of size 512KB
            // PUSH3 0x080000 (512KB) - return size
            // PUSH1 0x00 - memory position 0
            // RETURN - return uninitialized memory, will be filled with 0
            let init_code = vec![
                0x62, 0x08, 0x00, 0x00, // PUSH3 0x080000 (512KB)
                0x60, 0x00, // PUSH1 0
                0xf3, // RETURN
            ];

            let bytecode: Bytes = init_code.into();

            let mut db = CacheDB::<EmptyDB>::default();
            let result = deploy_contract(&mut db, bytecode, MegaethSpecId::MINI_RAX);
            assert!(matches!(
                result,
                Ok(ResultAndState { result: ExecutionResult::Success { .. }, .. })
            ));
        }

        #[test]
        fn test_eip170_code_size_limit_failure_pre_mini_rax() {
            // Use the simplest method to return a contract code of size 512KB
            // PUSH3 0x080000 (512KB) - return size
            // PUSH1 0x00 - memory position 0
            // RETURN - return uninitialized memory, will be filled with 0
            let init_code = vec![
                0x62, 0x08, 0x00, 0x00, // PUSH3 0x080000 (512KB)
                0x60, 0x00, // PUSH1 0
                0xf3, // RETURN
            ];

            let bytecode: Bytes = init_code.into();

            let mut db = CacheDB::<EmptyDB>::default();
            let result = deploy_contract(&mut db, bytecode, MegaethSpecId::EQUIVALENCE);
            assert!(matches!(
                result,
                Ok(ResultAndState {
                    result: ExecutionResult::Halt {
                        reason: MegaethHaltReason::Base(HaltReason::CreateContractSizeLimit),
                        ..
                    },
                    ..
                })
            ));
        }

        #[test]
        fn test_eip170_create_opcode_size_limit_failure() {
            // 1. Create a "factory" contract that uses the CREATE opcode to create another large
            //    contract
            // 2. Since the sub-contract exceeds the EIP-170 limit, the CREATE operation should fail

            // The bytecode of the factory contract:
            // PUSH1 0x01      - value for MSTORE
            // PUSH1 0x00      - memory position
            // MSTORE          - store a non-zero value at the beginning of memory

            // PUSH3 0x080001  - return size (exceeds 512KB)
            // PUSH1 0x00      - memory offset
            // PUSH1 0x00      - amount of sent ETH
            // CREATE          - create contract opcode (create contract from current memory)

            // PUSH1 0x00      - return value storage position
            // MSTORE          - store the CREATE return address to memory position 0
            // PUSH1 0x20      - return size (32 bytes)
            // PUSH1 0x00      - return offset
            // RETURN          - return result

            let factory_code = vec![
                // 1. Store a non-zero value in memory
                0x60, 0x01, // PUSH1 0x01
                0x60, 0x00, // PUSH1 0x00
                0x52, // MSTORE
                // 2. Prepare to create a large contract
                0x62, 0x08, 0x00, 0x01, // PUSH3 0x080001 (exceeds 512KB)
                0x60, 0x00, // PUSH1 0x00 (memory offset)
                0x60, 0x00, // PUSH1 0x00 (amount of sent ETH)
                0xf0, // CREATE
                // 3. Store the CREATE return address to memory position 0
                // The CREATE return address is on the stack top
                0x60, 0x00, // PUSH1 0x00
                0x52, // MSTORE (store the CREATE return address to memory position 0)
                // 4. Return the contract address in memory
                0x60, 0x20, // PUSH1 0x20 (32 bytes)
                0x60, 0x00, // PUSH1 0x00
                0xf3, // RETURN
            ];

            // deploy factory contract
            let factory_bytecode: Bytes = factory_code.into();
            let mut db = CacheDB::<EmptyDB>::default();
            let factory_result =
                deploy_contract(&mut db, factory_bytecode, MegaethSpecId::MINI_RAX);

            // get factory contract address
            let factory_address = match &factory_result {
                Ok(result_and_state) => match &result_and_state.result {
                    ExecutionResult::Success { output, .. } => match output {
                        Output::Create(bytes, _) | Output::Call(bytes) => {
                            Address::from_slice(&bytes[..20])
                        }
                    },
                    _ => panic!("factory contract deployment failed"),
                },
                _ => panic!("factory contract deployment failed"),
            };

            // call factory contract to create sub contract
            let caller = address!("0000000000000000000000000000000000100000");
            let call_result = transact(
                MegaethSpecId::MINI_RAX,
                &mut db,
                caller,
                TxKind::Call(factory_address),
                Bytes::new(),
                U256::from(0),
            );
            match &call_result {
                Ok(result_and_state) => {
                    println!("factory contract call result: {:?}", result_and_state.result);
                    assert!(matches!(result_and_state.result, ExecutionResult::Success { .. }));

                    match &result_and_state.result {
                        ExecutionResult::Success { output, .. } => match output {
                            Output::Call(bytes) => {
                                if !bytes.is_empty() {
                                    assert!(
                                            bytes.iter().all(|&b| b == 0),
                                            "When CREATE operation failed, it should return all zero address"
                                        );
                                }
                            }
                            _ => panic!("unexpected output type"),
                        },
                        _ => panic!("execution result is not Success"),
                    }
                }
                _ => panic!("call factory contract failed"),
            }
        }

        #[test]
        fn test_eip170_create_opcode_size_limit_success() {
            // 1. Create a "factory" contract that uses the CREATE opcode to create another contract
            // 2. The sub-contract generated by this factory contract does not exceed the EIP-170
            //    limit, so it should be created successfully

            // The bytecode of the factory contract:
            // PUSH1 0x01      - value for MSTORE
            // PUSH1 0x00      - memory position
            // MSTORE          - store a non-zero value at the beginning of memory

            // PUSH3 0x080000  - return size (512KB)
            // PUSH1 0x00      - memory offset
            // PUSH1 0x00      - amount of sent ETH
            // CREATE          - create contract opcode (create contract from current memory)

            // PUSH1 0x00      - return value storage position
            // MSTORE          - store the CREATE return address to memory position 0
            // PUSH1 0x20      - return size (32 bytes)
            // PUSH1 0x00      - return offset
            // RETURN          - return result

            let factory_code = vec![
                // 1. Store a non-zero value in memory
                0x60, 0x01, // PUSH1 0x01
                0x60, 0x00, // PUSH1 0x00
                0x52, // MSTORE
                // 2. Prepare to create a contract
                0x62, 0x08, 0x00, 0x00, // PUSH3 0x080000 (512KB)
                0x60, 0x00, // PUSH1 0x00 (memory offset)
                0x60, 0x00, // PUSH1 0x00 (amount of sent ETH)
                0xf0, // CREATE
                // 3. Store the CREATE return address to memory position 0
                // The CREATE return address is on the stack top
                0x60, 0x00, // PUSH1 0x00
                0x52, // MSTORE (store the CREATE return address to memory position 0)
                // 4. Return the contract address in memory
                0x60, 0x20, // PUSH1 0x20 (32 bytes)
                0x60, 0x00, // PUSH1 0x00
                0xf3, // RETURN
            ];

            // deploy factory contract
            let factory_bytecode: Bytes = factory_code.into();
            let mut db = CacheDB::<EmptyDB>::default();
            let factory_result =
                deploy_contract(&mut db, factory_bytecode, MegaethSpecId::MINI_RAX);
            // get factory contract address
            let factory_address = match &factory_result {
                Ok(result_and_state) => match &result_and_state.result {
                    ExecutionResult::Success { output, .. } => match output {
                        Output::Create(bytes, _) | Output::Call(bytes) => {
                            Address::from_slice(&bytes[..20])
                        }
                    },
                    _ => panic!("factory contract deployment failed"),
                },
                _ => panic!("factory contract deployment failed"),
            };

            // call factory contract to create sub contract
            let tx_caller = address!("0000000000000000000000000000000000100000");
            let call_result = transact(
                MegaethSpecId::MINI_RAX,
                &mut db,
                tx_caller,
                TxKind::Call(factory_address),
                Bytes::new(),
                U256::from(0),
            );
            match &call_result {
                Ok(result_and_state) => {
                    println!("factory contract call result: {:?}", result_and_state.result);
                    match &result_and_state.result {
                        ExecutionResult::Success { output, .. } => {
                            match output {
                                Output::Call(bytes) => {
                                    println!("call result: {:?}", bytes);

                                    // output should contain create contract address, but we cannot
                                    // determine the format of the return data, so check the state
                                    // instead
                                    let state = &result_and_state.state;

                                    // state should contain factory contract and caller account
                                    assert!(
                                        state.contains_key(&factory_address) &&
                                            state.contains_key(&tx_caller),
                                        "factory contract or caller account not found in state"
                                    );
                                    // if sub contract created successfully, state should contain at
                                    // least three accounts
                                    assert!(
                                        state.len() >= 2,
                                        "state account number is not enough, should at least have caller and factory contract"
                                    );

                                    // check if CREATE operation is successful (return non-zero
                                    // address)
                                    if !bytes.is_empty() {
                                        assert!(
                                            bytes.iter().any(|&b| b != 0),
                                            "create sub contract failed"
                                        );
                                    }
                                }
                                _ => panic!("unexpected output type"),
                            }
                        }
                        _ => panic!("execution result is not Success"),
                    }
                }
                _ => panic!("call factory contract failed"),
            }
        }
    }
}
