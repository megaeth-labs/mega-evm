//! The code-size limits the Satin configuration raises.
//!
//! A deployed contract may hold 512 KiB, eight times more than EIP-7954's bound and twenty-one
//! times EIP-170's; an initcode may hold twice that, the ratio EIP-3860 sets between the two.
//! Both bounds are `CfgEnv` limits the spec fixes, so they apply to a creation transaction and to
//! a `CREATE` alike.

use alloy_evm::{Evm, EvmError, InvalidTxError};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{MAX_CONTRACT_SIZE, MAX_INITCODE_SIZE},
    test_utils::{right_pad_bytes, BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaHaltReason, MegaTransaction,
};
use revm::{
    bytecode::opcode::{CREATE, INVALID, ISZERO, JUMPDEST, JUMPI, PUSH1, RETURN, STOP},
    context::result::{ExecutionResult, HaltReason, InvalidTransaction},
    primitives::{eip170, eip3860},
};

use crate::common::{call, context, create};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const FACTORY: Address = address!("0000000000000000000000000000000000100001");

/// Room for the state gas a megabyte of deployed code draws, all of it above the execution cap so
/// it comes out of the reservoir.
const GAS_LIMIT: u64 = 4_000_000_000;

/// Runs `tx` from `CALLER` and returns its result, or the validation error that rejected it.
fn run(tx: MegaTransaction, db: MemoryDatabase) -> Result<ExecutionResult<MegaHaltReason>, String> {
    let mut evm = MegaEvm::new(context(db));
    match evm.transact_raw(tx) {
        Ok(outcome) => Ok(outcome.result),
        Err(err) => {
            let invalid = err
                .as_invalid_tx_err()
                .and_then(InvalidTxError::as_invalid_tx_err)
                .unwrap_or_else(|| panic!("unexpected error {err:?}"));
            Err(format!("{invalid:?}"))
        }
    }
}

/// Runs `init_code` as a creation transaction from `CALLER`.
fn deploy(init_code: Bytes) -> Result<ExecutionResult<MegaHaltReason>, String> {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    run(create(CALLER, init_code, GAS_LIMIT), db)
}

/// Init code of `size` bytes that does nothing.
fn initcode_of(size: usize) -> Bytes {
    Bytes::from(vec![STOP; size])
}

/// Init code that returns `size` bytes of zeros as the deployed code.
fn constructor_returning(size: usize) -> Bytes {
    let code = BytecodeBuilder::default()
        .push_number(size as u64)
        .append_many([PUSH1, 0x00])
        .append(RETURN)
        .build_vec();
    right_pad_bytes(code, 32).into()
}

fn assert_initcode_limit(result: Result<ExecutionResult<MegaHaltReason>, String>) {
    let expected = format!("{:?}", InvalidTransaction::CreateInitCodeSizeLimit);
    assert_eq!(result.expect_err("the transaction is rejected"), expected);
}

fn assert_code_size_limit(result: Result<ExecutionResult<MegaHaltReason>, String>) {
    let halt = result.expect("the transaction is valid");
    assert!(
        matches!(
            halt,
            ExecutionResult::Halt {
                reason: MegaHaltReason::Base(HaltReason::CreateContractSizeLimit),
                ..
            }
        ),
        "{halt:?}"
    );
}

/* ---------- the initcode bound ---------- */

/// An initcode of EIP-3860's size is far below the Satin bound and goes through.
#[test]
fn test_the_eip3860_initcode_size_is_accepted() {
    assert!(deploy(initcode_of(eip3860::MAX_INITCODE_SIZE)).unwrap().is_success());
}

/// The Satin bound itself is accepted.
#[test]
fn test_the_satin_initcode_size_is_accepted() {
    assert!(deploy(initcode_of(MAX_INITCODE_SIZE)).unwrap().is_success());
}

/// One byte past it is rejected before the transaction runs.
#[test]
fn test_one_byte_past_the_satin_initcode_size_is_rejected() {
    assert_initcode_limit(deploy(initcode_of(MAX_INITCODE_SIZE + 1)));
}

/// So is twice it.
#[test]
fn test_double_the_satin_initcode_size_is_rejected() {
    assert_initcode_limit(deploy(initcode_of(2 * MAX_INITCODE_SIZE)));
}

/// The initcode bound is twice the contract bound, which is the ratio EIP-3860 sets.
#[test]
fn test_the_initcode_bound_is_twice_the_contract_bound() {
    assert_eq!(MAX_INITCODE_SIZE, 2 * MAX_CONTRACT_SIZE);
    assert_eq!(MAX_CONTRACT_SIZE, 512 * 1024);
}

/* ---------- the deployed-code bound, from a creation transaction ---------- */

/// Code of EIP-170's size deploys.
#[test]
fn test_the_eip170_code_size_deploys() {
    assert!(deploy(constructor_returning(eip170::MAX_CODE_SIZE)).unwrap().is_success());
}

/// Code of the Satin size deploys.
#[test]
fn test_the_satin_code_size_deploys() {
    assert!(deploy(constructor_returning(MAX_CONTRACT_SIZE)).unwrap().is_success());
}

/// One byte past it halts the creation.
#[test]
fn test_one_byte_past_the_satin_code_size_halts() {
    assert_code_size_limit(deploy(constructor_returning(MAX_CONTRACT_SIZE + 1)));
}

/// So does twice it.
#[test]
fn test_double_the_satin_code_size_halts() {
    assert_code_size_limit(deploy(constructor_returning(2 * MAX_CONTRACT_SIZE)));
}

/* ---------- the deployed-code bound, from a CREATE ---------- */

/// A contract that `CREATE`s a contract of `size` bytes and runs `INVALID` if the creation failed,
/// so a rejected creation shows up as a halt of the outer frame.
fn factory_creating(size: usize) -> Bytes {
    let init_code = constructor_returning(size);
    let code = BytecodeBuilder::default()
        .mstore(0, &init_code)
        .push_number(init_code.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(ISZERO);
    let len = code.len();
    code.push_number(len as u8 + 4)
        .append(JUMPI)
        .append(STOP)
        .append(JUMPDEST)
        .append(INVALID)
        .build()
}

/// Runs a factory that creates `size` bytes of code and returns whether the creation succeeded.
fn create_through_factory(size: usize) -> bool {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(FACTORY, factory_creating(size));
    let result =
        run(call(CALLER, FACTORY, U256::ZERO, GAS_LIMIT), db).expect("the transaction is valid");
    match result {
        ExecutionResult::Success { .. } => true,
        ExecutionResult::Halt {
            reason: MegaHaltReason::Base(HaltReason::InvalidFEOpcode), ..
        } => false,
        other => panic!("unexpected result {other:?}"),
    }
}

/// A `CREATE` of EIP-170's size succeeds, and so does one byte past it: EIP-170 is not the bound
/// here.
#[test]
fn test_create_of_the_eip170_code_size_succeeds() {
    assert!(create_through_factory(eip170::MAX_CODE_SIZE));
    assert!(create_through_factory(eip170::MAX_CODE_SIZE + 1));
}

/// A `CREATE` of the Satin size succeeds.
#[test]
fn test_create_of_the_satin_code_size_succeeds() {
    assert!(create_through_factory(MAX_CONTRACT_SIZE));
}

/// One byte past it fails the creation, which the factory turns into a halt.
#[test]
fn test_create_one_byte_past_the_satin_code_size_fails() {
    assert!(!create_through_factory(MAX_CONTRACT_SIZE + 1));
}
