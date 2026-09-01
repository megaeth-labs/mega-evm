//! The gas a synthetic outcome carries.
//!
//! A `frame_start` / `call` / `create` callback that returns `Some(outcome)` answers the frame
//! itself: no frame is built, `frame_init` never runs, and the number the caller reclaims is
//! whatever `Gas` the inspector put in that outcome. Nothing about it is derived from the
//! execution — the inspector chooses it outright — so it is a gas figure the transaction's
//! accounting has to be told about, exactly like an edit to a result the EVM did produce.
//!
//! The tests here are laid out over the sign of that choice, because the two directions settle
//! differently and a lane that books one and drops the other is a real failure mode:
//!
//! - an outcome that hands back **less** than the envelope makes the caller spend gas no frame ever
//!   performed work for;
//! - an outcome that hands back **more** conjures gas the transaction never funded;
//! - an outcome that hands back **exactly** the envelope — the echo convention every tracer that
//!   intercepts follows — moves nothing, and must book nothing.
//!
//! The halt direction is the asymmetry: a halting outcome hands nothing back at all, so what the
//! inspector wrote in the gas figure changes nothing the transaction spends, and the destroyed
//! remainder is settled against the envelope instead.

use crate::common::{CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    AdditionalLimit, ConservationTerms, EmptyExternalEnv, EvmTxRuntimeLimits, InspectorLedger,
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, MSTORE, MSTORE8, POP, RETURN, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::{EvmTr, FrameResult},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult, InterpreterTypes,
    },
    Inspector,
};
use std::vec::Vec;

/// High enough that EVM gas is never what binds.
const TX_GAS_LIMIT: u64 = 100_000_000;
/// Gas the fixture's `CALL` forwards, and the envelope every interception is measured against.
const FORWARDED: u64 = 50_000;

/// Everything one transaction reports, plus what the shim booked for it.
struct Reading {
    result: ExecutionResult<MegaHaltReason>,
    compute_gas: u64,
    enforced: u64,
    destroyed: u64,
    total_gas_spent: u64,
    terms: ConservationTerms,
    ledger: InspectorLedger,
}

/// The conservation identity, stated with the term the measurement shim contributes.
fn assert_identity(label: &str, r: &Reading) {
    assert_eq!(
        r.compute_gas,
        r.enforced + r.destroyed,
        "{label}: reported compute must split into enforced + destroyed",
    );
    assert_eq!(
        r.terms.inspector_conjured_gas,
        r.ledger.conjured_gas(),
        "{label}: the law's `I` term is the ledger's net, and nothing else",
    );
    assert_eq!(
        r.terms.envelope_for(r.destroyed),
        i128::from(r.total_gas_spent),
        "{label}: the law must close against the envelope the receipt reports; \
         reported compute={} destroyed={} envelope={} ({})",
        r.compute_gas,
        r.destroyed,
        r.total_gas_spent,
        r.terms,
    );
}

fn tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(TX_GAS_LIMIT).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn context_for(
    db: &mut MemoryDatabase,
    spec: MegaSpecId,
) -> MegaContext<&mut MemoryDatabase, EmptyExternalEnv> {
    let mut context =
        MegaContext::new(db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

fn read(limit: &AdditionalLimit, outcome: MegaTransactionOutcome) -> Reading {
    assert_eq!(
        outcome.inspector_ledger,
        limit.inspector_ledger(),
        "the outcome must report the ledger the shim booked, unchanged",
    );
    let total_gas_spent = outcome.result_and_state.result.gas().total_gas_spent();
    Reading {
        result: outcome.result_and_state.result,
        compute_gas: outcome.compute_gas_used,
        enforced: outcome.compute_gas_enforced,
        destroyed: outcome.compute_gas_destroyed,
        total_gas_spent,
        terms: limit.conservation_terms(),
        ledger: outcome.inspector_ledger,
    }
}

fn transact_on<I>(spec: MegaSpecId, mut db: MemoryDatabase, inspector: &mut I) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let mut evm = MegaEvm::new(context_for(&mut db, spec)).with_inspector(inspector);
    let outcome = evm.execute_transaction(tx()).expect("tx should not surface EVMError");
    let reading = read(&evm.ctx_ref().additional_limit.borrow(), outcome);
    reading
}

fn transact<I>(db: MemoryDatabase, inspector: &mut I) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    transact_on(MegaSpecId::REX7, db, inspector)
}

