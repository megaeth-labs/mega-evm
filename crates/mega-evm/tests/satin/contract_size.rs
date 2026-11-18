//! The code-size limits the Satin configuration raises.
//!
//! A deployed contract may hold 512 KiB, eight times more than EIP-7954's bound and twenty-one
//! times EIP-170's; an initcode may hold twice that, the ratio EIP-3860 sets between the two.
//! Both bounds are `CfgEnv` limits the spec fixes, so they apply to a creation transaction and to
//! a `CREATE` alike.

use alloy_evm::{Evm, EvmError, InvalidTxError};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{MAX_CONTRACT_SIZE, MAX_INITCODE_SIZE, TX_GAS_LIMIT_CAP},
    test_utils::MemoryDatabase,
    MegaEvm, MegaHaltReason, MegaTransaction,
};
use revm::{bytecode::opcode::STOP, context::result::ExecutionResult};

use crate::common::{context, create, history};

const CALLER: Address = address!("0000000000000000000000000000000000100000");

/// Room for everything a creation here draws, all of it above the execution cap so it comes out of
/// the reservoir: the state gas of the largest deployable code, the history of that code and of a
/// body carrying twice the largest init code, and a billion more, at the byte prices in effect.
fn gas_limit() -> u64 {
    TX_GAS_LIMIT_CAP +
        MAX_CONTRACT_SIZE as u64 *
            mega_evm::satin_gas_params()
                .get(revm::context_interface::cfg::GasId::code_deposit_state_gas()) +
        history((MAX_CONTRACT_SIZE + 2 * MAX_INITCODE_SIZE) as u64) +
        1_000_000_000
}

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
    run(create(CALLER, init_code, gas_limit()), db)
}

/// Init code of `size` bytes that does nothing.
fn initcode_of(size: usize) -> Bytes {
    Bytes::from(vec![STOP; size])
}

/* ---------- the initcode bound ---------- */

/// The Satin bound itself is accepted.
#[test]
fn test_the_satin_initcode_size_is_accepted() {
    assert!(deploy(initcode_of(MAX_INITCODE_SIZE)).unwrap().is_success());
}
