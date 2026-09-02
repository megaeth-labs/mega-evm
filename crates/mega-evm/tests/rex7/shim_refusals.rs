//! The rewrites the shim refuses outright, and the near boundary of each refusal.
//!
//! Almost everything an inspector does is measured and booked. Two shapes are not, because
//! honouring them would produce a receipt that contradicts state the EVM had already decided on
//! before any callback ran:
//!
//! - **A failed contract creation rewritten into a successful one.** By the time `create_end` or
//!   `frame_end` runs, revm has reverted the frame's checkpoint and declined to deposit any code,
//!   so the rewrite reports a deployment that did not happen. Both callbacks are covered, because a
//!   refusal wired only to the earlier one is one an inspector can step past.
//! - **The classification of a result frame init produced.** A precompile, an empty-code call, and
//!   the `KeylessDeploy` interceptor's synthetic result all come back out of frame init with the
//!   journal decision behind them already taken and no frame checkpoint left to unwind. What each
//!   case below pins is the state that decision left behind, and that the caller is not told
//!   something else about it.
//!
//! The near boundary is a frame the inspector answered *itself*. That result comes out of frame
//! init too, and it is not refused — nothing in the EVM decided anything for it, so there is
//! nothing for the rewrite to contradict.
//!
//! Both halves surface the same way in both builds: a debug build asserts (the shape is a
//! detector, and a corpus that produces it should stop), a release build fails the transaction
//! with the same message.

use crate::{
    common::{base_db, context, try_drive, CALLEE, CALLER, CONTRACT, EMPTY_TARGET, ONE_ETH},
    inspector_common::{
        append_call, assert_refused, deploy_then_stop, limits, try_transact_inspected,
        REVERTING_INIT_CODE, REVIVED_CREATION,
    },
};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, MegaTransactionNew as _, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{RETURN, SSTORE, STOP},
    context::{result::ExecutionResult, tx::TxEnvBuilder, ContextTr},
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult, InterpreterTypes,
    },
    state::EvmState,
    Inspector,
};
use std::{string::String, vec::Vec};

// === a failed creation, revived ===============================================================

/// Rewrites every failed contract creation into a successful one, from `create_end`.
#[derive(Default)]
struct CreateReviver;

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CreateReviver {
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if !outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Return;
        }
    }
}

/// A `create_end` that turns a failed contract creation into a successful one is refused, loudly.
///
/// By that point revm has already reverted the frame's journal checkpoint and already declined to
/// deposit any code, so the rewrite would report a deployment that did not happen. The shim
/// restores the original classification and refuses to let the transaction produce a receipt at
/// all.
#[test]
fn test_reviving_a_failed_creation_is_refused() {
    let db = base_db(deploy_then_stop(&REVERTING_INIT_CODE));

    assert_refused(REVIVED_CREATION, || {
        let mut inspector = CreateReviver;
        try_transact_inspected(db.clone(), limits(), &mut inspector)
    });
}

/// `frame_end` is the last callback that can rewrite a creation's classification, and the refusal
/// covers it too.
///
/// `measured_inspector.rs` pins the `create_end` form. This is the one callback later: revm calls
/// `create_end` first and `frame_end` after it, so an inspector that leaves `create_end` alone and
/// rewrites in `frame_end` would slip past a refusal wired only to the earlier one.
#[test]
fn test_reviving_a_failed_creation_is_refused_at_frame_end() {
    /// Rewrites a failed creation into a success, from `frame_end` only.
    struct LateReviver;

    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for LateReviver {
        fn frame_end(
            &mut self,
            _context: &mut CTX,
            _frame_input: &FrameInput,
            frame_result: &mut FrameResult,
        ) {
            let FrameResult::Create(outcome) = frame_result else { return };
            if !outcome.result.result.is_ok() {
                outcome.result.result = InstructionResult::Stop;
            }
        }
    }

    let db = base_db(deploy_then_stop(&REVERTING_INIT_CODE));

    assert_refused(REVIVED_CREATION, || {
        try_transact_inspected(db.clone(), limits(), &mut LateReviver)
    });
}

// === the classification of a result frame init produced =======================================

/// Transaction gas limit: high enough that EVM gas never binds.
const TX_GAS_LIMIT: u64 = 30_000_000;

