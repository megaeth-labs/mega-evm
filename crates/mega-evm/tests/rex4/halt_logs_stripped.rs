//! Frame-local Revert rewrite after CREATE commit must not put constructor logs on a failed
//! receipt (REX4).
//!
//! Under REX4 the CREATE code-deposit compute gas is still recorded post-commit in
//! `after_frame_run` (REX5 moved that charge earlier). When the deposit blows the **per-frame**
//! compute budget, `exceeding_instruction_result` / `before_frame_return_result` rewrite the
//! successful CREATE to `InstructionResult::Revert` (`limit.rs` frame-local branch) without
//! undoing the journal commit. revm 40 then attaches those logs to `ExecutionResult::Revert`.
//!
//! This test goes red if the `execution_result` strip is removed.
//!
//! Spec: `MegaSpecId::REX4`. Path: frame-local Revert rewrite after CREATE commit
//! (`before_frame_return_result` / `after_frame_run` with `is_frame_local()`).

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EmptyExternalEnv, EvmTxRuntimeLimits, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{LOG0, PUSH0, RETURN},
    context::{
        result::{ExecutionResult, ResultAndState},
        BlockEnv, TxEnv,
    },
    primitives::{TxKind, KECCAK_EMPTY},
    state::AccountStatus,
};

const CALLER: Address = address!("0000000000000000000000000000000000500600");
/// TX compute-gas limit sized so the top frame's budget is blown only by the code-deposit charge.
/// Intrinsic CREATE compute gas is well under this; `CODE_LEN * 200` is not.
const TX_COMPUTE_GAS_LIMIT: u64 = 80_000;
/// Runtime code length: deposit compute gas = `200_000`.
const CODE_LEN: u64 = 1_000;
const GAS_LIMIT: u64 = 50_000_000;
/// Pinned gasUsed for this exact setup — must stay stable across the logs-only fix.
const EXPECTED_GAS_USED: u64 = 10_293_980;

/// Init code: emit `LOG0`, return `CODE_LEN` zero bytes (no detention — frame budget alone trips).
fn init_code() -> Bytes {
    let mut code = vec![PUSH0, PUSH0, LOG0, 0x63];
    code.extend_from_slice(&(CODE_LEN as u32).to_be_bytes());
    code.extend_from_slice(&[PUSH0, RETURN]);
    Bytes::from(code)
}

fn deploy() -> ResultAndState<MegaHaltReason> {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1u64));
    let mut context = MegaContext::new(db, MegaSpecId::REX4)
        .with_block(BlockEnv { gas_limit: u64::MAX, ..Default::default() })
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(TX_COMPUTE_GAS_LIMIT),
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

/// Top-level CREATE whose post-commit deposit blows the frame-local compute budget → Revert
/// with empty logs, committed state, and stable gasUsed.
#[test]
fn test_rex4_frame_local_create_revert_has_empty_logs_and_committed_state() {
    let result = deploy();

    assert!(
        matches!(result.result, ExecutionResult::Revert { .. }),
        "frame-local exceed must surface as Revert (not Halt), got {:?}",
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

    let created = CALLER.create(0);
    let account = result
        .state
        .get(&created)
        .unwrap_or_else(|| panic!("deployed account {created} must remain committed in state"));
    assert!(
        account.status.contains(AccountStatus::Created),
        "created account must keep Created status after frame-local Revert rewrite, got {:?}",
        account.status,
    );
    assert!(
        account.info.code.as_ref().is_some_and(|c| !c.is_empty()) ||
            account.info.code_hash != KECCAK_EMPTY,
        "deployed bytecode must remain committed (window shape: state stays, receipt fails)",
    );
}
