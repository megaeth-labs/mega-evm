//! Where a volatile-guarded opcode's static gas is charged, seen from a frame that cannot pay it.
//!
//! A guarded opcode's static gas entry is zeroed in its spec's gas table so the interpreter's
//! pre-charge cannot pre-empt the `disableVolatileDataAccess` guard, and the handler chain charges
//! it instead. *Where* in the chain it charges is not free choice: `MegaETH`'s gas schedule is the
//! one revm charged from inside each opcode's body, and for the account-reading opcodes that body
//! pops its operands and reads the host **before** pricing them. A frame holding less than the
//! static gas therefore must not be turned away at the door — it has to reach the pop and the host
//! read first.
//!
//! Two things that are consensus-visible ride on that:
//!
//! - **The halt.** Whatever the body raises first wins: a stack underflow, an operand that does not
//!   fit a `usize`, a state change attempted inside a static frame. Charging ahead of the body
//!   converts all of them into out-of-gas.
//! - **The volatile-access mark.** The host marks beneficiary / oracle access as the account or
//!   slot is read, into the transaction-global bitmap that gas detention is derived from — a bitmap
//!   that does *not* roll back with the frame. An opcode turned away before its read leaves the
//!   rest of the transaction with an undetained compute budget.
//!
//! Each test below funds a frame to reach its guarded opcode holding exactly one gas less than the
//! opcode's static cost, and pins one of those two properties. The funding is asserted, not
//! assumed, so a scenario whose prologue cost shifts fails loudly instead of quietly testing a
//! well-funded frame.

use crate::common;
use mega_evm::MegaTransaction;
use std::{cell::RefCell, convert::Infallible, rc::Rc};

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaAccessControl, MegaContext, MegaEvm, MegaSpecId, TestExternalEnvs,
    ACCESS_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv, ContextTr, TxEnv},
    handler::EvmTr,
    inspector::Inspector,
    interpreter::{
        interpreter_types::{InputsTr, Jumps},
        CallInputs, CallOutcome, InstructionResult, Interpreter, InterpreterTypes,
    },
};

// ============================================================================
// TEST ADDRESSES AND CONSTANTS
// ============================================================================

/// Externally-owned transaction sender. Never the beneficiary.
const CALLER: Address = address!("0000000000000000000000000000000000700000");
/// Frame that sets up and calls the underfunded frame. Never the beneficiary, so the transaction's
/// own recipient check leaves the beneficiary unmarked.
const PARENT: Address = address!("0000000000000000000000000000000000700001");
/// The underfunded frame.
const CHILD: Address = address!("0000000000000000000000000000000000700002");
/// A funded, code-carrying account that is neither empty nor the beneficiary — an account-read and
/// `SELFDESTRUCT` target that triggers no new-account charge.
const TARGET: Address = address!("0000000000000000000000000000000000700003");
/// The block beneficiary. Deliberately not `Address::ZERO`, so a mark against the wrong address
/// cannot match it by accident.
const BENEFICIARY: Address = address!("0000000000000000000000000000000000700099");

/// Static gas of the account-touching opcodes — the EIP-2929 warm access cost.
const WARM_ACCESS_STATIC_GAS: u64 = 100;
/// Static gas of `SELFDESTRUCT`.
const SELFDESTRUCT_STATIC_GAS: u64 = 5_000;
/// Cost of any `PUSHn` the [`BytecodeBuilder`] emits — it never emits `PUSH0`.
const PUSH: u64 = 3;

/// Finite block-environment-access compute-gas cap, so detention is observable through
/// `detained_compute_gas_limit()` instead of staying saturated at `u64::MAX`.
const DETENTION_CAP: u64 = 20_000_000;

/// Every spec whose instruction table wraps the account reads in a volatile guard.
const ACCOUNT_READ_SPECS: [MegaSpecId; 8] = [
    MegaSpecId::MINI_REX,
    MegaSpecId::REX,
    MegaSpecId::REX1,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
];

