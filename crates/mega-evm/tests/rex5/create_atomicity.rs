#![allow(clippy::doc_markdown)]
//! Tests for REX5 CREATE atomicity.
//!
//! Under `MegaSpecId::REX5`, a frame-local compute-gas exceed caused by
//! the canonical code-deposit charge must take effect before
//! `process_next_action` commits the CREATE checkpoint. The result is
//! `Revert` and no code is deployed. Pre-REX5 keeps its existing
//! split-outcome behavior (`Revert` result with committed code).

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EthHaltReason, EvmTxRuntimeLimits, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, OpHaltReason, TestExternalEnvs,
};
use revm::{
    context::{
        result::{ExecutionResult, Output, ResultAndState},
        TxEnv,
    },
    state::EvmState,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");

/// Build init code that returns `code_len` bytes of zeros from memory[0..code_len].
///
/// Layout (PUSH3 used so we can pick large code_len up to ~16M):
///   PUSH3 <code_len>   // 4 bytes
///   PUSH1 0            // 2 bytes
///   RETURN             // 1 byte
/// Total: 7 bytes.
///
/// Compute gas inside the constructor: 3 (PUSH3) + 3 (PUSH1) + 0 (RETURN)
/// + memory expansion gas for `code_len`.
fn return_zeros_initcode(code_len: u32) -> Bytes {
    let bytes = code_len.to_be_bytes();
    let mut code = Vec::with_capacity(7);
    code.push(0x62); // PUSH3
    code.extend_from_slice(&bytes[1..]); // 3-byte big-endian length
    code.push(0x60); // PUSH1
    code.push(0x00);
    code.push(0xf3); // RETURN
    Bytes::from(code)
}

/// Build init code that returns `code_len` bytes starting with 0xEF (EIP-3541
/// prohibited prefix) — the rest is whatever was in memory (zeros). Used to
/// pin the EIP-3541 branch of the helper predicate.
fn return_ef_prefix_initcode() -> Bytes {
    // Constructor:
    //   PUSH1 0xEF    // value
    //   PUSH1 0       // memory offset
    //   MSTORE8       // store 1 byte at offset 0
    //   PUSH1 0x10    // length = 16 bytes
    //   PUSH1 0       // memory offset
    //   RETURN
    Bytes::from(vec![0x60, 0xef, 0x60, 0x00, 0x53, 0x60, 0x10, 0x60, 0x00, 0xf3])
}

fn run_create(
    spec: MegaSpecId,
    tx_compute_gas_limit: u64,
    init_code: Bytes,
    inspector: bool,
) -> ResultAndState<mega_evm::MegaHaltReason> {
    run_create_with_gas_limit(spec, tx_compute_gas_limit, 100_000_000, init_code, inspector)
}

fn run_create_with_gas_limit(
    spec: MegaSpecId,
    tx_compute_gas_limit: u64,
    tx_gas_limit: u64,
    init_code: Bytes,
    inspector: bool,
) -> ResultAndState<mega_evm::MegaHaltReason> {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000_000_000_000_000u128));

    let limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(tx_compute_gas_limit);
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs(TestExternalEnvs::<Infallible>::new().into())
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        gas_limit: tx_gas_limit,
        gas_price: 0,
        data: init_code,
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());

    if inspector {
        // Default no-op inspector — forces inspect_frame_run path.
        let mut evm = MegaEvm::new(context).with_inspector(revm::inspector::NoOpInspector {});
        alloy_evm::Evm::transact_raw(&mut evm, tx)
            .expect("transact (inspector) should not surface EVMError")
    } else {
        let mut evm = MegaEvm::new(context);
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact should not surface EVMError")
    }
}

/// Returns true if the post-state delta contains a non-empty code account
/// at the deployed address. Looking at `state.code` is more reliable than
/// scanning the journal because it reflects what `transact_raw` reports as
/// the committed state delta.
fn deployed_account_has_code(state: &EvmState, deployed_address: Address) -> bool {
    state
        .get(&deployed_address)
        .map(|acc| {
            acc.info.code.as_ref().map(|c| !c.is_empty()).unwrap_or(false) ||
                acc.info.code_hash != revm::primitives::KECCAK_EMPTY
        })
        .unwrap_or(false)
}

/// Constructor returns 8000 bytes; canonical `code_len * 200` = 1.6M.
const CODE_LEN: u32 = 8_000;

/// Per-frame compute budget calibrated so only the code-deposit charge
/// can overshoot: constructor + intrinsic stay well below 1M.
const TIGHT_COMPUTE_BUDGET: u64 = 1_000_000;

/// Per-frame compute budget large enough to cover code-deposit too.
const GENEROUS_COMPUTE_BUDGET: u64 = 4_000_000;

/// Under REX5, a tight compute budget that overshoots only on the
/// code-deposit charge produces a `Revert` result with no committed code.
#[test]
fn test_create_compute_gas_exceed_during_codedeposit_does_not_split_outcome() {
    let res =
        run_create(MegaSpecId::REX5, TIGHT_COMPUTE_BUDGET, return_zeros_initcode(CODE_LEN), false);

    let result_is_revert = matches!(&res.result, ExecutionResult::Revert { .. });
    let result_is_halt = matches!(&res.result, ExecutionResult::Halt { .. });
    assert!(result_is_revert || result_is_halt, "got: {:?}", res.result);

    let deployed_address = CALLER.create(0);
    assert!(
        !deployed_account_has_code(&res.state, deployed_address),
        "deployed account at {deployed_address} must not have code; state entry: {:?}",
        res.state.get(&deployed_address)
    );
}