/// A straight run of plain opcodes that always succeeds.
fn plain_run_code(pairs: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder.append(STOP).build()
}

/// The entry contract: one `CALL` to [`CALLEE`] forwarding [`FORWARDED`], then `STOP`.
fn call_fixture() -> MemoryDatabase {
    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(u128::from(FORWARDED))
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        .account_code(CALLEE, plain_run_code(20))
}

/// How an interception sizes the `Gas` it hands back, relative to the envelope it was given.
#[derive(Clone, Copy, Debug)]
enum Sizing {
    /// The echo convention: exactly the envelope.
    Echo,
    /// Half of it — the caller spends the other half for work no frame performed.
    Half,
    /// None of it.
    Zero,
    /// More than it — gas the transaction never funded.
    Excess(u64),
}

impl Sizing {
    fn gas(self, envelope: u64) -> u64 {
        match self {
            Self::Echo => envelope,
            Self::Half => envelope / 2,
            Self::Zero => 0,
            Self::Excess(extra) => envelope + extra,
        }
    }

    /// What the ledger must carry for this sizing, as a signed movement from the envelope.
    fn expected_delta(self, envelope: u64) -> i128 {
        i128::from(self.gas(envelope)) - i128::from(envelope)
    }
}

/// Intercepts the call to [`CALLEE`], sizing the outcome's gas by [`Sizing`].
struct CallInterceptor {
    sizing: Sizing,
    classification: InstructionResult,
    intercepted: u64,
    envelope: u64,
}

