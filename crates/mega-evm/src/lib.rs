//! The EVM implementation for the `MegaETH`.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(unused_imports)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg_attr(not(feature = "std"), macro_use)]
#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod constants;

mod context;
pub use context::*;

mod block;
pub use block::*;

mod evm;
pub use evm::*;

mod gas;
pub use gas::*;

mod host;
pub use host::*;

mod limit;
pub use limit::*;

mod instructions;
pub use instructions::*;

mod handler;
pub use handler::*;

mod spec;
pub use spec::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod types;
pub use types::*;

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, Bytes, U256};
    use revm::{
        bytecode::opcode::PUSH0,
        context::result::ExecutionResult,
        database::{CacheDB, EmptyDB},
    };

    use super::*;
    use crate::test_utils::*;

    mod contract_size_limit {
        use alloy_primitives::{address, Bytes, U256};
        use core::convert::Infallible;
        use revm::{
            bytecode::opcode::{CREATE, INVALID, ISZERO, JUMPDEST, JUMPI, PUSH1, RETURN, STOP},
            context::result::{EVMError, InvalidTransaction, ResultAndState},
            database::{CacheDB, EmptyDB},
        };

        use super::*;

        fn deploy_contract(
            db: &mut CacheDB<EmptyDB>,
            bytecode: Bytes,
            spec: SpecId,
        ) -> Result<ResultAndState<HaltReason>, EVMError<Infallible, TransactionError>> {
            transact(
                spec,
                db,
                address!("0000000000000000000000000000000000100000"),
                None,
                bytecode,
                U256::ZERO,
            )
        }

        fn initcode_size_limit_test_case(spec: SpecId, initcode_size: usize, success: bool) {
            let large_bytecode = vec![STOP; initcode_size];
            let bytecode: Bytes = large_bytecode.into();
            let mut db = CacheDB::<EmptyDB>::default();
            let result = deploy_contract(&mut db, bytecode, spec);
            if success {
                assert!(result.is_ok());
            } else {
                assert!(matches!(
                    result,
                    Err(EVMError::Transaction(TransactionError::Base(
                        InvalidTransaction::CreateInitCodeSizeLimit
                    )))
                ));
            }
        }

        #[test]
        fn test_eip3860_initcode_size() {
            initcode_size_limit_test_case(
                SpecId::EQUIVALENCE,
                revm::primitives::eip3860::MAX_INITCODE_SIZE,
                true,
            );
            initcode_size_limit_test_case(
                SpecId::EQUIVALENCE,
                constants::mini_rex::MAX_INITCODE_SIZE,
                false,
            );
            initcode_size_limit_test_case(
                SpecId::MINI_REX,
                revm::primitives::eip3860::MAX_INITCODE_SIZE,
                true,
            );
            initcode_size_limit_test_case(
                SpecId::MINI_REX,
                constants::mini_rex::MAX_INITCODE_SIZE,
                true,
            );
            initcode_size_limit_test_case(
                SpecId::MINI_REX,
                constants::mini_rex::MAX_INITCODE_SIZE + 1,
                false,
            );
            initcode_size_limit_test_case(
                SpecId::MINI_REX,
                2 * constants::mini_rex::MAX_INITCODE_SIZE,
                false,
            );
        }

        fn constructor_code(contract_size: usize) -> Bytes {
            let mut init_code = vec![];
            opcode_gen::push_number(&mut init_code, contract_size as u64);
            init_code.extend(vec![PUSH1, 0x00]);
            init_code.push(RETURN);

            init_code = right_pad_bytes(init_code, 32);
            init_code.into()
        }

        fn contract_size_limit_test_case(spec: SpecId, contract_size: usize, success: bool) {
            // Use the simplest method to return a contract code
            let init_code = constructor_code(contract_size);
            let mut db = CacheDB::<EmptyDB>::default();
            let result = deploy_contract(&mut db, init_code, spec);
            if success {
                assert!(result.is_ok());
            } else {
                assert!(matches!(
                    result,
                    Ok(ResultAndState {
                        result: ExecutionResult::Halt {
                            reason: HaltReason::Base(
                                revm::context::result::HaltReason::CreateContractSizeLimit
                            ),
                            ..
                        },
                        ..
                    })
                ));
            }
        }

        #[test]
        fn test_eip170_code_size_limit() {
            contract_size_limit_test_case(
                SpecId::EQUIVALENCE,
                revm::primitives::eip170::MAX_CODE_SIZE,
                true,
            );
            contract_size_limit_test_case(
                SpecId::EQUIVALENCE,
                constants::mini_rex::MAX_CONTRACT_SIZE,
                false,
            );
            contract_size_limit_test_case(
                SpecId::MINI_REX,
                revm::primitives::eip170::MAX_CODE_SIZE,
                true,
            );
            contract_size_limit_test_case(
                SpecId::MINI_REX,
                constants::mini_rex::MAX_CONTRACT_SIZE,
                true,
            );
            contract_size_limit_test_case(
                SpecId::MINI_REX,
                constants::mini_rex::MAX_CONTRACT_SIZE + 1,
                false,
            );
            contract_size_limit_test_case(
                SpecId::MINI_REX,
                2 * constants::mini_rex::MAX_CONTRACT_SIZE,
                false,
            );
        }

        fn contract_factory_code_size_limit_test_case(
            spec: SpecId,
            contract_size: usize,
            success: bool,
        ) {
            // 1. Create a "factory" contract that uses the CREATE opcode to create another large
            //    contract
            // 2. Since the sub-contract exceeds the EIP-170 limit, the CREATE operation should fail

            let mut factory_code = vec![];
            // 1. put contract constructor code in memory
            let init_code = constructor_code(contract_size);
            opcode_gen::store_memory_bytes(&mut factory_code, 0, &init_code);

            // 2. create contract
            opcode_gen::push_number(&mut factory_code, init_code.len() as u64);
            opcode_gen::push_number(&mut factory_code, 0u64);
            opcode_gen::push_number(&mut factory_code, 0u64);
            factory_code.extend(vec![CREATE]);

            // 3. check if the create is successful, if not, jump to INVALID
            factory_code.extend(vec![ISZERO]);
            factory_code.extend(vec![PUSH1, (factory_code.len() + 4) as u8]);
            factory_code.extend(vec![JUMPI]);
            factory_code.extend(vec![STOP]);
            factory_code.extend(vec![JUMPDEST, INVALID]);

            let caller = address!("0000000000000000000000000000000000100000");
            let mut db = CacheDB::<EmptyDB>::default();
            let factory_address = address!("0000000000000000000000000000000000100001");
            set_account_code(&mut db, factory_address, factory_code.clone().into());
            let result = transact(
                spec,
                &mut db,
                caller,
                Some(factory_address),
                Bytes::default(),
                U256::ZERO,
            );
            if success {
                assert!(matches!(
                    result,
                    Ok(ResultAndState { result: ExecutionResult::Success { .. }, .. })
                ));
            } else {
                assert!(matches!(
                    result,
                    Ok(ResultAndState {
                        result: ExecutionResult::Halt {
                            reason: HaltReason::Base(
                                revm::context::result::HaltReason::InvalidFEOpcode
                            ),
                            ..
                        },
                        ..
                    })
                ));
            }
        }

        #[test]
        fn test_eip170_create_opcode_size_limit() {
            contract_factory_code_size_limit_test_case(
                SpecId::EQUIVALENCE,
                revm::primitives::eip170::MAX_CODE_SIZE,
                true,
            );
            contract_factory_code_size_limit_test_case(
                SpecId::EQUIVALENCE,
                revm::primitives::eip170::MAX_CODE_SIZE + 1,
                false,
            );
            contract_factory_code_size_limit_test_case(
                SpecId::MINI_REX,
                revm::primitives::eip170::MAX_CODE_SIZE,
                true,
            );
            contract_factory_code_size_limit_test_case(
                SpecId::MINI_REX,
                revm::primitives::eip170::MAX_CODE_SIZE + 1,
                true,
            );
            contract_factory_code_size_limit_test_case(
                SpecId::MINI_REX,
                constants::mini_rex::MAX_CONTRACT_SIZE,
                true,
            );
            contract_factory_code_size_limit_test_case(
                SpecId::MINI_REX,
                constants::mini_rex::MAX_CONTRACT_SIZE + 1,
                false,
            );
        }
    }

    mod block_env_access_tracking {
        use alloy_primitives::{address, Bytes, U256};
        use revm::{
            bytecode::opcode::{
                BASEFEE, BLOCKHASH, CALLER, CHAINID, COINBASE, DIFFICULTY, GASLIMIT, GASPRICE,
                NUMBER, ORIGIN, POP, PUSH1, STOP, TIMESTAMP,
            },
            context::ContextTr,
            database::{CacheDB, EmptyDB},
            inspector::NoOpInspector,
        };

        use super::*;

        /// Test that EVM correctly tracks block environment access
        #[test]
        fn test_block_env_tracking_with_evm() {
            let test_cases = vec![
                ("NUMBER", vec![NUMBER, STOP], Some(BlockEnvAccess::BLOCK_NUMBER)),
                ("TIMESTAMP", vec![TIMESTAMP, STOP], Some(BlockEnvAccess::TIMESTAMP)),
                ("COINBASE", vec![COINBASE, STOP], Some(BlockEnvAccess::COINBASE)),
                ("DIFFICULTY", vec![DIFFICULTY, STOP], Some(BlockEnvAccess::PREV_RANDAO)),
                ("GASLIMIT", vec![GASLIMIT, STOP], Some(BlockEnvAccess::GAS_LIMIT)),
                ("BASEFEE", vec![BASEFEE, STOP], Some(BlockEnvAccess::BASE_FEE)),
                ("BLOCKHASH", vec![PUSH1, 0x05, BLOCKHASH, STOP], Some(BlockEnvAccess::BLOCK_HASH)),
                // Non-block env opcodes
                ("CALLER", vec![CALLER, STOP], None),
                ("ORIGIN", vec![ORIGIN, STOP], None),
                ("GASPRICE", vec![GASPRICE, STOP], None),
                ("CHAINID", vec![CHAINID, STOP], None),
            ];

            for (opcode_name, bytecode, expected_access_type) in test_cases {
                println!("Testing opcode: {}", opcode_name);

                let mut db = CacheDB::<EmptyDB>::default();
                let contract_address = address!("0000000000000000000000000000000000100001");
                set_account_code(&mut db, contract_address, bytecode.into());

                let mut context = Context::new(db, SpecId::MINI_REX, NoOpOracle);
                // Configure L1BlockInfo to avoid operator fee scalar panic
                context.chain_mut().operator_fee_scalar = Some(U256::from(0));
                context.chain_mut().operator_fee_constant = Some(U256::from(0));

                // Set block number to allow BLOCKHASH to work properly
                use revm::context::{BlockEnv, ContextSetters};
                let block_env = BlockEnv { number: U256::from(10), ..Default::default() };
                context.set_block(block_env);

                let mut evm = Evm::new(context, NoOpInspector);

                // Ensure we start with no block env access
                assert!(
                    evm.get_block_env_accesses().is_empty(),
                    "EVM should start with no block env access for {}",
                    opcode_name
                );
                assert_eq!(
                    evm.get_block_env_accesses().count_accessed(),
                    0,
                    "EVM should start with no access count for {}",
                    opcode_name
                );

                // Create transaction
                let tx = Transaction {
                    base: revm::context::TxEnv {
                        caller: address!("0000000000000000000000000000000000100000"),
                        kind: revm::primitives::TxKind::Call(contract_address),
                        data: Bytes::default(),
                        value: U256::ZERO,
                        gas_limit: 1000000000000000000u64,
                        ..Default::default()
                    },
                    ..Default::default()
                };

                // Execute transaction
                let result = alloy_evm::Evm::transact_raw(&mut evm, tx);
                assert!(result.is_ok(), "Transaction should succeed for {}", opcode_name);

                // Check if block env was accessed
                match expected_access_type {
                    Some(access_type) => {
                        assert!(
                            !evm.get_block_env_accesses().is_empty(),
                            "Transaction should have accessed block env for {}",
                            opcode_name
                        );
                        assert!(
                            evm.get_block_env_accesses().contains(access_type),
                            "Transaction should have accessed {:?} for {}",
                            access_type,
                            opcode_name
                        );

                        let accesses = evm.get_block_env_accesses();
                        if opcode_name == "BLOCKHASH" {
                            // BLOCKHASH accesses both BlockNumber (to validate range) and BlockHash
                            assert_eq!(accesses.count_accessed(), 2,  "BLOCKHASH should access exactly two items: BlockNumber and BlockHash");
                            assert!(
                                accesses.contains(BlockEnvAccess::BLOCK_NUMBER),
                                "BLOCKHASH should access BlockNumber for validation"
                            );
                            assert!(
                                accesses.contains(access_type),
                                "BLOCKHASH should access BlockHash"
                            );
                        } else {
                            assert_eq!(
                                accesses.count_accessed(),
                                1,
                                "Should have exactly one access for {}",
                                opcode_name
                            );
                            assert!(
                                accesses.contains(access_type),
                                "Should have accessed {:?} for {}",
                                access_type,
                                opcode_name
                            );
                        }
                    }
                    None => {
                        assert!(
                            evm.get_block_env_accesses().is_empty(),
                            "Transaction should NOT have accessed block env for {}",
                            opcode_name
                        );
                        assert_eq!(
                            evm.get_block_env_accesses().count_accessed(),
                            0,
                            "Access count should be zero for {}",
                            opcode_name
                        );
                    }
                }

                // Test reset functionality
                evm.reset_block_env_access();
                assert!(
                    evm.get_block_env_accesses().is_empty(),
                    "Block env access should be reset for {}",
                    opcode_name
                );
                assert_eq!(
                    evm.get_block_env_accesses().count_accessed(),
                    0,
                    "Access count should be zero after reset for {}",
                    opcode_name
                );
            }
        }

        /// Test multiple block env accesses in a single transaction
        #[test]
        fn test_multiple_block_env_accesses() {
            let mut db = CacheDB::<EmptyDB>::default();
            let contract_address = address!("0000000000000000000000000000000000100001");

            // Contract that accesses multiple block env values: NUMBER, TIMESTAMP, BASEFEE
            let contract_code = vec![
                NUMBER, POP, // Access block number, then pop
                TIMESTAMP, POP, // Access timestamp, then pop
                BASEFEE, POP, // Access basefee, then pop
                STOP,
            ];
            set_account_code(&mut db, contract_address, contract_code.into());

            let mut context = Context::new(db, SpecId::MINI_REX, NoOpOracle);
            // Configure L1BlockInfo to avoid operator fee scalar panic
            context.chain_mut().operator_fee_scalar = Some(U256::from(0));
            context.chain_mut().operator_fee_constant = Some(U256::from(0));
            let mut evm = Evm::new(context, NoOpInspector);

            // Execute transaction
            let tx = Transaction {
                base: revm::context::TxEnv {
                    caller: address!("0000000000000000000000000000000000100000"),
                    kind: revm::primitives::TxKind::Call(contract_address),
                    data: Bytes::default(),
                    value: U256::ZERO,
                    gas_limit: 1000000000000000000u64,
                    ..Default::default()
                },
                ..Default::default()
            };

            let result = alloy_evm::Evm::transact_raw(&mut evm, tx);
            assert!(result.is_ok());

            // Should have accessed 3 different types
            let accesses = evm.get_block_env_accesses();
            assert!(!accesses.is_empty());
            assert_eq!(
                accesses.count_accessed(),
                3,
                "Should have accessed 3 different block env types"
            );

            // Check specific accesses
            assert!(accesses.contains(BlockEnvAccess::BLOCK_NUMBER));
            assert!(accesses.contains(BlockEnvAccess::TIMESTAMP));
            assert!(accesses.contains(BlockEnvAccess::BASE_FEE));
        }

        /// Test that access list is reset between transactions
        #[test]
        fn test_block_env_reset_between_transactions() {
            let mut db = CacheDB::<EmptyDB>::default();
            let contract_address = address!("0000000000000000000000000000000000100001");
            set_account_code(&mut db, contract_address, vec![NUMBER, STOP].into());

            let mut context = Context::new(db, SpecId::MINI_REX, NoOpOracle);
            // Configure L1BlockInfo to avoid operator fee scalar panic
            context.chain_mut().operator_fee_scalar = Some(U256::from(0));
            context.chain_mut().operator_fee_constant = Some(U256::from(0));
            let mut evm = Evm::new(context, NoOpInspector);

            // First transaction - accesses block env
            let tx1 = Transaction {
                base: revm::context::TxEnv {
                    caller: address!("0000000000000000000000000000000000100000"),
                    kind: revm::primitives::TxKind::Call(contract_address),
                    data: Bytes::default(),
                    value: U256::ZERO,
                    gas_limit: 1000000000000000000u64,
                    ..Default::default()
                },
                ..Default::default()
            };

            let result1 = alloy_evm::Evm::transact_raw(&mut evm, tx1);
            assert!(result1.is_ok());
            assert!(
                !evm.get_block_env_accesses().is_empty(),
                "First transaction should access block env"
            );

            // Simulate setting up for a new transaction
            let tx2_env = revm::context::TxEnv {
                caller: address!("0000000000000000000000000000000000100000"),
                kind: revm::primitives::TxKind::Call(contract_address),
                data: Bytes::default(),
                value: U256::ZERO,
                gas_limit: 1000000000000000000u64,
                nonce: 0, // Same account should continue with correct nonce
                ..Default::default()
            };
            let tx2 = Transaction { base: tx2_env, ..Default::default() };

            // This simulates how the EVM would be used for a new transaction
            // Setting tx should automatically reset the block env access
            use revm::ExecuteEvm;
            evm.transact_one(tx2).unwrap();

            // After setting new transaction, flag should be reset automatically
            // But since we executed the same contract (NUMBER opcode), it should be accessed again
            assert!(
                !evm.get_block_env_accesses().is_empty(),
                "Block env should be accessed again for second transaction"
            );
        }
    }

    mod quadratic_log_cost {
        use alloy_primitives::{address, Bytes, U256};
        use revm::{
            bytecode::opcode::LOG0,
            database::{CacheDB, EmptyDB},
        };

        use super::*;

        fn log_cost_test_case(
            spec: SpecId,
            topic_count: usize,
            log_size: usize,
            expected_gas: u64,
        ) {
            assert!(topic_count <= 4);

            let mut contract_code = vec![];
            opcode_gen::push_number(&mut contract_code, log_size as u64);
            opcode_gen::push_number(&mut contract_code, 0u64);
            for _ in 0..topic_count {
                opcode_gen::push_number(&mut contract_code, 0u64);
            }
            contract_code.extend(vec![LOG0 + topic_count as u8]);

            let mut db = CacheDB::<EmptyDB>::default();
            let contract_address = address!("0000000000000000000000000000000000100002");
            set_account_code(&mut db, contract_address, contract_code.clone().into());

            let caller = address!("0000000000000000000000000000000000100000");
            let callee = Some(contract_address);
            let result = transact(spec, &mut db, caller, callee, Bytes::default(), U256::ZERO);
            assert!(result.is_ok());
            assert_eq!(result.unwrap().result.gas_used(), expected_gas);
        }

        #[test]
        fn test_log_data_cost() {
            log_cost_test_case(SpecId::EQUIVALENCE, 0, 0, 21381);
            log_cost_test_case(SpecId::MINI_REX, 0, 0, 21381);
            log_cost_test_case(SpecId::EQUIVALENCE, 0, 1024, 29671);
            log_cost_test_case(SpecId::MINI_REX, 0, 1024, 29671);
            log_cost_test_case(SpecId::EQUIVALENCE, 0, 4096, 54565);
            log_cost_test_case(SpecId::MINI_REX, 0, 4096, 54565);
            log_cost_test_case(SpecId::EQUIVALENCE, 0, 4097, 54576);
            log_cost_test_case(SpecId::MINI_REX, 0, 4097, 54569);
            log_cost_test_case(SpecId::EQUIVALENCE, 0, 8192, 87813);
            log_cost_test_case(SpecId::MINI_REX, 0, 8192, 16832261);
        }

        #[test]
        fn test_log_topic_cost() {
            log_cost_test_case(SpecId::EQUIVALENCE, 0, 0, 21381);
            log_cost_test_case(SpecId::MINI_REX, 0, 0, 21381);
            log_cost_test_case(SpecId::EQUIVALENCE, 1, 0, 21759);
            log_cost_test_case(SpecId::MINI_REX, 1, 0, 31384);
            log_cost_test_case(SpecId::EQUIVALENCE, 4, 0, 22893);
            log_cost_test_case(SpecId::MINI_REX, 4, 0, 61393);
        }
    }

    mod disallow_selfdestruct {
        use alloy_primitives::{address, Bytes, U256};
        use revm::{
            bytecode::opcode::{PUSH0, SELFDESTRUCT},
            context::result::{EVMError, ExecutionResult, InvalidTransaction, ResultAndState},
            database::{CacheDB, EmptyDB},
            state::AccountInfo,
        };

        use crate::{
            test_utils::{set_account_code, transact},
            HaltReason, SpecId, TransactionError,
        };

        /// Test that SELFDESTRUCT opcode works normally before Mini-Rex
        #[test]
        fn test_selfdestruct_allowed_before_mini_rex() {
            let mut db = CacheDB::<EmptyDB>::default();
            let contract_address = address!("0000000000000000000000000000000000100001");
            let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
            set_account_code(&mut db, contract_address, code.into());

            let caller = address!("0000000000000000000000000000000000100000");
            let callee = Some(contract_address);
            let result = transact(
                SpecId::EQUIVALENCE,
                &mut db,
                caller,
                callee,
                Bytes::default(),
                U256::ZERO,
            );
            assert!(result.is_ok());
            assert_eq!(result.unwrap().result.gas_used(), 26004);
        }

        /// Test that SELFDESTRUCT opcode is disabled and returns `InvalidFEOpcode` after Mini-Rex
        /// hardfork
        #[test]
        fn test_selfdestruct_disallowed_after_mini_rex() {
            let mut db = CacheDB::<EmptyDB>::default();
            let contract_address = address!("0000000000000000000000000000000000100001");
            let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
            set_account_code(&mut db, contract_address, code.into());

            let caller = address!("0000000000000000000000000000000000100000");
            let callee = Some(contract_address);
            let result =
                transact(SpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO);
            assert!(matches!(
                result,
                Ok(ResultAndState {
                    result: ExecutionResult::Halt {
                        reason: HaltReason::Base(
                            revm::context::result::HaltReason::InvalidFEOpcode
                        ),
                        ..
                    },
                    ..
                })
            ));
        }
    }

    #[test]
    fn test_evm_state_zero_storage() {
        let mut db = CacheDB::<EmptyDB>::default();
        let contract_address = address!("0000000000000000000000000000000000100001");
        let code = vec![PUSH0, revm::bytecode::opcode::SLOAD];
        set_account_code(&mut db, contract_address, code.into());

        let caller = address!("0000000000000000000000000000000000100000");
        let callee = Some(contract_address);
        let result =
            transact(SpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO)
                .unwrap();
        let state = result.state;
        let contract = state.get(&contract_address).unwrap();
        // the non-existent storage slot will present in the state
        assert_eq!(contract.storage.len(), 1);
        assert!(contract.storage.contains_key(&U256::ZERO));
    }
}