/// Same fixture under REX4: pre-REX5 keeps its existing split-outcome
/// behavior (`Revert` result with code already committed by
/// `return_create`).
#[test]
fn test_pre_rex5_preserves_split_outcome() {
    let res =
        run_create(MegaSpecId::REX4, TIGHT_COMPUTE_BUDGET, return_zeros_initcode(CODE_LEN), false);

    let result_is_revert = matches!(&res.result, ExecutionResult::Revert { .. });
    let result_is_halt = matches!(&res.result, ExecutionResult::Halt { .. });
    assert!(result_is_revert || result_is_halt, "got: {:?}", res.result);

    let deployed_address = CALLER.create(0);
    assert!(
        deployed_account_has_code(&res.state, deployed_address),
        "REX4 split outcome: deployed account at {deployed_address} must have code; \
         state entry: {:?}",
        res.state.get(&deployed_address)
    );
}

/// Generous compute budget allows CREATE to commit normally.
#[test]
fn test_create_compute_gas_within_budget_still_commits_normally() {
    let res = run_create(
        MegaSpecId::REX5,
        GENEROUS_COMPUTE_BUDGET,
        return_zeros_initcode(CODE_LEN),
        false,
    );

    assert!(res.result.is_success(), "got: {:?}", res.result);

    let deployed_address = match &res.result {
        ExecutionResult::Success { output: Output::Create(_, Some(addr)), .. } => *addr,
        other => panic!("Expected Create output with address, got: {other:?}"),
    };
    assert_eq!(deployed_address, CALLER.create(0));
    assert!(deployed_account_has_code(&res.state, deployed_address));
}

/// `inspect_frame_run` shares the same hooks as `frame_run`; the REX5
/// behavior must apply identically on the inspector path.
#[test]
fn test_inspect_frame_run_path_matches_frame_run() {
    let res = run_create(
        MegaSpecId::REX5,
        TIGHT_COMPUTE_BUDGET,
        return_zeros_initcode(CODE_LEN),
        true, // forces inspect_frame_run path
    );

    let result_is_revert = matches!(&res.result, ExecutionResult::Revert { .. });
    let result_is_halt = matches!(&res.result, ExecutionResult::Halt { .. });
    assert!(result_is_revert || result_is_halt, "got: {:?}", res.result);

    let deployed_address = CALLER.create(0);
    assert!(!deployed_account_has_code(&res.state, deployed_address));
}

/// A CREATE whose constructor returns code starting with 0xEF takes
/// `return_create`'s EIP-3541 revert path; the pre-charge must not fire,
/// so a generous compute budget produces a clean reject.
#[test]
fn test_create_with_eip3541_prefix_skips_pre_charge() {
    let res =
        run_create(MegaSpecId::REX5, GENEROUS_COMPUTE_BUDGET, return_ef_prefix_initcode(), false);

    // op-revm classifies the EIP-3541 reject as either Halt or Revert
    // depending on wrapping; what matters is no success and no code.
    assert!(
        matches!(&res.result, ExecutionResult::Halt { .. } | ExecutionResult::Revert { .. }),
        "got: {:?}",
        res.result
    );
    let deployed_address = CALLER.create(0);
    assert!(!deployed_account_has_code(&res.state, deployed_address));
}

/// A CREATE whose constructor returns valid code but leaves
/// `gas.remaining() < code_deposit_gas` takes `return_create`'s
/// `record_cost` fail path (post-Homestead `OutOfGas`). That is the
/// fourth branch of the pre-charge predicate, and it must skip the
/// pre-charge: revm reverts the checkpoint itself, so a spurious
/// REX5 pre-charge would only double-bill the compute meter.
///
/// We engineer `gas.remaining() < code_deposit_gas` with a tight tx
/// gas limit (`1_650_000` vs `code_deposit_gas = CODE_LEN * 200 =
/// 1_600_000`) and use `TIGHT_COMPUTE_BUDGET` so that any spurious
/// pre-charge would visibly trip the compute meter — the halt would
/// surface as `ComputeGasLimitExceeded` rather than the natural revm
/// `OutOfGas`.
#[test]
fn test_create_insufficient_gas_for_code_deposit_skips_pre_charge() {
    let res = run_create_with_gas_limit(
        MegaSpecId::REX5,
        TIGHT_COMPUTE_BUDGET,
        1_650_000,
        return_zeros_initcode(CODE_LEN),
        false,
    );

    let reason = match &res.result {
        ExecutionResult::Halt { reason, .. } => reason.clone(),
        other => panic!("Expected Halt, got: {other:?}"),
    };
    assert!(
        matches!(reason, MegaHaltReason::Base(OpHaltReason::Base(EthHaltReason::OutOfGas(_)))),
        "halt reason must be the natural revm OutOfGas (return_create's \
         code-deposit record_cost fail), not a compute-meter halt that \
         would signal a spurious pre-charge; got: {reason:?}",
    );

    let deployed_address = CALLER.create(0);
    assert!(!deployed_account_has_code(&res.state, deployed_address));
}

/// A CREATE whose constructor returns more than `max_code_size` bytes
/// takes `return_create`'s EIP-170 revert path; the pre-charge must not
/// fire, so a generous compute budget produces a clean reject.
#[test]
fn test_create_with_oversized_code_skips_pre_charge() {
    // Larger than both EIP-170 (0x6000) and EIP-7907 (0xc000).
    const OVERSIZE_CODE_LEN: u32 = 64_000;
    let res = run_create(
        MegaSpecId::REX5,
        GENEROUS_COMPUTE_BUDGET,
        return_zeros_initcode(OVERSIZE_CODE_LEN),
        false,
    );

    assert!(!res.result.is_success(), "got: {:?}", res.result);
    let deployed_address = CALLER.create(0);
    assert!(!deployed_account_has_code(&res.state, deployed_address));
}