impl CallInterceptor {
    fn new(sizing: Sizing, classification: InstructionResult) -> Self {
        Self { sizing, classification, intercepted: 0, envelope: 0 }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallInterceptor {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address != CALLEE {
            return None;
        }
        self.intercepted += 1;
        self.envelope = inputs.gas_limit;
        Some(CallOutcome::new(
            InterpreterResult::new(
                self.classification,
                Bytes::new(),
                Gas::new(self.sizing.gas(inputs.gas_limit)),
            ),
            inputs.return_memory_offset.clone(),
        ))
    }
}

/// An outcome that hands back less than the envelope makes the caller spend gas nothing performed.
#[test]
fn test_a_half_gas_interception_books_the_gas_it_took_from_the_caller() {
    let mut inspector = CallInterceptor::new(Sizing::Half, InstructionResult::Stop);
    let reading = transact(call_fixture(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(inspector.envelope, FORWARDED, "fixture check: the forwarded envelope");
    assert!(reading.result.is_success(), "fixture check: {:?}", reading.result);
    assert_eq!(
        reading.ledger,
        InspectorLedger {
            result: Sizing::Half.expected_delta(FORWARDED),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the half the outcome withheld is gas the inspector destroyed",
    );
    assert_identity("half-gas interception", &reading);
}

/// The extreme of the same direction: the outcome hands back nothing at all.
#[test]
fn test_a_zero_gas_interception_books_the_whole_envelope() {
    let mut inspector = CallInterceptor::new(Sizing::Zero, InstructionResult::Stop);
    let reading = transact(call_fixture(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.ledger,
        InspectorLedger {
            result: Sizing::Zero.expected_delta(FORWARDED),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "an outcome that returns nothing consumed the whole envelope",
    );
    assert_identity("zero-gas interception", &reading);
}

/// The other direction: an outcome that hands back more than it was given conjures the difference.
#[test]
fn test_an_over_funded_interception_books_the_gas_it_conjured() {
    const EXTRA: u64 = 7_000;
    let mut inspector = CallInterceptor::new(Sizing::Excess(EXTRA), InstructionResult::Stop);
    let reading = transact(call_fixture(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.ledger,
        InspectorLedger {
            result: Sizing::Excess(EXTRA).expected_delta(FORWARDED),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "gas the transaction never funded is gas the inspector conjured",
    );
    assert_identity("over-funded interception", &reading);
}

/// The echo convention moves nothing, and must book nothing.
///
/// This is the shape every tool that intercepts actually uses, and the reason the lane could go
/// missing for as long as it did: with the envelope echoed back the accounting closes whether or
/// not anything measures it. Pinning the zero is what says the lane is measuring rather than
/// coincidentally agreeing.
#[test]
fn test_an_echoing_interception_books_no_gas_at_all() {
    for classification in [InstructionResult::Stop, InstructionResult::Revert] {
        let mut inspector = CallInterceptor::new(Sizing::Echo, classification);
        let reading = transact(call_fixture(), &mut inspector);

        assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
        assert_eq!(
            reading.ledger,
            InspectorLedger { interventions: 1, ..InspectorLedger::default() },
            "{classification:?}: an echoed envelope moves no gas, so no gas lane may move",
        );
        assert_eq!(reading.ledger.conjured_gas(), 0, "{classification:?}");
        assert_identity("echoing interception", &reading);
    }
}

/// A halting outcome hands nothing back, so what the inspector wrote in its gas figure changes
/// nothing the transaction spends — and the envelope is destroyed whole.
#[test]
fn test_a_halting_interception_destroys_the_envelope_whatever_gas_it_reports() {
    for sizing in [Sizing::Echo, Sizing::Half, Sizing::Zero, Sizing::Excess(7_000)] {
        let mut inspector = CallInterceptor::new(sizing, InstructionResult::OutOfGas);
        let reading = transact(call_fixture(), &mut inspector);

        assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
        assert!(reading.result.is_success(), "the caller absorbs the halt: {:?}", reading.result);
        assert_eq!(
            reading.ledger,
            InspectorLedger { interventions: 1, ..InspectorLedger::default() },
            "{sizing:?}: a halting frame hands nothing back, so no gas lane may move",
        );
        assert_eq!(
            reading.destroyed, FORWARDED,
            "{sizing:?}: the whole envelope is destroyed, whatever the outcome claimed",
        );
        assert_identity("halting interception", &reading);
    }
}

/// The generic callback intercepts too, and is measured by the same rule.
///
/// revm runs `frame_start` before the variant-specific `call` / `create`, and an outcome returned
/// there skips both. A lane wired only to the variant hooks would leave this one unmeasured.
#[test]
fn test_the_generic_frame_start_interception_is_measured_too() {
    /// Intercepts the call to [`CALLEE`] from the generic callback, handing back half.
    #[derive(Default)]
    struct GenericInterceptor {
        intercepted: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for GenericInterceptor {
        fn frame_start(
            &mut self,
            _context: &mut CTX,
            frame_input: &mut FrameInput,
        ) -> Option<FrameResult> {
            let FrameInput::Call(inputs) = frame_input else { return None };
            if inputs.target_address != CALLEE {
                return None;
            }
            self.intercepted += 1;
            Some(FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit / 2),
                ),
                inputs.return_memory_offset.clone(),
            )))
        }
    }

    let mut inspector = GenericInterceptor::default();
    let reading = transact(call_fixture(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.ledger,
        InspectorLedger {
            result: Sizing::Half.expected_delta(FORWARDED),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the generic callback's interception books on the same lane as the variant one's",
    );
    assert_identity("frame_start interception", &reading);
}

/// Init code that writes one slot and returns two bytes of runtime code.
fn init_code() -> Vec<u8> {
    BytecodeBuilder::default()
        .sstore(U256::from(0x30), U256::from(1))
        .push_number(0x6000u64)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(2u64) // size
        .push_number(30u64) // offset
        .append(RETURN)
        .build()
        .to_vec()
}

/// The entry contract: one `CREATE`, then `STOP`.
fn create_fixture() -> MemoryDatabase {
    let init = init_code();
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    let code = builder
        .push_number(init.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// A creation answered by the inspector is measured against the envelope its `CREATE` forwarded.
///
/// The envelope is not a constant here — `CREATE` forwards all but a sixty-fourth of what the
/// caller holds — so the test reads it back from the callback rather than asserting a figure.
#[test]
fn test_an_intercepted_creation_is_measured_against_the_envelope_it_was_handed() {
    /// Intercepts the creation, handing back half of what it was given.
    #[derive(Default)]
    struct CreateInterceptor {
        intercepted: u64,
        envelope: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CreateInterceptor {
        fn create(
            &mut self,
            _context: &mut CTX,
            inputs: &mut CreateInputs,
        ) -> Option<CreateOutcome> {
            self.intercepted += 1;
            self.envelope = inputs.gas_limit();
            Some(CreateOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit() / 2),
                ),
                None,
            ))
        }
    }

    let mut inspector = CreateInterceptor::default();
    let reading = transact(create_fixture(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one creation");
    assert!(inspector.envelope > 0, "fixture check: the creation must forward an envelope");
    assert_eq!(
        reading.ledger,
        InspectorLedger {
            result: Sizing::Half.expected_delta(inspector.envelope),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "a creation's interception is measured against the envelope its CREATE forwarded",
    );
    assert_identity("intercepted creation", &reading);
}

/// The envelope an interception is measured against is the one the callback *received*.
///
/// A callback is free to edit the inputs and then answer the frame itself. The edit reaches no
/// frame — nothing is built from those inputs — so the envelope the caller actually funded is the
/// one the callback was handed, and an outcome echoing the *edited* limit hands back more than
/// that. Measuring against the post-edit number instead would read this run as conjuring nothing.
#[test]
fn test_the_envelope_is_the_one_the_callback_received_not_the_one_it_left() {
    const BONUS: u64 = 9_000;

    /// Raises the child's gas limit and then intercepts, echoing the raised figure.
    #[derive(Default)]
    struct RaisingInterceptor {
        intercepted: u64,
    }

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for RaisingInterceptor {
        fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
            if inputs.target_address != CALLEE {
                return None;
            }
            self.intercepted += 1;
            inputs.gas_limit += BONUS;
            Some(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            ))
        }
    }

    let mut inspector = RaisingInterceptor::default();
    let reading = transact(call_fixture(), &mut inspector);

    assert_eq!(inspector.intercepted, 1, "the fixture must intercept exactly one call");
    assert_eq!(
        reading.ledger,
        InspectorLedger {
            result: i128::from(BONUS),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "the bonus reaches the caller through the outcome, so it is booked once, on the result \
         lane — the env lane stays empty because no frame was ever built from those inputs",
    );
    assert_identity("raised then intercepted", &reading);
}

/// The lane reports on a frozen spec too, and reporting it settles nothing there.
///
/// The measurement is not REX7-gated, and neither are the two lanes it joins: `InspectorLedger` is
/// what the canonical block path's guard reads, so a frame an inspector answered has to be visible
/// on it whatever spec is executing. What is REX7's alone is the settlement the lane feeds — the
/// envelope a refused frame init decides the fate of. REX6 derives nothing from the envelope and
/// books no destroyed remainder, so what it reports is what it always reported.
///
/// The transaction's own gas does follow the figure the inspector wrote, on both specs. That is
/// the EVM handing the caller back what the result carries, which is upstream's arithmetic rather
/// than `MegaETH`'s, and it is the movement the lane exists to account for rather than to prevent.
#[test]
fn test_a_frozen_spec_reports_the_lane_without_settling_anything() {
    let mut echoing = CallInterceptor::new(Sizing::Echo, InstructionResult::Stop);
    let echo = transact_on(MegaSpecId::REX6, call_fixture(), &mut echoing);
    let mut halving = CallInterceptor::new(Sizing::Half, InstructionResult::Stop);
    let half = transact_on(MegaSpecId::REX6, call_fixture(), &mut halving);

    assert_eq!(echoing.intercepted, 1, "fixture check");
    assert_eq!(halving.intercepted, 1, "fixture check");
    assert_eq!(
        echo.ledger,
        InspectorLedger { interventions: 1, ..InspectorLedger::default() },
        "REX6: an echoed envelope moves no gas here either",
    );
    assert_eq!(
        half.ledger,
        InspectorLedger {
            result: Sizing::Half.expected_delta(FORWARDED),
            interventions: 1,
            ..InspectorLedger::default()
        },
        "REX6: the lane reports, because the block guard has to see this frame on every spec",
    );
    assert_eq!(
        (echo.destroyed, half.destroyed),
        (0, 0),
        "REX6 has no destroyed remainder to book, on either sizing",
    );
    assert_eq!(
        echo.compute_gas, half.compute_gas,
        "and its compute total does not follow the figure the inspector wrote",
    );
    assert_eq!(
        half.total_gas_spent - echo.total_gas_spent,
        FORWARDED / 2,
        "the caller really did lose the half the outcome withheld — that is the EVM's arithmetic",
    );
}