/// `ecrecover`, the precompile the failing-precompile case calls.
const ECRECOVER: Address = address!("0000000000000000000000000000000000000001");

/// The relayer that sends the keyless deployment.
const RELAYER: Address = address!("0000000000000000000000000000000000340009");

/// The slot `CONTRACT` writes its `CALL`'s success flag to.
const FLAG_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// The wei the value-transferring cases send.
const SENT: u64 = 1;

/// What one run produced, in the shape the splits are asserted over.
struct Reading {
    result: Result<ExecutionResult<MegaHaltReason>, String>,
    rejected_rewrites: u32,
    state: EvmState,
}

impl Reading {
    /// The balance the produced state gives `address`, or zero when it never touched it.
    fn balance(&self, address: Address) -> U256 {
        self.state.get(&address).map(|a| a.info.balance).unwrap_or_default()
    }

    /// The value at `slot` on `address` in the produced state.
    fn storage(&self, address: Address, slot: U256) -> U256 {
        self.state
            .get(&address)
            .and_then(|a| a.storage.get(&slot))
            .map(|s| s.present_value())
            .unwrap_or_default()
    }

    /// Whether the produced state gives `address` any code.
    fn has_code(&self, address: Address) -> bool {
        self.state
            .get(&address)
            .is_some_and(|a| a.info.code_hash != B256::ZERO && !a.info.is_empty_code_hash())
    }

    /// Whether the transaction produced a receipt at all, and a successful one.
    fn succeeded(&self) -> bool {
        matches!(&self.result, Ok(r) if r.is_success())
    }
}

/// Rewrites the classification of the result of every call into `target`, once.
#[derive(Debug)]
struct RewriteInitResult {
    target: Address,
    to: InstructionResult,
    /// How many results it actually rewrote. Asserted, so a fixture that stops reaching the
    /// callback fails rather than passing as a run that rewrote nothing.
    fired: u32,
}

impl RewriteInitResult {
    const fn new(target: Address, to: InstructionResult) -> Self {
        Self { target, to, fired: 0 }
    }
}

impl<CTX, INTR> Inspector<CTX, INTR> for RewriteInitResult
where
    CTX: ContextTr,
    INTR: InterpreterTypes,
{
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address != self.target || outcome.result.result == self.to {
            return;
        }
        outcome.result.result = self.to;
        self.fired += 1;
    }
}

/// Runs `tx` under `spec` with `inspector` attached.
fn run_on<I>(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    tx: MegaTransaction,
    inspector: &mut I,
) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let limits = EvmTxRuntimeLimits::from_spec(spec);
    let mut evm = MegaEvm::new(context(&mut db, spec, limits)).with_inspector(inspector);
    match try_drive(spec, &mut evm, tx) {
        Ok(outcome) => Reading {
            result: Ok(outcome.result),
            rejected_rewrites: outcome.inspector_ledger.rejected_rewrites,
            state: outcome.state,
        },
        Err(refusal) => Reading {
            result: Err(refusal.error),
            rejected_rewrites: refusal.rejected_rewrites,
            state: EvmState::default(),
        },
    }
}

/// [`run_on`] under REX7, which is where every rewrite below is refused.
fn run<I>(db: MemoryDatabase, tx: MegaTransaction, inspector: &mut I) -> Reading
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    run_on(MegaSpecId::REX7, db, tx, inspector)
}

/// The two facts every case here pins: the rewrite was counted as refused, and the transaction
/// failed with an error rather than reporting a receipt built on it.
fn assert_reading_refused(reading: &Reading) {
    assert_eq!(reading.rejected_rewrites, 1, "the shim must count the refusal");
    assert!(
        reading.result.is_err(),
        "a refused rewrite must fail the transaction, got {:?}",
        reading.result,
    );
}

fn call_tx(to: Address) -> MegaTransaction {
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(TX_GAS_LIMIT).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// A contract that calls `target` with `value` wei and `gas`, then records whether the call
/// reported success.
///
/// The recorded flag is what makes the split visible: it is the answer the *caller* was given,
/// which the state the call left behind has to agree with.
fn calls_and_records(target: Address, gas: u64, value: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), target, gas, value)
        .push_u256(FLAG_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build()
}

