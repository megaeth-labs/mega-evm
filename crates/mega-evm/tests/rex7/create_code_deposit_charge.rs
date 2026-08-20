//! REX7: the canonical code-deposit compute gas of a CREATE frame is recorded only when the
//! deposit actually happens.
//!
//! revm charges a successful CREATE the active gas schedule's per-byte code-deposit rate when it
//! processes the frame's action, and only then — a frame whose result is no longer successful at
//! that point pays nothing and deposits nothing. `MegaETH` has to decide the charge one step
//! earlier, before the action is processed, because a compute-limit exceed discovered after the
//! CREATE checkpoint is committed would leave the frame's state changes in the journal under a
//! reverted result.
//!
//! Deciding early is not the same as charging early. REX5 and REX6 record the charge and then let
//! the latched exceed mark the result, which leaves the tracker holding compute gas for a deposit
//! that never happened. REX7 asks first: the charge is weighed against the frame's completed usage
//! and recorded only on the answer that lets the deposit go through. The other two answers stop the
//! frame — a frame-local exceed reverts it, a TX-level exceed halts the transaction — and record
//! nothing, which is what keeps the reported compute total equal to the gas the transaction spent.
//!
//! These tests pin the four rows of that decision, the frozen REX4-REX6 shapes they must not
//! disturb, and the journal consistency that the early decision exists for in the first place.
//!
//! One shape that used to be pinned here is gone: a knife-edge CREATE holding exactly revm's
//! built-in per-byte charge under a schedule that charged more, which separated a predicate
//! reading the active schedule from one reading the constant. That fork needed a configuration
//! carrying a gas schedule other than its spec's, and such a configuration is now rejected at
//! every entry point into the EVM rather than run, so the two readings can no longer disagree on
//! any input. What survives here is the pair that is still decidable: installing the built-in
//! rate explicitly changes nothing, and installing anything else is turned away.

use crate::common::{default_envs, finish, transact_tx, Outcome, CALLER, ONE_ETH};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolError as _;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitKind, MegaContext, MegaEvm, MegaHaltReason, MegaLimitExceeded,
    MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{MSTORE, POP, RETURN, TIMESTAMP},
    context::{result::ExecutionResult, tx::TxEnvBuilder, CfgEnv},
    context_interface::cfg::GasId,
    handler::EvmTr,
    state::EvmState,
};

/// revm's per-byte code-deposit gas (`revm::interpreter::gas::CODEDEPOSIT`).
const CODEDEPOSIT: u64 = 200;

/// Runtime code size the constructor returns. Small enough that the `MegaETH` code-deposit storage
/// charge (10,000 per byte) stays well inside the transaction gas limit.
const RUNTIME_LEN: u64 = 100;

/// The canonical code-deposit compute gas for [`RUNTIME_LEN`] bytes — the charge under test.
const CODE_DEPOSIT_GAS: u64 = RUNTIME_LEN * CODEDEPOSIT;

/// Transaction gas limit: covers the constructor, the 1,000,000 `MegaETH` code-deposit storage
/// charge and the canonical charge many times over.
const TX_GAS_LIMIT: u64 = 10_000_000;

/// Init code that returns `len` zero bytes from memory, and nothing else.
fn return_zeros_initcode(len: u64) -> Bytes {
    BytecodeBuilder::default().push_number(len).push_number(0u64).append(RETURN).build()
}

/// The address the first CREATE from [`CALLER`] deploys to.
fn deployed_address() -> Address {
    CALLER.create(0)
}

/// Whether the produced state actually carries deployed code at `address`.
///
/// Reads the state delta rather than the journal: it is what the transaction reports as committed,
/// so it answers the question a caller of the CREATE would ask.
fn has_deployed_code(state: &EvmState, address: Address) -> bool {
    state
        .get(&address)
        .map(|account| {
            account.info.code.as_ref().is_some_and(|code| !code.is_empty()) ||
                account.info.code_hash != revm::primitives::KECCAK_EMPTY
        })
        .unwrap_or(false)
}

/// Runs `init_code` as a creation transaction under `spec` with `limits`.
fn create(spec: MegaSpecId, limits: EvmTxRuntimeLimits, init_code: Bytes) -> Outcome {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10 * ONE_ETH));
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .kind(TxKind::Create)
        .gas_limit(TX_GAS_LIMIT)
        .gas_price(0)
        .data(init_code)
        .build_fill();
    transact_tx(spec, db, limits, tx, &default_envs())
}

