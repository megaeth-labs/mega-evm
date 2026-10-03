//! Post-commit limit rewrite must not put constructor logs on a failed receipt (REX).
//!
//! Under REX the CREATE code-deposit compute gas is recorded in `after_frame_run` **after**
//! `process_next_action` has committed the CREATE checkpoint. When that post-commit charge
//! exceeds the TX-level detained compute gas cap, the frame result is rewritten from `Return`
//! to `OutOfGas` while the journal still holds the constructor's logs and state.
//!
//! revm 40 attaches `journal.take_logs()` to `Halt`, so without the `execution_result` strip
//! the failed receipt would carry those logs. This test is the positive leak case: it goes
//! red if the strip is removed.
//!
//! Spec: `MegaSpecId::REX`. Path: TX-level halt rewrite after CREATE commit
//! (`AdditionalLimit::after_frame_run` → `mark_frame_result_as_exceeding_limit`).

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EmptyExternalEnv, EvmTxRuntimeLimits, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{LOG0, NUMBER, POP, PUSH0, RETURN},
    context::{
        result::{ExecutionResult, ResultAndState},
        BlockEnv, TxEnv,
    },
    primitives::{TxKind, KECCAK_EMPTY},
    state::AccountStatus,
};

const CALLER: Address = address!("0000000000000000000000000000000000500400");
/// Absolute detained compute-gas cap after `NUMBER` (pre-REX4 absolute detention).
///
/// Must sit above constructor compute so the frame reaches a successful RETURN and commits,
/// but below constructor + `CODE_LEN * CODEDEPOSIT` so the post-commit deposit charge halts.
const DETENTION_CAP: u64 = 100_000;
/// Runtime code length: deposit compute gas = `CODE_LEN * 200` exceeds residual detention room.
const CODE_LEN: u64 = 1_000;
/// revm's per-byte code-deposit gas (`revm::interpreter::gas::CODEDEPOSIT`).
const CODEDEPOSIT: u64 = 200;
/// Enough EVM gas for CREATE storage gas (`CODEDEPOSIT_STORAGE_GAS * CODE_LEN` plus overhead).
const GAS_LIMIT: u64 = 50_000_000;
/// Pinned gasUsed for this exact setup — must stay stable across the logs-only fix.
const EXPECTED_GAS_USED: u64 = 10_294_336;

/// Init code: read `NUMBER` (detain), emit `LOG0`, return `CODE_LEN` zero bytes from memory.
fn init_code() -> Bytes {
    let mut code = vec![NUMBER, POP, PUSH0, PUSH0, LOG0, 0x63];
    code.extend_from_slice(&(CODE_LEN as u32).to_be_bytes());
    code.extend_from_slice(&[PUSH0, RETURN]);
    Bytes::from(code)
}

fn deploy() -> ResultAndState<MegaHaltReason> {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1u64));
    let mut context = MegaContext::new(db, MegaSpecId::REX)
        .with_block(BlockEnv { gas_limit: u64::MAX, ..Default::default() })
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(DETENTION_CAP),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut tx = MegaTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        data: init_code(),
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm: MegaEvm<_, revm::inspector::NoOpInspector, EmptyExternalEnv> =
        MegaEvm::new(context);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("CREATE tx must execute")
}

/// Top-level CREATE that detains via `NUMBER`, emits a log, then OOGs on post-commit code-deposit
/// compute gas. The receipt must be a log-free Halt; the deployed account stays in state.
#[test]
fn test_rex_post_commit_create_halt_has_empty_logs_and_committed_state() {
    let result = deploy();

    assert!(
        result.result.is_halt(),
        "post-commit code-deposit exceed must Halt, got {:?}",
        result.result,
    );
    assert!(
        result.result.logs().is_empty(),
        "failed receipt must not carry constructor logs; got {} log(s)",
        result.result.logs().len(),
    );
    assert_eq!(
        result.result.tx_gas_used(),
        EXPECTED_GAS_USED,
        "gasUsed must be unchanged by the logs strip",
    );
    // Tie the halt to the mechanism under test. Only the one-shot code-deposit record is large
    // enough to carry usage this far past the cap, so a future change that halts this transaction
    // for some other reason stops satisfying the test instead of quietly passing it.
    let ExecutionResult::Halt { reason, .. } = &result.result else {
        unreachable!("the halt was asserted above")
    };
    let MegaHaltReason::VolatileDataAccessOutOfGas { actual, .. } = reason else {
        panic!("the detained cap must be the binding limit, got {reason:?}")
    };
    assert!(
        *actual >= CODE_LEN * CODEDEPOSIT,
        "usage must overshoot by the {} code-deposit charge, but only reached {actual}",
        CODE_LEN * CODEDEPOSIT,
    );

    let created = CALLER.create(0);
    let account = result
        .state
        .get(&created)
        .unwrap_or_else(|| panic!("deployed account {created} must remain committed in state"));
    assert!(
        account.status.contains(AccountStatus::Created),
        "created account must keep Created status after post-commit halt rewrite, got {:?}",
        account.status,
    );
    assert!(
        account.info.code.as_ref().is_some_and(|c| !c.is_empty()) ||
            account.info.code_hash != KECCAK_EMPTY,
        "deployed bytecode must remain committed (window shape: state stays, receipt fails)",
    );
}