/// A value-transferring `CALL` into an empty-code account, rewritten from its `Stop` into a
/// revert.
///
/// `make_call_frame` transfers the value and commits its checkpoint before returning `Stop`, so
/// the transfer is already in the journal by the time any callback runs and no journal decision is
/// left to follow the rewrite. Honouring it tells the caller its transfer failed while the
/// recipient keeps the wei.
#[test]
fn test_rewriting_an_empty_code_call_into_a_revert_is_refused() {
    let mut inspector = RewriteInitResult::new(EMPTY_TARGET, InstructionResult::Revert);
    let reading = run(
        base_db(calls_and_records(EMPTY_TARGET, 200_000, SENT)),
        call_tx(CONTRACT),
        &mut inspector,
    );
    assert_eq!(inspector.fired, 1, "the fixture must reach the callback exactly once");
    assert!(
        !reading.succeeded(),
        "the rewrite was honoured: the caller recorded the transfer as {} and {EMPTY_TARGET} \
         holds {}",
        reading.storage(CONTRACT, FLAG_SLOT),
        reading.balance(EMPTY_TARGET),
    );
    assert_reading_refused(&reading);
}

/// A value-transferring `CALL` into a precompile that cannot afford its own fee, rewritten from
/// its out-of-gas into a success.
///
/// The split runs the other way: the precompile's failure made `make_call_frame` revert its
/// checkpoint, so the transfer is already rolled back. A success there tells the caller the
/// precompile was paid.
#[test]
fn test_reviving_a_failed_precompile_call_is_refused() {
    // The 2,300 gas stipend a value-transferring call mints is under `ecrecover`'s 3,000 fee, so
    // the precompile is reached and cannot pay.
    let mut inspector = RewriteInitResult::new(ECRECOVER, InstructionResult::Stop);
    let reading =
        run(base_db(calls_and_records(ECRECOVER, 0, SENT)), call_tx(CONTRACT), &mut inspector);
    assert_eq!(inspector.fired, 1, "the fixture must reach the callback exactly once");
    assert!(
        !reading.succeeded(),
        "the rewrite was honoured: the caller recorded the call as {} and {ECRECOVER} holds {}",
        reading.storage(CONTRACT, FLAG_SLOT),
        reading.balance(ECRECOVER),
    );
    assert_reading_refused(&reading);
}

/// A deterministic pre-EIP-155 keyless deployment transaction whose init code returns one byte of
/// runtime code, so the deployment it makes is visible in the produced state.
fn keyless_tx_bytes() -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: 200_000,
        to: TxKind::Create,
        value: U256::ZERO,
        // MSTORE8 a STOP at offset 0, then return that one byte as the runtime code.
        input: BytecodeBuilder::default()
            .push_number(u128::from(STOP))
            .push_number(0u64)
            .append(0x53) // MSTORE8
            .push_number(1u64)
            .push_number(0u64)
            .append(RETURN)
            .build(),
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(buf)
}

/// The address the keyless transaction above deploys to, recovered from the receipt of an
/// unrewritten run.
fn deployed_address(reading: &Reading) -> Option<Address> {
    let Ok(ExecutionResult::Success { output, .. }) = &reading.result else { return None };
    IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
        .ok()
        .map(|returns| returns.deployedAddress)
}

/// The `KeylessDeploy` interceptor's synthetic result, rewritten across the boundary.
///
/// The interceptor runs a whole sandbox EVM and merges its state into the journal before it
/// returns, and it returns out of frame init, so there is no frame checkpoint the rewrite could
/// unwind. The deployment stands whatever the caller is told.
#[test]
fn test_rewriting_the_keyless_deploy_synthetic_result_is_refused() {
    let deploy_tx = || {
        let data = IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: keyless_tx_bytes(),
            gasLimitOverride: U256::from(1_000_000u64),
        }
        .abi_encode();
        let mut tx = MegaTransaction::new(
            TxEnvBuilder::default()
                .caller(RELAYER)
                .call(KEYLESS_DEPLOY_ADDRESS)
                .chain_id(Some(1))
                .data(Bytes::from(data))
                .gas_limit(TX_GAS_LIMIT)
                .build_fill(),
        );
        tx.enveloped_tx = Some(Bytes::new());
        tx
    };
    let db = || MemoryDatabase::default().account_balance(RELAYER, U256::from(10 * ONE_ETH));

    // The unrewritten run, which says where the deployment lands and that it lands at all.
    let mut observer = RewriteInitResult::new(Address::ZERO, InstructionResult::Stop);
    let plain = run(db(), deploy_tx(), &mut observer);
    assert!(plain.succeeded(), "the fixture must deploy, got {:?}", plain.result);
    let deployed = deployed_address(&plain).expect("the fixture must report a deployed address");
    assert!(plain.has_code(deployed), "the fixture must leave code at {deployed}");

    let mut inspector = RewriteInitResult::new(KEYLESS_DEPLOY_ADDRESS, InstructionResult::Revert);
    let reading = run(db(), deploy_tx(), &mut inspector);
    assert_eq!(inspector.fired, 1, "the fixture must reach the callback exactly once");
    assert!(
        !reading.has_code(deployed),
        "the rewrite was honoured: the caller was told {:?} and {deployed} holds the sandbox's \
         deployed code anyway",
        reading.result,
    );
    assert_reading_refused(&reading);
}