/// The unconstrained run of the standard fixture: a CREATE that deposits [`RUNTIME_LEN`] bytes with
/// no resource limit in the way. Used to calibrate the limits the constrained runs sit against.
fn calibrate(spec: MegaSpecId) -> Outcome {
    let outcome = create(spec, EvmTxRuntimeLimits::no_limits(), return_zeros_initcode(RUNTIME_LEN));
    assert!(
        outcome.is_success(),
        "{spec:?}: the calibration run must deploy: {:?}",
        outcome.result
    );
    assert!(
        has_deployed_code(&outcome.state, deployed_address()),
        "{spec:?}: the calibration run must leave code behind",
    );
    outcome
}

/// The revert payload of an absorbed frame-local limit exceed.
fn revert_payload(label: &str, outcome: &Outcome) -> Bytes {
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("{label}: expected a revert, got {other:?}"),
    }
}

/// A frame-local compute exceed produced by the code-deposit charge stops the deposit, and REX7
/// records nothing for it.
///
/// The compute limit is set one gas below what the whole transaction needs, so the constructor runs
/// to a successful RETURN and only the code-deposit charge is unaffordable. REX6 is run beside it:
/// the two must produce the same failure, the same revert payload and the same receipt, and differ
/// only in the compute total they report — REX6 by exactly the charge it recorded and then did not
/// spend.
#[test]
fn test_create_frame_local_code_deposit_exceed_records_nothing() {
    let deployed = calibrate(MegaSpecId::REX7);
    let full_compute = deployed.compute_gas;
    assert_eq!(
        calibrate(MegaSpecId::REX6).compute_gas,
        full_compute,
        "the specs must agree on a successful CREATE's compute total, or the shared limit below \
         would not mean the same thing to both",
    );

    let limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(full_compute - 1);
    let rex6 = create(MegaSpecId::REX6, limits, return_zeros_initcode(RUNTIME_LEN));
    let rex7 = create(MegaSpecId::REX7, limits, return_zeros_initcode(RUNTIME_LEN));

    // The frame fails, and its journal fails with it: no code is deployed under either spec.
    for (label, outcome) in [("REX6", &rex6), ("REX7", &rex7)] {
        assert!(
            matches!(outcome.result, ExecutionResult::Revert { .. }),
            "{label}: the frame-local exceed must be absorbed into a revert: {:?}",
            outcome.result,
        );
        assert!(
            !has_deployed_code(&outcome.state, deployed_address()),
            "{label}: a reverted CREATE must leave no code behind",
        );
    }

    // The parent's view is bit-identical: same payload, same receipt.
    assert_eq!(
        revert_payload("REX7", &rex7),
        revert_payload("REX6", &rex6),
        "the revert payload must not change",
    );
    assert_eq!(
        MegaLimitExceeded::abi_decode(&revert_payload("REX7", &rex7))
            .expect("the payload must be a MegaLimitExceeded")
            .kind,
        LimitKind::ComputeGas.as_u8(),
        "the revert must blame compute gas",
    );
    assert_eq!(rex7.gas_used, rex6.gas_used, "the receipt's gas must not change");

    // What does change is the compute total: REX6 holds a charge nobody spent, REX7 does not.
    assert_eq!(
        rex6.compute_gas,
        rex7.compute_gas + CODE_DEPOSIT_GAS,
        "REX6 must keep recording the unspent code-deposit charge (rex6={}, rex7={})",
        rex6.compute_gas,
        rex7.compute_gas,
    );
    assert_eq!(
        rex7.compute_gas,
        full_compute - CODE_DEPOSIT_GAS,
        "REX7's total must be the successful run's minus exactly the charge that never happened",
    );

    // Nothing was destroyed and nothing was latched: with the charge not made, the transaction is
    // within its limits and the reverted frame is an ordinary revert.
    assert_eq!(rex7.destroyed, 0, "a reverted frame keeps its gas; nothing is destroyed");
    assert_eq!(rex7.booked_destroyed, 0, "no site may book a destroyed remainder here");
    assert_eq!(
        rex7.enforced(),
        rex7.compute_gas,
        "the whole reported total enforces when nothing is destroyed",
    );
}

