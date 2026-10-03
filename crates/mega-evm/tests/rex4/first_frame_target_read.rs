//! A call transaction reads its target only when its first frame is created.
//!
//! revm 40 reads a call transaction's target, and through an EIP-7702 designation its delegate,
//! with their code before any frame-init check runs. The deployed implementation read them only
//! when it created the first frame, so a first frame ended before that — by a limit already
//! exceeded, or by a system contract interceptor — read neither. A stateless witness carries
//! only what the deployed execution read, so executing the transaction must not need more.
//!
//! Each case serves the database from a store that fails the read in question: a transaction
//! that never makes the read executes, one that makes it fails with the database error. Every
//! expectation here was measured on the deployed implementation.

use alloy_primitives::{address, keccak256, Address, Bytes, U256};
use mega_evm::{
    test_utils::{ErrorInjectingDatabase, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH,
};
use revm::{
    context::{result::ExecutionResult, tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::Bytecode,
};

const SENDER: Address = address!("0000000000000000000000000000000000470000");
/// A contract the database cannot serve.
const TARGET: Address = address!("00000000000000000000000000000000004700a1");
/// An EIP-7702 delegator whose delegate is [`TARGET`].
const DELEGATOR: Address = address!("00000000000000000000000000000000004700d1");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000470099");

const SPECS: [MegaSpecId; 3] = [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// Runs a call from [`SENDER`] to `to` with `data`, capping the transaction's data size at
/// `data_size_limit` when given, and returns the result, or `None` on a database error.
fn run(
    spec: MegaSpecId,
    mut db: ErrorInjectingDatabase,
    to: Address,
    data: Bytes,
    data_size_limit: Option<u64>,
) -> Option<ExecutionResult<mega_evm::MegaHaltReason>> {
    db.set_account_balance(SENDER, U256::from(10).pow(U256::from(30)));
    let mut limits = EvmTxRuntimeLimits::from_spec(spec);
    if let Some(limit) = data_size_limit {
        limits.tx_data_size_limit = limit;
    }
    let mut context = MegaContext::new(&mut db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(SENDER)
            .call(to)
            .data(data)
            .gas_limit(5_000_000)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).ok().map(|outcome| outcome.result)
}

/// A database whose `basic()` fails for [`TARGET`], with [`DELEGATOR`] delegating to it.
fn failing_target_db() -> ErrorInjectingDatabase {
    let mut memory = MemoryDatabase::default();
    let mut raw = vec![0xef, 0x01, 0x00];
    raw.extend_from_slice(TARGET.as_slice());
    let designation = Bytecode::new_raw(Bytes::from(raw));
    assert!(designation.is_eip7702(), "fixture must install a real delegation");
    let code_hash = designation.hash_slow();
    let account = memory.load_account(DELEGATOR).expect("in-memory account load");
    account.info.code = Some(designation);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
    let mut db = ErrorInjectingDatabase::new(memory);
    db.fail_on_account = Some(TARGET);
    db
}

/// A transaction halted by a limit its intrinsic usage already exceeds never creates its first
/// frame, so it reads neither its target nor, for a delegator, the delegate.
#[test]
fn test_first_frame_ended_by_an_exceeded_limit_reads_no_target() {
    for spec in SPECS {
        for to in [TARGET, DELEGATOR] {
            let result = run(spec, failing_target_db(), to, Bytes::new(), Some(1));
            assert!(
                result.as_ref().is_some_and(ExecutionResult::is_halt),
                "{spec:?}: to {to} with a data-size limit of 1: {result:?}",
            );
            // Control: without the limit the frame is created and the read fails.
            assert!(
                run(spec, failing_target_db(), to, Bytes::new(), None).is_none(),
                "{spec:?}: to {to}, frame created: the read must reach the database",
            );
        }
    }
}

/// A top-level call that a system contract interceptor answers never creates its first frame,
/// so it never reads the system contract's code.
#[test]
fn test_first_frame_answered_by_an_interceptor_reads_no_code() {
    let intercepted = Bytes::copy_from_slice(&keccak256("remainingComputeGas()")[..4]);
    let unknown = Bytes::copy_from_slice(&keccak256("notAMethod()")[..4]);
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        let db = || {
            let memory = MemoryDatabase::default()
                .account_lazy_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE_HASH);
            let mut db = ErrorInjectingDatabase::new(memory);
            db.fail_on_code_by_hash = Some(LIMIT_CONTROL_CODE_HASH);
            db
        };
        let result = run(spec, db(), LIMIT_CONTROL_ADDRESS, intercepted.clone(), None);
        assert!(
            result.as_ref().is_some_and(ExecutionResult::is_success),
            "{spec:?}: intercepted remainingComputeGas(): {result:?}",
        );
        // Control: an unknown selector falls through to the bytecode, which must be read.
        assert!(
            run(spec, db(), LIMIT_CONTROL_ADDRESS, unknown.clone(), None).is_none(),
            "{spec:?}: unknown selector: the code read must reach the database",
        );
    }
}
