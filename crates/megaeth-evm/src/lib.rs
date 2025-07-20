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

#[cfg(any(test, feature = "test-utils"))]
mod test_utils;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;

mod types;
pub use types::*;

#[cfg(test)]
mod tests {
    use revm::context::result::{ExecutionResult, HaltReason};

    use super::*;

    mod contract_size_limit {
        use alloy_primitives::{address, Bytes, U256};
        use revm::{
            bytecode::opcode::{CREATE, INVALID, ISZERO, JUMPDEST, JUMPI, PUSH1, RETURN, STOP},
            context::result::{EVMError, InvalidTransaction, ResultAndState},
            database::{CacheDB, EmptyDB},
        };
        use std::convert::Infallible;

        use super::*;

        fn deploy_contract(
            db: &mut CacheDB<EmptyDB>,
            bytecode: Bytes,
            spec: MegaethSpecId,
        ) -> Result<ResultAndState<MegaethHaltReason>, EVMError<Infallible, MegaethTransactionError>>
        {
            transact(
                spec,
                db,
                address!("0000000000000000000000000000000000100000"),
                None,
                bytecode,
                U256::ZERO,
            )
        }

        fn initcode_size_limit_test_case(spec: MegaethSpecId, initcode_size: usize, success: bool) {
            let large_bytecode = vec![STOP; initcode_size];
            let bytecode: Bytes = large_bytecode.into();
            let mut db = CacheDB::<EmptyDB>::default();
            let result = deploy_contract(&mut db, bytecode, spec);
            if success {
                assert!(result.is_ok());
            } else {
                assert!(matches!(
                    result,
                    Err(EVMError::Transaction(MegaethTransactionError::Base(
                        InvalidTransaction::CreateInitCodeSizeLimit
                    )))
                ));
            }
        }

        #[test]
        fn test_eip3860_initcode_size() {
            initcode_size_limit_test_case(
                MegaethSpecId::EQUIVALENCE,
                revm::primitives::MAX_INITCODE_SIZE,
                true,
            );
            initcode_size_limit_test_case(
                MegaethSpecId::EQUIVALENCE,
                constants::mini_rax::MAX_INITCODE_SIZE,
                false,
            );
            initcode_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                revm::primitives::MAX_INITCODE_SIZE,
                true,
            );
            initcode_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                constants::mini_rax::MAX_INITCODE_SIZE,
                true,
            );
            initcode_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                constants::mini_rax::MAX_INITCODE_SIZE + 1,
                false,
            );
            initcode_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                2 * constants::mini_rax::MAX_INITCODE_SIZE,
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

        fn contract_size_limit_test_case(spec: MegaethSpecId, contract_size: usize, success: bool) {
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
                            reason: MegaethHaltReason::Base(HaltReason::CreateContractSizeLimit),
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
                MegaethSpecId::EQUIVALENCE,
                revm::interpreter::MAX_CODE_SIZE,
                true,
            );
            contract_size_limit_test_case(
                MegaethSpecId::EQUIVALENCE,
                constants::mini_rax::MAX_CONTRACT_SIZE,
                false,
            );
            contract_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                revm::interpreter::MAX_CODE_SIZE,
                true,
            );
            contract_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                constants::mini_rax::MAX_CONTRACT_SIZE,
                true,
            );
            contract_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                constants::mini_rax::MAX_CONTRACT_SIZE + 1,
                false,
            );
            contract_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                2 * constants::mini_rax::MAX_CONTRACT_SIZE,
                false,
            );
        }

        fn contract_factory_code_size_limit_test_case(
            spec: MegaethSpecId,
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
                            reason: MegaethHaltReason::Base(HaltReason::InvalidFEOpcode),
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
                MegaethSpecId::EQUIVALENCE,
                revm::interpreter::MAX_CODE_SIZE,
                true,
            );
            contract_factory_code_size_limit_test_case(
                MegaethSpecId::EQUIVALENCE,
                revm::interpreter::MAX_CODE_SIZE + 1,
                false,
            );
            contract_factory_code_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                revm::interpreter::MAX_CODE_SIZE,
                true,
            );
            contract_factory_code_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                revm::interpreter::MAX_CODE_SIZE + 1,
                true,
            );
            contract_factory_code_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                constants::mini_rax::MAX_CONTRACT_SIZE,
                true,
            );
            contract_factory_code_size_limit_test_case(
                MegaethSpecId::MINI_RAX,
                constants::mini_rax::MAX_CONTRACT_SIZE + 1,
                false,
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

        fn log_cost_test_case(spec: MegaethSpecId, log_size: usize, expected_gas: u64) {
            let mut contract_code = vec![];
            opcode_gen::push_number(&mut contract_code, log_size as u64);
            opcode_gen::push_number(&mut contract_code, 0u64);
            contract_code.extend(vec![LOG0]);

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
        fn test_log_cost() {
            log_cost_test_case(MegaethSpecId::EQUIVALENCE, 0, 21381);
            log_cost_test_case(MegaethSpecId::MINI_RAX, 0, 21381);
            log_cost_test_case(MegaethSpecId::EQUIVALENCE, 1024, 29671);
            log_cost_test_case(MegaethSpecId::MINI_RAX, 1024, 29671);
            log_cost_test_case(MegaethSpecId::EQUIVALENCE, 1025, 29682);
            log_cost_test_case(MegaethSpecId::MINI_RAX, 1025, 29675);
            log_cost_test_case(MegaethSpecId::EQUIVALENCE, 2048, 37965);
            log_cost_test_case(MegaethSpecId::MINI_RAX, 2048, 1078349);
        }
    }
}