/// The knife edge of the same decision: the compute limit that exactly affords the charge deploys,
/// one gas less reverts. Both sides stay accounted for.
#[test]
fn test_create_code_deposit_charge_knife_edge() {
    let full_compute = calibrate(MegaSpecId::REX7).compute_gas;

    let exact = create(
        MegaSpecId::REX7,
        EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(full_compute),
        return_zeros_initcode(RUNTIME_LEN),
    );
    assert!(
        exact.is_success(),
        "a limit exactly equal to the transaction's compute total must deploy: {:?}",
        exact.result,
    );
    assert!(
        has_deployed_code(&exact.state, deployed_address()),
        "the exactly-affordable CREATE must leave code behind",
    );
    assert_eq!(
        exact.compute_gas, full_compute,
        "the affordable charge is recorded like any other work",
    );

    let short = create(
        MegaSpecId::REX7,
        EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(full_compute - 1),
        return_zeros_initcode(RUNTIME_LEN),
    );
    assert!(
        matches!(short.result, ExecutionResult::Revert { .. }),
        "one gas short must revert: {:?}",
        short.result,
    );
    assert!(
        !has_deployed_code(&short.state, deployed_address()),
        "one gas short must leave no code behind",
    );
    assert_eq!(
        short.compute_gas,
        full_compute - CODE_DEPOSIT_GAS,
        "one gas short must record the frame's work and none of the charge",
    );
}

/// The same decision reached through a dimension that is not compute gas.
///
/// The data-size tracker records the deployed code's size as the frame ends, one step before the
/// code-deposit charge is settled. When that record puts the frame over its data-size budget the
/// frame reverts, so revm never charges the deposit — and REX7 must not have recorded it either.
#[test]
fn test_create_data_size_exceed_at_frame_exit_records_no_code_deposit() {
    let deployed = calibrate(MegaSpecId::REX7);
    let full_compute = deployed.compute_gas;
    let full_data_size = deployed.data_size;

    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(full_data_size - 1);
    let rex6 = create(MegaSpecId::REX6, limits, return_zeros_initcode(RUNTIME_LEN));
    let rex7 = create(MegaSpecId::REX7, limits, return_zeros_initcode(RUNTIME_LEN));

    for (label, outcome) in [("REX6", &rex6), ("REX7", &rex7)] {
        assert!(
            matches!(outcome.result, ExecutionResult::Revert { .. }),
            "{label}: the data-size exceed must be absorbed into a revert: {:?}",
            outcome.result,
        );
        assert!(
            !has_deployed_code(&outcome.state, deployed_address()),
            "{label}: a reverted CREATE must leave no code behind",
        );
    }
    assert_eq!(
        MegaLimitExceeded::abi_decode(&revert_payload("REX7", &rex7))
            .expect("the payload must be a MegaLimitExceeded")
            .kind,
        LimitKind::DataSize.as_u8(),
        "the revert must blame data size",
    );
    assert_eq!(rex7.gas_used, rex6.gas_used, "the receipt's gas must not change");
    assert_eq!(
        rex7.compute_gas,
        full_compute - CODE_DEPOSIT_GAS,
        "a frame that failed on another dimension must not be charged for a deposit it never made",
    );
    assert_eq!(
        rex6.compute_gas, full_compute,
        "REX6 keeps recording the charge whatever the frame's fate",
    );
}

/// The reverted CREATE's other tracked usage goes with it: the state growth of an account that was
/// never deployed is discarded when the frame pops.
#[test]
fn test_create_frame_local_exceed_discards_state_growth() {
    let deployed = calibrate(MegaSpecId::REX7);
    assert!(
        deployed.state_growth > 0,
        "the successful CREATE must record state growth for the new account",
    );

    let short = create(
        MegaSpecId::REX7,
        EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(deployed.compute_gas - 1),
        return_zeros_initcode(RUNTIME_LEN),
    );
    assert!(matches!(short.result, ExecutionResult::Revert { .. }), "{:?}", short.result);
    assert_eq!(
        short.state_growth, 0,
        "the reverted frame's state growth must be discarded with the frame",
    );
}