/// Answers the frame itself and then moves the classification of its own answer.
///
/// The near boundary of the refusal: this result also comes back out of frame init with no child
/// frame built, and it is not refused — because nothing in the EVM decided anything for it. No
/// checkpoint was opened and no state was written, so there is no journal decision for a later
/// rewrite to contradict.
#[derive(Debug)]
struct AnswerThenRewrite {
    target: Address,
    answered: u32,
    rewrote: u32,
}

impl<CTX, INTR> Inspector<CTX, INTR> for AnswerThenRewrite
where
    CTX: ContextTr,
    INTR: InterpreterTypes,
{
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address != self.target {
            return None;
        }
        self.answered += 1;
        Some(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Stop,
                Bytes::new(),
                Gas::new(inputs.gas_limit),
            ),
            inputs.return_memory_offset.clone(),
        ))
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address != self.target || outcome.result.result.is_revert() {
            return;
        }
        outcome.result.result = InstructionResult::Revert;
        self.rewrote += 1;
    }
}

/// A frame the inspector answered itself, and then reclassified, is supported.
///
/// The refusal is stated over what frame init *produced*, not over every result that reaches a
/// callback without a frame having run. An intercepting callback's own outcome is the inspector's
/// in whole — the EVM opened no checkpoint for it and wrote no state — so moving its
/// classification contradicts nothing, and is booked as an ordinary intervention.
#[test]
fn test_rewriting_an_inspector_s_own_synthetic_outcome_is_supported() {
    let mut inspector = AnswerThenRewrite { target: CALLEE, answered: 0, rewrote: 0 };
    let reading = run(
        base_db(calls_and_records(CALLEE, 200_000, 0)).account_code(
            CALLEE,
            BytecodeBuilder::default().sstore(U256::from(1), U256::from(7)).stop().build(),
        ),
        call_tx(CONTRACT),
        &mut inspector,
    );
    assert_eq!(inspector.answered, 1, "the fixture must reach the answering callback once");
    assert_eq!(inspector.rewrote, 1, "and the rewriting one once");
    assert_eq!(reading.rejected_rewrites, 0, "an inspector's own answer is not refused");
    assert!(reading.succeeded(), "the transaction must still execute, got {:?}", reading.result);
    assert_eq!(
        reading.storage(CONTRACT, FLAG_SLOT),
        U256::ZERO,
        "the caller must be handed the classification the callback last wrote",
    );
    assert_eq!(
        reading.storage(CALLEE, U256::from(1)),
        U256::ZERO,
        "no frame ran, so there is no write for the rewrite to disagree with",
    );
}

/// The same three rewrites under the frozen spec, which does not defend against them.
///
/// REX6 is closed: what it replays includes whatever an inspector on that path produced, so the
/// refusal is REX7-only and this pins that it is.
#[test]
fn test_the_frozen_spec_refuses_nothing() {
    let mut inspector = RewriteInitResult::new(EMPTY_TARGET, InstructionResult::Revert);
    let reading = run_on(
        MegaSpecId::REX6,
        base_db(calls_and_records(EMPTY_TARGET, 200_000, SENT)),
        call_tx(CONTRACT),
        &mut inspector,
    );

    assert_eq!(reading.rejected_rewrites, 0, "a frozen spec refuses nothing");
    assert!(reading.succeeded(), "the frozen run must still succeed, got {:?}", reading.result);
}
