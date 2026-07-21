//! Attempts to kill the frame-interaction mutants in `src/limit/limit.rs`:
//!
//! * `:585` — `AdditionalLimit::before_frame_run -> Option<InterpreterResult>` replaced with
//!   `None`. The four sub-tracker `before_frame_run` calls use the default trait no-op, so the only
//!   behavioral effect of this hook is its early `check_limit()` halt: when a TX-level limit was
//!   already latched (e.g. a CREATE child frame whose `after_frame_init` recording of the created
//!   account tipped the KV/data budget over), the hook short-circuits the frame *before* its init
//!   code runs. The mutant drops that short-circuit, so the init code runs.
//!
//! * `:676` — `&&` replaced with `||` in the `duplicate_return_frame_result` guard of
//!   `before_frame_return_result`. The flag distinguishes the two `last_frame_result` /
//!   `frame_return_result` calls for a top-level frame that has child frames.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const FACTORY: Address = address!("0000000000000000000000000000000000200001");

/// Constructor (init) code that runs `SSTORE(0, 1)` then `RETURN(0, 1)`: 8 bytes,
/// `PUSH1 1 PUSH0 SSTORE PUSH1 1 PUSH0 RETURN`. The SSTORE makes the constructor's execution
/// observable in gas and state: if the created frame is short-circuited at `before_frame_run`,
/// the SSTORE never runs.
fn create_init_code() -> Vec<u8> {
    vec![PUSH1, 0x01, PUSH0, SSTORE, PUSH1, 0x01, PUSH0, RETURN]
}

/// Factory that stores the init code in memory and CREATEs a contract from it, then STOPs.
fn create_factory_code() -> Bytes {
    let init = create_init_code();
    let init_len = init.len() as u64;
    BytecodeBuilder::default()
        .mstore(0, init) // left-aligned init code at offset 0
        .push_number(init_len) // size
        .push_number(0_u64) // offset
        .push_number(0_u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

fn transact_with_kv_limit(
    db: &mut MemoryDatabase,
    kv_update_limit: u64,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_kv_updates_limit(kv_update_limit),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// A CREATE child frame whose `after_frame_init` records the created account's KV update,
/// tipping the TX over the KV-update budget, must be short-circuited at `before_frame_run`
/// before its constructor body executes. The transaction halts with `KVUpdateLimitExceeded`.
///
/// This pins `gas_used`: if `before_frame_run` is mutated to always return `None`, the
/// constructor's SSTORE runs before the exceed is re-detected in `after_frame_run`,
/// changing the gas consumed by the create frame and therefore the transaction `gas_used`.
#[test]
fn test_before_frame_run_short_circuits_already_exceeded_create_frame() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000));
    db.set_account_code(FACTORY, create_factory_code());

    // KV updates: caller nonce (1) + factory nonce (1) + created account (1) = 3.
    // The created-account KV is recorded in the CREATE frame's `after_frame_init`; with a
    // budget of 2 it tips the limit over, so `before_frame_run` must short-circuit the
    // constructor.
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(FACTORY).gas_limit(10_000_000).build_fill();

    let result = transact_with_kv_limit(&mut db, 2, tx).unwrap();

    let gas_used = match &result.result {
        ExecutionResult::Halt {
            reason: MegaHaltReason::KVUpdateLimitExceeded { .. },
            gas_used,
        } => *gas_used,
        other => panic!("expected KVUpdateLimitExceeded halt, got {other:?}"),
    };
    // Pinned `gas_used` with the constructor short-circuited at `before_frame_run`.
    // Calibrated against the unmutated build; the `-> None` mutant lets the constructor run
    // and produces a different value.
    assert_eq!(
        gas_used, GAS_USED_SHORT_CIRCUITED,
        "before_frame_run must short-circuit the over-budget CREATE frame before its \
         constructor runs; a different gas_used means the short-circuit was dropped"
    );
}

/// Calibrated `gas_used` for the unmutated build, where `before_frame_run` short-circuits the
/// over-budget CREATE frame before its SSTORE constructor runs.
const GAS_USED_SHORT_CIRCUITED: u64 = 2_053_023;