/// Every spec whose `SLOAD` carries the oracle volatile guard.
const ORACLE_SLOAD_SPECS: [MegaSpecId; 4] =
    [MegaSpecId::REX3, MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// Every spec whose `SELFDESTRUCT` carries the beneficiary volatile guard, and whose host marks
/// beneficiary access from `SELFDESTRUCT`'s target.
const GUARDED_SELFDESTRUCT_SPECS: [MegaSpecId; 3] =
    [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

// ============================================================================
// INSPECTOR
// ============================================================================

/// Watches chosen `(frame, opcode)` pairs and records the gas the frame held when it reached the
/// opcode, plus the result each watched frame ended with.
///
/// `step` runs before anything is charged for the opcode, so the recorded gas is the frame's budget
/// as of reaching it. When a frame reaches the same opcode more than once the last occurrence wins,
/// which is what the multi-`CALL` scenarios want.
#[derive(Clone, Default)]
struct FrameProbe {
    watch: Rc<Vec<(Address, u8)>>,
    state: Rc<RefCell<ProbeState>>,
}

#[derive(Default)]
struct ProbeState {
    gas_at_opcode: Vec<((Address, u8), u64)>,
    frame_result: Vec<(Address, InstructionResult)>,
}

impl FrameProbe {
    fn new(watch: impl IntoIterator<Item = (Address, u8)>) -> Self {
        Self { watch: Rc::new(watch.into_iter().collect()), state: Rc::default() }
    }

    /// Gas the frame held on its last visit to the opcode.
    fn gas_at(&self, frame: Address, opcode: u8) -> Option<u64> {
        let state = self.state.borrow();
        state.gas_at_opcode.iter().find(|(key, _)| *key == (frame, opcode)).map(|(_, gas)| *gas)
    }

    /// The result the frame ended with.
    fn result_of(&self, frame: Address) -> Option<InstructionResult> {
        let state = self.state.borrow();
        state.frame_result.iter().find(|(key, _)| *key == frame).map(|(_, result)| *result)
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for FrameProbe {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        let key = (interp.input.target_address(), interp.bytecode.opcode());
        if !self.watch.contains(&key) {
            return;
        }
        let remaining = interp.gas.remaining();
        let mut state = self.state.borrow_mut();
        match state.gas_at_opcode.iter_mut().find(|(entry, _)| *entry == key) {
            Some((_, gas)) => *gas = remaining,
            None => state.gas_at_opcode.push((key, remaining)),
        }
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let frame = inputs.target_address;
        if !self.watch.iter().any(|(watched, _)| *watched == frame) {
            return;
        }
        let result = outcome.result.result;
        let mut state = self.state.borrow_mut();
        match state.frame_result.iter_mut().find(|(entry, _)| *entry == frame) {
            Some((_, slot)) => *slot = result,
            None => state.frame_result.push((frame, result)),
        }
    }
}

// ============================================================================
// HELPERS
// ============================================================================

/// What the transaction left behind in the volatile-access tracker and the limit tracker.
struct VolatileState {
    beneficiary_marked: bool,
    oracle_marked: bool,
    detained_compute_gas_limit: u64,
}

/// Runs `tx` under `spec` with `BENEFICIARY` as the block beneficiary and a finite detention cap.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
    probe: FrameProbe,
) -> VolatileState {
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let mut context = MegaContext::new(db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(DETENTION_CAP),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context).with_inspector(probe);
    let mut tx = MegaTransaction(OpTransaction::new(tx));
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface an EVMError");

    let tracker = evm.ctx_ref().volatile_data_tracker.borrow();
    let state = VolatileState {
        beneficiary_marked: tracker.has_accessed_beneficiary_balance(),
        oracle_marked: tracker.has_accessed_oracle(),
        detained_compute_gas_limit: evm
            .ctx_ref()
            .additional_limit
            .borrow()
            .detained_compute_gas_limit(),
    };
    drop(tracker);
    state
}

/// `CALL(target, gas)` with no value and no calldata.
fn append_call(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// `STATICCALL(target, gas)` with no calldata.
fn append_static_call(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(target)
        .push_number(gas)
        .append(STATICCALL)
}

/// A database with the fixed cast: a funded sender, a code-carrying non-empty `TARGET`, and a
/// beneficiary with a balance (so `SELFDESTRUCT` to it is not a new-account creation).
fn base_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(TARGET, BytecodeBuilder::default().stop().build())
        .account_balance(TARGET, U256::from(1u64))
        .account_balance(BENEFICIARY, U256::from(1u64))
}

/// Runs `child_code` at `child`, called by `PARENT` with exactly `child_gas` forwarded, and returns
/// the probe observations together with the transaction's volatile-access state.
///
/// `call_child` builds the call so a test can pick `CALL` or `STATICCALL`; `parent_tail` appends
/// whatever the parent should run after the child returns.
fn run_child_frame(
    spec: MegaSpecId,
    child: Address,
    child_code: Bytes,
    child_gas: u64,
    call_child: fn(BytecodeBuilder, Address, u64) -> BytecodeBuilder,
    parent_tail: impl Fn(BytecodeBuilder) -> BytecodeBuilder,
    probe: FrameProbe,
) -> VolatileState {
    let parent_code =
        parent_tail(call_child(BytecodeBuilder::default(), child, child_gas).append(POP))
            .stop()
            .build();

    let mut db = base_db().account_code(PARENT, parent_code);
    if child != PARENT {
        db = db.account_code(child, child_code);
    }

    transact(
        spec,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(30_000_000).build_fill(),
        probe,
    )
}

/// Leaves the parent's tail empty.
fn no_tail(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
}

// ============================================================================
// HALT KIND: the body's own failure must not be pre-empted by out-of-gas
// ============================================================================

/// A static frame that cannot afford `SELFDESTRUCT`'s 5,000 static gas must still halt on the
/// static-context violation revm's body raises first, not out of gas.
///
/// This is the sharpest case of the family: revm's body checks the static context before it so much
/// as pops the target, so no amount of underfunding can turn the halt into an out-of-gas one.
#[test]
fn test_underfunded_static_selfdestruct_halts_on_the_static_context_violation() {
    for spec in GUARDED_SELFDESTRUCT_SPECS {
        let child_code =
            BytecodeBuilder::default().push_address(TARGET).append(SELFDESTRUCT).build();
        let probe = FrameProbe::new([(CHILD, SELFDESTRUCT)]);
        run_child_frame(
            spec,
            CHILD,
            child_code,
            PUSH + SELFDESTRUCT_STATIC_GAS - 1,
            append_static_call,
            no_tail,
            probe.clone(),
        );

        assert_eq!(
            probe.gas_at(CHILD, SELFDESTRUCT),
            Some(SELFDESTRUCT_STATIC_GAS - 1),
            "{spec}: the frame should reach SELFDESTRUCT one gas short of its static cost",
        );
        assert_eq!(
            probe.result_of(CHILD),
            Some(InstructionResult::StateChangeDuringStaticCall),
            "{spec}: a static frame's SELFDESTRUCT halts on the static-context check, which revm \
             runs before the opcode is priced",
        );
    }
}

/// A frame that cannot afford `BALANCE`'s static gas and reaches it with an empty stack must still
/// halt on the stack underflow revm's body raises when it pops the address.
#[test]
fn test_underfunded_balance_halts_on_stack_underflow() {
    for spec in ACCOUNT_READ_SPECS {
        let child_code = BytecodeBuilder::default().append(BALANCE).build();
        let probe = FrameProbe::new([(CHILD, BALANCE)]);
        run_child_frame(
            spec,
            CHILD,
            child_code,
            WARM_ACCESS_STATIC_GAS - 1,
            append_call,
            no_tail,
            probe.clone(),
        );

        assert_eq!(
            probe.gas_at(CHILD, BALANCE),
            Some(WARM_ACCESS_STATIC_GAS - 1),
            "{spec}: the frame should reach BALANCE one gas short of its static cost",
        );
        assert_eq!(
            probe.result_of(CHILD),
            Some(InstructionResult::StackUnderflow),
            "{spec}: BALANCE pops its address before it is priced, so an empty stack underflows",
        );
    }
}

/// A frame that cannot afford `EXTCODECOPY`'s static gas must still halt on the unrepresentable
/// length operand, which revm's body rejects before charging anything.
#[test]
fn test_underfunded_extcodecopy_halts_on_the_unrepresentable_length() {
    // Four `PUSH`es of prologue: length, code offset, destination offset, address.
    const PROLOGUE: u64 = 4 * PUSH;

    for spec in ACCOUNT_READ_SPECS {
        let child_code = BytecodeBuilder::default()
            .push_u256(U256::MAX) // length — cannot be a `usize`
            .push_number(0_u64) // code offset
            .push_number(0_u64) // destination offset
            .push_address(TARGET)
            .append(EXTCODECOPY)
            .build();
        let probe = FrameProbe::new([(CHILD, EXTCODECOPY)]);
        run_child_frame(
            spec,
            CHILD,
            child_code,
            PROLOGUE + WARM_ACCESS_STATIC_GAS - 1,
            append_call,
            no_tail,
            probe.clone(),
        );

        assert_eq!(
            probe.gas_at(CHILD, EXTCODECOPY),
            Some(WARM_ACCESS_STATIC_GAS - 1),
            "{spec}: the frame should reach EXTCODECOPY one gas short of its static cost",
        );
        assert_eq!(
            probe.result_of(CHILD),
            Some(InstructionResult::InvalidOperandOOG),
            "{spec}: EXTCODECOPY rejects a length that is not a `usize` before it is priced",
        );
    }
}

// ============================================================================
// VOLATILE-ACCESS MARK: the read happens before the price
// ============================================================================

/// A frame too poor to afford `BALANCE`'s static gas still reads the beneficiary's account, so the
/// transaction-global volatile bitmap records the access.
#[test]
fn test_underfunded_balance_still_marks_beneficiary_access() {
    for spec in ACCOUNT_READ_SPECS {
        let child_code =
            BytecodeBuilder::default().push_address(BENEFICIARY).append(BALANCE).build();
        let probe = FrameProbe::new([(CHILD, BALANCE)]);
        let state = run_child_frame(
            spec,
            CHILD,
            child_code,
            PUSH + WARM_ACCESS_STATIC_GAS - 1,
            append_call,
            no_tail,
            probe.clone(),
        );

        assert_eq!(
            probe.gas_at(CHILD, BALANCE),
            Some(WARM_ACCESS_STATIC_GAS - 1),
            "{spec}: the frame should reach BALANCE one gas short of its static cost",
        );
        assert_eq!(
            probe.result_of(CHILD),
            Some(InstructionResult::OutOfGas),
            "{spec}: the frame still runs out of gas — the read comes first, the price still lands",
        );
        assert!(
            state.beneficiary_marked,
            "{spec}: BALANCE reads the beneficiary before it is priced, so the access is marked \
             even though the frame could not pay for it",
        );
    }
}

/// The control for the test above: the same underfunded `BALANCE` against a non-beneficiary target
/// marks nothing, so the assertion there is about the read and not about some unrelated
/// beneficiary touch elsewhere in the transaction.
#[test]
fn test_underfunded_balance_of_a_third_party_marks_nothing() {
    for spec in ACCOUNT_READ_SPECS {
        let child_code = BytecodeBuilder::default().push_address(TARGET).append(BALANCE).build();
        let state = run_child_frame(
            spec,
            CHILD,
            child_code,
            PUSH + WARM_ACCESS_STATIC_GAS - 1,
            append_call,
            no_tail,
            FrameProbe::default(),
        );

        assert!(
            !state.beneficiary_marked,
            "{spec}: reading a non-beneficiary account must leave the beneficiary unmarked",
        );
        assert_eq!(
            state.detained_compute_gas_limit,
            u64::MAX,
            "{spec}: with nothing volatile accessed, the compute budget must stay undetained",
        );
    }
}

/// A frame too poor to afford `SELFDESTRUCT`'s static gas still reaches `host.selfdestruct`, which
/// marks the beneficiary when it is the target.
#[test]
fn test_underfunded_selfdestruct_still_marks_beneficiary_access() {
    for spec in GUARDED_SELFDESTRUCT_SPECS {
        let child_code =
            BytecodeBuilder::default().push_address(BENEFICIARY).append(SELFDESTRUCT).build();
        let probe = FrameProbe::new([(CHILD, SELFDESTRUCT)]);
        let state = run_child_frame(
            spec,
            CHILD,
            child_code,
            PUSH + SELFDESTRUCT_STATIC_GAS - 1,
            append_call,
            no_tail,
            probe.clone(),
        );

        assert_eq!(
            probe.gas_at(CHILD, SELFDESTRUCT),
            Some(SELFDESTRUCT_STATIC_GAS - 1),
            "{spec}: the frame should reach SELFDESTRUCT one gas short of its static cost",
        );
        assert!(
            state.beneficiary_marked,
            "{spec}: SELFDESTRUCT reaches its beneficiary target before it is priced, so the \
             access is marked even though the frame could not pay for it",
        );
    }
}

/// A frame too poor to afford `SLOAD`'s static gas still issues the oracle read, so oracle access
/// is marked.
///
/// revm 40 skips the journal load outright when the frame cannot cover the cold-storage surcharge
/// and reports `ColdLoadSkipped`, where the pre-`GasTable` revm loaded the slot and then failed the
/// combined charge. Both end the frame out of gas, and `MegaETH`'s oracle read marks before either
/// branch is taken, so the frame result and the bitmap come out the same either way.
#[test]
fn test_underfunded_oracle_sload_still_marks_oracle_access() {
    for spec in ORACLE_SLOAD_SPECS {
        // The frame has to *be* the oracle contract: `SLOAD` reads the executing contract's
        // storage. Empty calldata matches no system-contract selector, so the call runs this
        // bytecode rather than being intercepted.
        let oracle_code = BytecodeBuilder::default().push_number(0_u64).append(SLOAD).build();
        let probe = FrameProbe::new([(ORACLE_CONTRACT_ADDRESS, SLOAD)]);
        let state = run_child_frame(
            spec,
            ORACLE_CONTRACT_ADDRESS,
            oracle_code,
            PUSH + WARM_ACCESS_STATIC_GAS - 1,
            append_call,
            no_tail,
            probe.clone(),
        );

        assert_eq!(
            probe.gas_at(ORACLE_CONTRACT_ADDRESS, SLOAD),
            Some(WARM_ACCESS_STATIC_GAS - 1),
            "{spec}: the frame should reach SLOAD one gas short of its static cost",
        );
        assert_eq!(
            probe.result_of(ORACLE_CONTRACT_ADDRESS),
            Some(InstructionResult::OutOfGas),
            "{spec}: the frame still runs out of gas",
        );
        assert!(
            state.oracle_marked,
            "{spec}: the oracle read is issued before SLOAD is priced, so oracle access is marked \
             even though the frame could not pay for it",
        );
    }
}

// ============================================================================
// DETENTION: the ancestor inherits the mark the underfunded frame left behind
// ============================================================================

/// Bytecode for a frame that reads the beneficiary's balance with one gas less than `BALANCE`
/// costs: it marks the access and then runs out of gas.
fn underfunded_beneficiary_balance_code() -> Bytes {
    BytecodeBuilder::default().push_address(BENEFICIARY).append(BALANCE).build()
}

/// Gas to forward so [`underfunded_beneficiary_balance_code`] reaches `BALANCE` one gas short.
const UNDERFUNDED_BALANCE_GAS: u64 = PUSH + WARM_ACCESS_STATIC_GAS - 1;

/// After an inner frame marks beneficiary access and dies out of gas, the ancestor's compute budget
/// must be detained.
///
/// The ancestor's own tail is a `BALANCE` of a *third party*, which marks nothing itself — it only
/// propagates whatever the tracker already holds. So the detention observed here can only have come
/// from the underfunded inner frame's read.
#[test]
fn test_inner_underfunded_access_detains_the_ancestor() {
    for spec in GUARDED_SELFDESTRUCT_SPECS {
        let state = run_child_frame(
            spec,
            CHILD,
            underfunded_beneficiary_balance_code(),
            UNDERFUNDED_BALANCE_GAS,
            append_call,
            |builder| builder.push_address(TARGET).append(BALANCE).append(POP),
            FrameProbe::default(),
        );

        assert!(
            state.beneficiary_marked,
            "{spec}: the inner frame's read must be marked for this scenario to mean anything",
        );
        assert!(
            state.detained_compute_gas_limit < u64::MAX,
            "{spec}: the ancestor must inherit the detention the inner frame's access earned",
        );
    }
}

/// A `CALL` that cannot afford its own static gas must still propagate a pending detention into the
/// transaction's compute budget.
///
/// The CALL family is the one that has to charge ahead of its body — the body derives the child
/// frame's gas limit from what is left afterwards. Its detention tail therefore has to run on the
/// out-of-gas path explicitly, which is what this pins: an inner frame leaves a mark behind, and
/// the ancestor's next `CALL` is too poor to execute yet still has to hand that mark to the limit
/// tracker.
#[test]
fn test_underfunded_call_still_applies_a_pending_detention() {
    for spec in GUARDED_SELFDESTRUCT_SPECS {
        // Calibrate the transaction's gas limit so the parent reaches its *second* CALL holding one
        // gas less than the opcode's static cost. The prologue cost does not depend on the funding,
        // so the first correction lands on the answer; the extra rounds only absorb a prologue
        // whose own cost moved with it.
        let target = WARM_ACCESS_STATIC_GAS - 1;
        let mut gas_limit = 1_000_000;
        let mut probe = FrameProbe::default();
        for _ in 0..8 {
            probe = FrameProbe::new([(CHILD, BALANCE), (PARENT, CALL)]);
            run_pending_detention_scenario(spec, gas_limit, probe.clone());
            let remaining =
                probe.gas_at(PARENT, CALL).expect("the parent should reach its second CALL");
            if remaining == target {
                break;
            }
            gas_limit = (gas_limit + target).checked_sub(remaining).expect("funding underflow");
        }

        let state = run_pending_detention_scenario(spec, gas_limit, probe.clone());
        assert_eq!(
            probe.gas_at(PARENT, CALL),
            Some(target),
            "{spec}: the parent should reach its second CALL one gas short of its static cost",
        );
        assert_eq!(
            probe.result_of(CHILD),
            Some(InstructionResult::OutOfGas),
            "{spec}: the inner frame should still die out of gas after marking the access",
        );
        assert!(
            state.beneficiary_marked,
            "{spec}: the inner frame's read must be marked for this scenario to mean anything",
        );
        assert!(
            state.detained_compute_gas_limit < u64::MAX,
            "{spec}: a CALL that cannot afford its static gas must still propagate the pending \
             detention into the compute budget",
        );
    }
}

/// The scenario behind [`test_underfunded_call_still_applies_a_pending_detention`]: `PARENT` runs
/// as the transaction's own frame, calls the underfunded beneficiary reader, and then reaches a
/// second `CALL` it cannot afford.
fn run_pending_detention_scenario(
    spec: MegaSpecId,
    gas_limit: u64,
    probe: FrameProbe,
) -> VolatileState {
    let parent_code = append_call(
        append_call(BytecodeBuilder::default(), CHILD, UNDERFUNDED_BALANCE_GAS).append(POP),
        TARGET,
        0,
    )
    .append(POP)
    .stop()
    .build();

    let mut db = base_db()
        .account_code(PARENT, parent_code)
        .account_code(CHILD, underfunded_beneficiary_balance_code());

    transact(
        spec,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(gas_limit).build_fill(),
        probe,
    )
}

// ============================================================================
// COLD ACCESS: the charge moved past the surcharge, the total did not
// ============================================================================

/// EIP-2929 cold account access cost.
const COLD_ACCOUNT_ACCESS: u64 = 2_600;
/// EIP-2929 cold storage slot access cost.
const COLD_SLOT_ACCESS: u64 = 2_100;
/// Cost of `POP`.
const POP_GAS: u64 = 2;

/// A cold account that no test funds, so its first read pays the cold surcharge.
const COLD_A: Address = address!("00000000000000000000000000000000007000a1");
/// A second, distinct cold account.
const COLD_B: Address = address!("00000000000000000000000000000000007000a2");

/// Asserts that appending one more `fragment` to `prelude` costs exactly `expected`, in both EVM
/// gas and recorded compute gas.
///
/// Differencing cancels the transaction's fixed costs and leaves the fragment's own price. Under-
/// or double-charging the static gas shows up as a mismatched EVM-gas delta; forgetting to drop the
/// static-gas add-back once the charge moved into the measurement window shows up only in the
/// compute-gas delta, which is why both are pinned.
///
/// These run through [`common::transact_default`], whose transaction is its own sender calling its
/// own contract — not the [`PARENT`] / [`CHILD`] pair the underfunded scenarios above build.
fn assert_one_more_occurrence_costs(
    expected: u64,
    label: &str,
    first: impl Fn(BytecodeBuilder) -> BytecodeBuilder,
    second: impl Fn(BytecodeBuilder) -> BytecodeBuilder,
) {
    for spec in ACCOUNT_READ_SPECS {
        let measure = |code: BytecodeBuilder| {
            let db = MemoryDatabase::default()
                .account_balance(common::CALLER, U256::from(1_000_000_000_000u64))
                .account_code(common::CONTRACT, code.stop().build());
            let outcome = common::transact_default(spec, db);
            assert!(outcome.is_success(), "{spec} {label}: tx should succeed");
            (outcome.gas_used, outcome.compute_gas)
        };

        let (gas_once, compute_once) = measure(first(BytecodeBuilder::default()));
        let (gas_twice, compute_twice) = measure(second(first(BytecodeBuilder::default())));

        assert_eq!(
            gas_twice - gas_once,
            expected,
            "{spec} {label}: one more occurrence must cost exactly {expected} EVM gas",
        );
        assert_eq!(
            compute_twice - compute_once,
            expected,
            "{spec} {label}: one more occurrence must record exactly {expected} compute gas",
        );
    }
}

/// One more *cold* `BALANCE` costs its full cold price — the static gas plus the cold surcharge —
/// once in EVM gas and once in compute gas.
///
/// The two are now charged from opposite sides of the inner instruction: revm's body charges the
/// surcharge, this crate's handler charges the static gas afterwards. Their sum, and the single
/// compute-gas window that covers both, must be exactly what the schedule says.
#[test]
fn test_extra_cold_balance_costs_its_full_cold_price() {
    assert_one_more_occurrence_costs(
        PUSH + COLD_ACCOUNT_ACCESS + POP_GAS,
        "cold BALANCE",
        |b| b.push_address(COLD_A).append(BALANCE).append(POP),
        |b| b.push_address(COLD_B).append(BALANCE).append(POP),
    );
}

/// One more *cold* `SLOAD` costs its full cold price, on the specs that pre-charge its static gas
/// and on the specs whose oracle guard makes the handler charge it after the load.
#[test]
fn test_extra_cold_sload_costs_its_full_cold_price() {
    assert_one_more_occurrence_costs(
        PUSH + COLD_SLOT_ACCESS + POP_GAS,
        "cold SLOAD",
        |b| b.push_number(0_u64).append(SLOAD).append(POP),
        |b| b.push_number(1_u64).append(SLOAD).append(POP),
    );
}

// ============================================================================
// THE GUARD ITSELF IS UNAFFECTED
// ============================================================================

/// Moving the charge must not move the guard: an underfunded frame whose guarded opcode targets the
/// beneficiary while `disableVolatileDataAccess()` is active still reverts before the opcode runs,
/// keeping its gas and leaving the tracker untouched.
#[test]
fn test_underfunded_guarded_balance_still_reverts_without_marking() {
    let disable_selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

    for spec in GUARDED_SELFDESTRUCT_SPECS {
        let parent_code = append_call(
            BytecodeBuilder::default()
                .mstore(0x0, disable_selector)
                .push_number(0_u64) // retSize
                .push_number(0_u64) // retOffset
                .push_number(4_u64) // argsSize
                .push_number(0_u64) // argsOffset
                .push_number(0_u64) // value
                .push_address(ACCESS_CONTROL_ADDRESS)
                .push_number(100_000_u64) // gas
                .append(CALL)
                .append(POP),
            CHILD,
            UNDERFUNDED_BALANCE_GAS,
        )
        .append(POP)
        .stop()
        .build();

        let mut db = base_db()
            .account_code(PARENT, parent_code)
            .account_code(CHILD, underfunded_beneficiary_balance_code());

        let probe = FrameProbe::new([(CHILD, BALANCE)]);
        let state = transact(
            spec,
            &mut db,
            TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(30_000_000).build_fill(),
            probe.clone(),
        );

        assert_eq!(
            probe.result_of(CHILD),
            Some(InstructionResult::Revert),
            "{spec}: the guard still rejects the opcode with a revert",
        );
        assert!(
            !state.beneficiary_marked,
            "{spec}: a rejected opcode never runs, so it must not mark the access",
        );
    }
}
