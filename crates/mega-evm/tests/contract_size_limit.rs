//! Tests for the increased contract size limit and initcode size limit.

use alloy_primitives::{address, Bytes, U256};
use core::convert::Infallible;
use mega_evm::{test_utils::*, *};
use revm::{
    bytecode::opcode::{CREATE, INVALID, ISZERO, JUMPDEST, JUMPI, PUSH1, RETURN, STOP},
    context::result::{EVMError, ExecutionResult, InvalidTransaction, ResultAndState},
    database::{CacheDB, EmptyDB},
};

fn deploy_contract(
    db: &mut CacheDB<EmptyDB>,
    bytecode: Bytes,
    spec: MegaSpecId,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, TransactionError>> {
    transact(
        spec,
        db,
        address!("0000000000000000000000000000000000100000"),
        None,
        bytecode,
        U256::ZERO,
    )
}

fn initcode_size_limit_test_case(spec: MegaSpecId, initcode_size: usize, success: bool) {
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
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip3860::MAX_INITCODE_SIZE,
        true,
    );
    initcode_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        constants::mini_rex::MAX_INITCODE_SIZE,
        false,
    );
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip3860::MAX_INITCODE_SIZE,
        true,
    );
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_INITCODE_SIZE,
        true,
    );
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_INITCODE_SIZE + 1,
        false,
    );
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
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

fn contract_size_limit_test_case(spec: MegaSpecId, contract_size: usize, success: bool) {
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
                    reason: MegaHaltReason::Base(OpHaltReason::Base(
                        EthHaltReason::CreateContractSizeLimit
                    ),),
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
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
    contract_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        constants::mini_rex::MAX_CONTRACT_SIZE,
        false,
    );
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE,
        true,
    );
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE + 1,
        false,
    );
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        2 * constants::mini_rex::MAX_CONTRACT_SIZE,
        false,
    );
}

fn contract_factory_code_size_limit_test_case(
    spec: MegaSpecId,
    contract_size: usize,
    success: bool,
) {
    // 1. Create a "factory" contract that uses the CREATE opcode to create another large contract
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
    let result =
        transact(spec, &mut db, caller, Some(factory_address), Bytes::default(), U256::ZERO);
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
                    reason: MegaHaltReason::Base(OpHaltReason::Base(
                        EthHaltReason::InvalidFEOpcode
                    )),
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
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
    contract_factory_code_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip170::MAX_CODE_SIZE + 1,
        false,
    );
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip170::MAX_CODE_SIZE + 1,
        true,
    );
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE,
        true,
    );
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE + 1,
        false,
    );
}
