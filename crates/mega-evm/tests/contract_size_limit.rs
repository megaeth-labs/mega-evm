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

/// Test that verifies `EQUIVALENCE` spec allows deployment with EIP-3860 max initcode size.
#[test]
fn test_equivalence_eip3860_max_initcode_size() {
    initcode_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip3860::MAX_INITCODE_SIZE,
        true,
    );
}

/// Test that verifies `EQUIVALENCE` spec rejects deployment with `MiniRex` max initcode size.
#[test]
fn test_equivalence_minirex_max_initcode_size() {
    initcode_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        constants::mini_rex::MAX_INITCODE_SIZE,
        false,
    );
}

/// Test that verifies `MINI_REX` spec allows deployment with EIP-3860 max initcode size.
#[test]
fn test_minirex_eip3860_max_initcode_size() {
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip3860::MAX_INITCODE_SIZE,
        true,
    );
}

/// Test that verifies `MINI_REX` spec allows deployment with `MiniRex` max initcode size.
#[test]
fn test_minirex_minirex_max_initcode_size() {
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_INITCODE_SIZE,
        true,
    );
}

/// Test that verifies `MINI_REX` spec rejects deployment with `MiniRex` max initcode size + 1.
#[test]
fn test_minirex_minirex_max_initcode_size_plus_one() {
    initcode_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_INITCODE_SIZE + 1,
        false,
    );
}

/// Test that verifies `MINI_REX` spec rejects deployment with double `MiniRex` max initcode size.
#[test]
fn test_minirex_double_minirex_max_initcode_size() {
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

/// Test that verifies `EQUIVALENCE` spec allows deployment with EIP-170 max code size.
#[test]
fn test_equivalence_eip170_max_code_size() {
    contract_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
}

/// Test that verifies `EQUIVALENCE` spec rejects deployment with `MiniRex` max contract size.
#[test]
fn test_equivalence_minirex_max_contract_size() {
    contract_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        constants::mini_rex::MAX_CONTRACT_SIZE,
        false,
    );
}

/// Test that verifies `MINI_REX` spec allows deployment with EIP-170 max code size.
#[test]
fn test_minirex_eip170_max_code_size() {
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
}

/// Test that verifies `MINI_REX` spec allows deployment with `MiniRex` max contract size.
#[test]
fn test_minirex_minirex_max_contract_size() {
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE,
        true,
    );
}

/// Test that verifies `MINI_REX` spec rejects deployment with `MiniRex` max contract size + 1.
#[test]
fn test_minirex_minirex_max_contract_size_plus_one() {
    contract_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE + 1,
        false,
    );
}

/// Test that verifies `MINI_REX` spec rejects deployment with double `MiniRex` max contract size.
#[test]
fn test_minirex_double_minirex_max_contract_size() {
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

/// Test that verifies `EQUIVALENCE` spec allows CREATE opcode with EIP-170 max code size.
#[test]
fn test_equivalence_create_eip170_max_code_size() {
    contract_factory_code_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
}

/// Test that verifies `EQUIVALENCE` spec rejects CREATE opcode with EIP-170 max code size + 1.
#[test]
fn test_equivalence_create_eip170_max_code_size_plus_one() {
    contract_factory_code_size_limit_test_case(
        MegaSpecId::EQUIVALENCE,
        revm::primitives::eip170::MAX_CODE_SIZE + 1,
        false,
    );
}

/// Test that verifies `MINI_REX` spec allows CREATE opcode with EIP-170 max code size.
#[test]
fn test_minirex_create_eip170_max_code_size() {
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip170::MAX_CODE_SIZE,
        true,
    );
}

/// Test that verifies `MINI_REX` spec allows CREATE opcode with EIP-170 max code size + 1.
#[test]
fn test_minirex_create_eip170_max_code_size_plus_one() {
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        revm::primitives::eip170::MAX_CODE_SIZE + 1,
        true,
    );
}

/// Test that verifies `MINI_REX` spec allows CREATE opcode with `MiniRex` max contract size.
#[test]
fn test_minirex_create_minirex_max_contract_size() {
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE,
        true,
    );
}

/// Test that verifies `MINI_REX` spec rejects CREATE opcode with `MiniRex` max contract size + 1.
#[test]
fn test_minirex_create_minirex_max_contract_size_plus_one() {
    contract_factory_code_size_limit_test_case(
        MegaSpecId::MINI_REX,
        constants::mini_rex::MAX_CONTRACT_SIZE + 1,
        false,
    );
}