/// Init code that detains the transaction, burns most of the detained budget in one memory
/// expansion, and then returns `len` bytes of runtime code.
///
/// `TIMESTAMP` caps the transaction's remaining compute gas relative to usage at that point; the
/// `MSTORE` then spends nearly all of that cap. The frame's own budget is untouched by detention,
/// so what the code-deposit charge runs into afterwards is the transaction limit alone.
fn detained_burn_initcode(mstore_offset: u64, len: u64) -> Bytes {
    BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .push_number(0u64)
        .push_number(mstore_offset)
        .append(MSTORE)
        .push_number(len)
        .push_number(0u64)
        .append(RETURN)
        .build()
}

/// A TX-level exceed produced by the code-deposit charge halts the transaction and rescues its
/// remaining gas, and REX7 records nothing for the charge that caused it.
///
/// Detention is what separates the two budgets: it lowers the transaction's compute limit without
/// touching the frame's, so the charge can be unaffordable for the transaction while the frame
/// still has room. The halt must keep blaming detention, which it can no longer read off usage —
/// the charge that crossed the limit was never recorded.
#[test]
fn test_create_tx_level_code_deposit_exceed_halts_and_blames_detention() {
    // Memory offset chosen so the expansion costs ~19.9M of the 20M detention cap, leaving less
    // than the 200,000-gas code-deposit charge but more than zero.
    const BURN_OFFSET: u64 = 3_205_568;
    const DETAINED_RUNTIME_LEN: u64 = 1_000;
    const DETAINED_TX_GAS_LIMIT: u64 = 50_000_000;

    let init_code = detained_burn_initcode(BURN_OFFSET, DETAINED_RUNTIME_LEN);
    let run = |spec: MegaSpecId| {
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10 * ONE_ETH));
        let tx = TxEnvBuilder::default()
            .caller(CALLER)
            .kind(TxKind::Create)
            .gas_limit(DETAINED_TX_GAS_LIMIT)
            .gas_price(0)
            .data(init_code.clone())
            .build_fill();
        transact_tx(spec, db, EvmTxRuntimeLimits::from_spec(spec), tx, &default_envs())
    };

    let rex6 = run(MegaSpecId::REX6);
    let rex7 = run(MegaSpecId::REX7);

    for (label, outcome) in [("REX6", &rex6), ("REX7", &rex7)] {
        assert!(
            matches!(outcome.halt_reason(label), MegaHaltReason::VolatileDataAccessOutOfGas { .. }),
            "{label}: the halt must blame detention: {:?}",
            outcome.result,
        );
        assert!(
            !has_deployed_code(&outcome.state, deployed_address()),
            "{label}: a halted CREATE must leave no code behind",
        );
    }

    assert_eq!(
        rex7.gas_used, rex6.gas_used,
        "the rescued receipt must not change (rex6={}, rex7={})",
        rex6.gas_used, rex7.gas_used,
    );

    // What proves the halt came from the charge rather than from usage crossing on its own: REX7's
    // recorded usage never reaches the detained limit, so the only thing left that can classify
    // this halt as detention is the flag the charge's settlement set.
    assert!(
        rex7.enforced() <= rex7.detained_compute_gas_limit,
        "REX7's usage must stay within the detained limit (usage={}, limit={})",
        rex7.enforced(),
        rex7.detained_compute_gas_limit,
    );
    assert!(
        rex6.enforced() > rex6.detained_compute_gas_limit,
        "REX6's usage crosses the limit because it recorded the charge (usage={}, limit={})",
        rex6.enforced(),
        rex6.detained_compute_gas_limit,
    );
    assert_eq!(
        rex6.compute_gas,
        rex7.compute_gas + DETAINED_RUNTIME_LEN * CODEDEPOSIT,
        "REX6 records the charge that halted it, REX7 does not (rex6={}, rex7={})",
        rex6.compute_gas,
        rex7.compute_gas,
    );
}

/// The frozen shapes this decision sits on top of.
///
/// Deciding the charge before the action is processed is what keeps a CREATE's journal and its
/// reported result in agreement, and that has been true since REX5 — REX4 is the last spec that
/// reports a revert over a committed deployment. REX7 changes what is recorded, not this.
#[test]
fn test_frozen_specs_keep_their_create_journal_shapes() {
    let full_compute = calibrate(MegaSpecId::REX5).compute_gas;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(full_compute - 1);

    let rex4 = create(MegaSpecId::REX4, limits, return_zeros_initcode(RUNTIME_LEN));
    assert!(
        matches!(rex4.result, ExecutionResult::Revert { .. }),
        "REX4 reports a revert: {:?}",
        rex4.result,
    );
    assert!(
        has_deployed_code(&rex4.state, deployed_address()),
        "REX4 keeps its split outcome: the deployment stands under the reverted result",
    );

    for spec in [MegaSpecId::REX5, MegaSpecId::REX6, MegaSpecId::REX7] {
        let outcome = create(spec, limits, return_zeros_initcode(RUNTIME_LEN));
        assert!(
            matches!(outcome.result, ExecutionResult::Revert { .. }),
            "{spec:?} reports a revert: {:?}",
            outcome.result,
        );
        assert!(
            !has_deployed_code(&outcome.state, deployed_address()),
            "{spec:?} must roll the deployment back with the result",
        );
    }
}

/// A per-byte code-deposit rate one gas above revm's built-in [`CODEDEPOSIT`] — the smallest
/// deviation from the schedule `REX7` defines, and one no configuration may carry.
const OVERRIDDEN_CODEDEPOSIT: u64 = CODEDEPOSIT + 1;

/// Runs [`return_zeros_initcode`] as a REX7 creation transaction with `gas_limit`, under a
/// configuration whose gas schedule charges `rate` gas per deployed byte.
///
/// The shared helpers all take the context's default configuration, so this builds the context
/// itself — everything else about the transaction matches [`create`]. A `rate` other than the
/// one `REX7`'s schedule defines makes the configuration inadmissible, and `with_cfg` panics
/// before any transaction runs.
fn create_at_rate(rate: u64, gas_limit: u64) -> Outcome {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(10 * ONE_ETH));
    let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX7);
    cfg.gas_params.override_gas([(GasId::code_deposit_cost(), rate)]);
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX7)
        .with_cfg(cfg)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .kind(TxKind::Create)
        .gas_limit(gas_limit)
        .gas_price(0)
        .data(return_zeros_initcode(RUNTIME_LEN))
        .build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let outcome = evm.execute_transaction(tx).expect("tx should not surface EVMError");
    let (detained_compute_gas_limit, non_compute_gas, minted_call_stipend, booked_destroyed) = {
        let additional_limit = EvmTr::ctx_ref(&evm).additional_limit.borrow();
        let (non_compute_gas, minted_call_stipend, booked_destroyed) =
            additional_limit.conservation_terms_for_test();
        (
            additional_limit.detained_compute_gas_limit(),
            non_compute_gas,
            minted_call_stipend,
            booked_destroyed,
        )
    };
    finish(
        MegaSpecId::REX7,
        outcome,
        detained_compute_gas_limit,
        non_compute_gas,
        minted_call_stipend,
        booked_destroyed,
    )
}

/// Installing a rate explicitly is not itself a deviation: a schedule that names revm's built-in
/// per-byte rate is the schedule `REX7` defines, is admitted, and measures exactly what the
/// shared helper measures on the default configuration.
#[test]
fn test_installing_the_built_in_code_deposit_rate_changes_nothing() {
    let explicit = create_at_rate(CODEDEPOSIT, TX_GAS_LIMIT);

    assert!(explicit.is_success(), "the explicit-rate run must deploy: {:?}", explicit.result);
    assert!(
        has_deployed_code(&explicit.state, deployed_address()),
        "the explicit-rate run must leave code behind",
    );
    assert_eq!(
        explicit.compute_gas,
        calibrate(MegaSpecId::REX7).compute_gas,
        "naming the built-in rate must not change what the shared helper measures",
    );
    assert_eq!(explicit.destroyed, 0, "a successful CREATE destroys nothing");
    assert_eq!(
        explicit.enforced(),
        explicit.compute_gas,
        "the whole reported total enforces when nothing is destroyed",
    );
}

/// A schedule that charges a different per-byte rate never runs. `MegaETH`'s gas schedule is
/// defined by the spec, and a configuration carrying any other one is rejected where it enters
/// rather than executed — which is what keeps the charge revm debits at the create-return equal
/// to the one the tracker weighed and recorded a step earlier.
#[test]
#[should_panic(expected = "gas params differ from the spec-defined schedule")]
fn test_a_code_deposit_rate_off_the_spec_schedule_is_rejected() {
    let _ = create_at_rate(OVERRIDDEN_CODEDEPOSIT, TX_GAS_LIMIT);
}
