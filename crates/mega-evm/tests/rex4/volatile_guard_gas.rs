//! Gas accounting of the `disableVolatileDataAccess` opcode guards.
//!
//! Each volatile guard rejects its opcode *before* the opcode executes, so the guarded opcode must
//! cost the reverting frame nothing at all: the gas the frame hands back to its caller must equal
//! the gas it held when the guarded opcode was reached. The frame's remaining gas is observable
//! (the caller can read it with `GAS`, and it decides whether a subsequent operation runs out of
//! gas), so a discrepancy here is consensus-visible on the frozen specs.
//!
//! Each test below pins one guard family against one opcode; the covered static-gas amounts (2 for
//! `TIMESTAMP`, 20 for `BLOCKHASH`, 100 for the account-touching opcodes, 5 for `SELFBALANCE`,
//! 5,000 for `SELFDESTRUCT`) are distinct, so a guard that returned a wrong or constant amount
//! would fail here rather than pass by coincidence.
//!
//! Two funding levels are covered per family:
//!
//! - a frame holding far more gas than the opcode's static cost, which pins the "the guard hands
//!   the whole frame back" property; and
//! - a frame holding *less* gas than the opcode's static cost. A guarded opcode may not be
//!   pre-charged at all, so being unable to afford the static cost must not pre-empt the guard: the
//!   frame still reverts with `VolatileDataAccessDisabled` and keeps every unit of gas it held. The
//!   same underfunded frame with volatile access *enabled* must still run out of gas exactly as an
//!   unguarded opcode does, which the `..._runs_out_of_gas` tests pin.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    alloy_op_evm::OpTx,
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaAccessControl, MegaContext, MegaEvm, MegaSpecId, TestExternalEnvs, VolatileDataAccessType,
    ACCESS_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv, ContextTr, TxEnv},
    interpreter::{
        interpreter_types::{InputsTr, Jumps},
        CallInputs, CallOutcome, InstructionResult, Interpreter, InterpreterTypes,
    },
    Inspector,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000210000");
const PARENT: Address = address!("0000000000000000000000000000000000210001");
const CHILD: Address = address!("0000000000000000000000000000000000210002");
/// The block beneficiary in these tests. Deliberately not `Address::ZERO` so that a guard reading
/// the wrong address cannot match it by accident.
const BENEFICIARY: Address = address!("0000000000000000000000000000000000210099");

/// The 4-byte selector for `disableVolatileDataAccess()`.
const DISABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

// ============================================================================
// STATIC GAS OF THE GUARDED OPCODES
// ============================================================================

/// Static gas of the block-environment reads (`BASE`).
const TIMESTAMP_STATIC_GAS: u64 = 2;
/// Static gas of `BLOCKHASH`.
const BLOCKHASH_STATIC_GAS: u64 = 20;
/// Static gas of the account-touching opcodes — the EIP-2929 warm access cost.
const WARM_ACCESS_STATIC_GAS: u64 = 100;
/// Static gas of `SELFBALANCE` (`LOW`).
const SELFBALANCE_STATIC_GAS: u64 = 5;
/// Static gas of `SELFDESTRUCT`.
const SELFDESTRUCT_STATIC_GAS: u64 = 5_000;

// ============================================================================
// INSPECTOR
// ============================================================================

/// What the guarded frame handed back to its caller.
struct GuardedFrameOutcome {
    /// Gas remaining in the frame's return action.
    remaining: u64,
    /// The frame's instruction result.
    result: InstructionResult,
    /// The frame's revert data.
    output: Bytes,
}

/// Observes one frame's gas immediately before a chosen opcode, and the outcome that frame
/// returns.
///
/// `step` runs before anything is charged for the opcode, so `remaining_before_opcode` is the
/// frame's gas as of reaching the guarded opcode — the amount a guard that rejects the opcode
/// outright must hand back untouched.
struct GuardedFrameGasInspector {
    /// Address of the frame that runs the guarded opcode.
    frame: Address,
    /// The opcode expected to trip the guard.
    opcode: u8,
    /// `gas.remaining()` observed when the guarded opcode was reached.
    remaining_before_opcode: Option<u64>,
    /// The guarded frame's return outcome.
    outcome: Option<GuardedFrameOutcome>,
}

impl GuardedFrameGasInspector {
    fn new(frame: Address, opcode: u8) -> Self {
        Self { frame, opcode, remaining_before_opcode: None, outcome: None }
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for GuardedFrameGasInspector {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if interp.input.target_address() == self.frame && interp.bytecode.opcode() == self.opcode {
            self.remaining_before_opcode = Some(interp.gas.remaining());
        }
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == self.frame {
            self.outcome = Some(GuardedFrameOutcome {
                remaining: outcome.result.gas.remaining(),
                result: outcome.result.result,
                output: outcome.result.output.clone(),
            });
        }
    }
}

// ============================================================================
// HELPERS
// ============================================================================

/// Builds bytecode that calls `disableVolatileDataAccess()` on the access control contract.
fn call_disable_volatile_data_access(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP)
}

/// Builds bytecode that CALLs `target` with no value and no calldata.
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

/// Runs `tx` on `spec` with `BENEFICIARY` as the block beneficiary, under the given inspector.
fn transact_inspected(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
    inspector: &mut GuardedFrameGasInspector,
) -> bool {
    let external_envs = TestExternalEnvs::<Infallible>::new()
        .with_oracle_storage(U256::from(0), U256::from(0x1234));
    let mut context = MegaContext::new(db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context).with_inspector(inspector);
    let mut tx = OpTx(OpTransaction::new(tx));
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap().result.is_success()
}

/// Asserts that the guarded frame reverted with `VolatileDataAccessDisabled(access_type)` without
/// spending any of its gas.
fn assert_guard_charged_nothing(
    inspector: &GuardedFrameGasInspector,
    access_type: VolatileDataAccessType,
    label: &str,
) {
    let before = inspector
        .remaining_before_opcode
        .unwrap_or_else(|| panic!("{label}: guarded opcode was never reached"));
    let outcome =
        inspector.outcome.as_ref().unwrap_or_else(|| panic!("{label}: guarded frame never ended"));

    assert_eq!(
        outcome.result,
        InstructionResult::Revert,
        "{label}: guarded frame should revert, got {:?}",
        outcome.result
    );
    assert_eq!(
        outcome.remaining,
        before,
        "{label}: the guard rejects the opcode before it runs, so the frame must return all \
         {before} gas it held; it returned {}, i.e. {} gas short",
        outcome.remaining,
        before - outcome.remaining
    );

    let expected = IMegaAccessControl::VolatileDataAccessDisabled { accessType: access_type };
    assert_eq!(
        outcome.output.as_ref(),
        expected.abi_encode().as_slice(),
        "{label}: unexpected revert data"
    );
}

/// Runs `child_code` as a frame at `child` holding `child_gas`, called by a parent that has first
/// disabled volatile data access when `disable` is set.
fn run_child_frame(
    child: Address,
    opcode: u8,
    child_code: Bytes,
    child_gas: u64,
    disable: bool,
) -> (bool, GuardedFrameGasInspector) {
    let builder = BytecodeBuilder::default();
    let parent_code = if disable { call_disable_volatile_data_access(builder) } else { builder };
    let parent_code = append_call(parent_code, child, child_gas).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(child, child_code);

    let mut inspector = GuardedFrameGasInspector::new(child, opcode);
    let success = transact_inspected(
        MegaSpecId::REX4,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill(),
        &mut inspector,
    );
    (success, inspector)
}

/// Finds the funding that leaves the guarded frame holding exactly `target` gas when it reaches the
/// guarded opcode.
///
/// `run` funds the frame with the given amount and reports the gas it held at the opcode. Deriving
/// the funding instead of hard-coding it keeps the underfunded tests honest: each caller asserts
/// the observed amount afterwards, so a scenario whose prologue cost shifted fails loudly rather
/// than silently testing a well-funded frame.
fn calibrate_funding(target: u64, mut run: impl FnMut(u64) -> u64) -> u64 {
    // The prologue cost is (near enough) independent of the funding, so the first correction lands
    // on the answer; the remaining rounds only absorb a prologue whose own cost moved with it.
    let mut funding = 1_000_000;
    for _ in 0..8 {
        let remaining = run(funding);
        if remaining == target {
            return funding;
        }
        funding = (funding + target).checked_sub(remaining).expect("funding underflow");
    }
    panic!("could not fund the frame to reach the guarded opcode holding {target} gas");
}

/// Runs `child_code` as a frame at `CHILD` under disabled volatile access, and asserts its guarded
/// `opcode` reverted without charging the frame.
fn assert_child_guard_charged_nothing(
    opcode: u8,
    child_code: Bytes,
    access_type: VolatileDataAccessType,
    label: &str,
) {
    let (success, inspector) = run_child_frame(CHILD, opcode, child_code, 50_000_000, true);
    assert!(success, "{label}: only the guarded child frame should revert, not the whole tx");

    assert_guard_charged_nothing(&inspector, access_type, label);
}

/// Asserts that a frame that cannot even afford `opcode`'s static gas still gets the guard's
/// revert, with every unit of gas it held handed back.
///
/// `static_gas` is the opcode's entry in the interpreter's static gas table; the frame is funded to
/// reach the opcode holding exactly one gas less than that.
fn assert_underfunded_guard_charged_nothing(
    child: Address,
    opcode: u8,
    child_code: Bytes,
    access_type: VolatileDataAccessType,
    static_gas: u64,
    label: &str,
) {
    let remaining_at_opcode = static_gas - 1;
    let funding = calibrate_funding(remaining_at_opcode, |gas| {
        let (_, inspector) = run_child_frame(child, opcode, child_code.clone(), gas, true);
        inspector.remaining_before_opcode.expect("the guarded opcode was never reached")
    });

    let (success, inspector) = run_child_frame(child, opcode, child_code, funding, true);
    assert!(success, "{label}: only the guarded child frame should revert, not the whole tx");
    assert_eq!(
        inspector.remaining_before_opcode,
        Some(remaining_at_opcode),
        "{label}: the frame should reach the guarded opcode holding {remaining_at_opcode} gas, \
         one short of its {static_gas} static gas",
    );

    assert_guard_charged_nothing(&inspector, access_type, label);
}

/// Asserts that the same underfunded frame, with volatile access *enabled*, still runs out of gas
/// on `opcode` — spending everything it held and returning no data, exactly as an unguarded opcode
/// that cannot afford its static gas does.
fn assert_underfunded_opcode_runs_out_of_gas(
    child: Address,
    opcode: u8,
    child_code: Bytes,
    static_gas: u64,
    label: &str,
) {
    let remaining_at_opcode = static_gas - 1;
    let funding = calibrate_funding(remaining_at_opcode, |gas| {
        let (_, inspector) = run_child_frame(child, opcode, child_code.clone(), gas, false);
        inspector.remaining_before_opcode.expect("the opcode was never reached")
    });

    let (_, inspector) = run_child_frame(child, opcode, child_code, funding, false);
    assert_eq!(
        inspector.remaining_before_opcode,
        Some(remaining_at_opcode),
        "{label}: the frame should reach the opcode holding {remaining_at_opcode} gas, one short \
         of its {static_gas} static gas",
    );

    let outcome =
        inspector.outcome.as_ref().unwrap_or_else(|| panic!("{label}: frame never ended"));
    assert_eq!(
        outcome.result,
        InstructionResult::OutOfGas,
        "{label}: an opcode that cannot afford its static gas must halt out of gas",
    );
    assert_eq!(outcome.remaining, 0, "{label}: an out-of-gas halt spends the frame's whole budget");
    assert!(outcome.output.is_empty(), "{label}: an out-of-gas halt returns no data");
}

// ============================================================================
// UNCONDITIONALLY VOLATILE OPCODES (`wrap_op_detain_gas_unconditional!`)
// ============================================================================

/// `TIMESTAMP` (2 static gas) must not charge the frame it rejects.
#[test]
fn test_guarded_timestamp_charges_nothing() {
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();
    assert_child_guard_charged_nothing(
        TIMESTAMP,
        child_code,
        VolatileDataAccessType::Timestamp,
        "TIMESTAMP",
    );
}

/// `BLOCKHASH` (20 static gas) must not charge the frame it rejects.
///
/// Paired with the `TIMESTAMP` test above, this pins the returned amount to the *opcode's* static
/// gas rather than a single constant shared by the family.
#[test]
fn test_guarded_blockhash_charges_nothing() {
    let child_code = BytecodeBuilder::default()
        .push_number(0_u64) // block number operand — never consumed, the guard fires first
        .append(BLOCKHASH)
        .append(POP)
        .stop()
        .build();
    assert_child_guard_charged_nothing(
        BLOCKHASH,
        child_code,
        VolatileDataAccessType::BlockHash,
        "BLOCKHASH",
    );
}

/// A frame too poor to afford `TIMESTAMP`'s static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_timestamp_reverts_with_all_gas_kept() {
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();
    assert_underfunded_guard_charged_nothing(
        CHILD,
        TIMESTAMP,
        child_code,
        VolatileDataAccessType::Timestamp,
        TIMESTAMP_STATIC_GAS,
        "underfunded TIMESTAMP",
    );
}

/// The same underfunded frame with volatile access enabled still runs out of gas on `TIMESTAMP`.
#[test]
fn test_underfunded_timestamp_runs_out_of_gas() {
    let child_code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();
    assert_underfunded_opcode_runs_out_of_gas(
        CHILD,
        TIMESTAMP,
        child_code,
        TIMESTAMP_STATIC_GAS,
        "underfunded TIMESTAMP",
    );
}

/// A frame too poor to afford `BLOCKHASH`'s static gas still gets the guard's revert.
///
/// Paired with the `TIMESTAMP` case above, this pins the kept amount to the *opcode's* static gas
/// rather than a single constant shared by the family.
#[test]
fn test_underfunded_guarded_blockhash_reverts_with_all_gas_kept() {
    let child_code = BytecodeBuilder::default()
        .push_number(0_u64) // block number operand — never consumed, the guard fires first
        .append(BLOCKHASH)
        .append(POP)
        .stop()
        .build();
    assert_underfunded_guard_charged_nothing(
        CHILD,
        BLOCKHASH,
        child_code,
        VolatileDataAccessType::BlockHash,
        BLOCKHASH_STATIC_GAS,
        "underfunded BLOCKHASH",
    );
}

// ============================================================================
// BENEFICIARY-CONDITIONAL OPCODES (`wrap_op_detain_gas_conditional!`)
// ============================================================================

/// `BALANCE(beneficiary)` (100 static gas) must not charge the frame it rejects.
#[test]
fn test_guarded_balance_charges_nothing() {
    let child_code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    assert_child_guard_charged_nothing(
        BALANCE,
        child_code,
        VolatileDataAccessType::Beneficiary,
        "BALANCE",
    );
}

/// `EXTCODEHASH(beneficiary)` (100 static gas) must not charge the frame it rejects.
#[test]
fn test_guarded_extcodehash_charges_nothing() {
    let child_code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(EXTCODEHASH)
        .append(POP)
        .stop()
        .build();
    assert_child_guard_charged_nothing(
        EXTCODEHASH,
        child_code,
        VolatileDataAccessType::Beneficiary,
        "EXTCODEHASH",
    );
}

/// `SELFDESTRUCT(beneficiary)` (5,000 static gas — by far the largest of the guarded opcodes) must
/// not charge the frame it rejects.
///
/// REX4 routes `SELFDESTRUCT` through the beneficiary-conditional wrapper; REX5 replaces it with a
/// dedicated guard, covered by the `rex5` suite.
#[test]
fn test_guarded_selfdestruct_charges_nothing() {
    let child_code =
        BytecodeBuilder::default().push_address(BENEFICIARY).append(SELFDESTRUCT).build();
    assert_child_guard_charged_nothing(
        SELFDESTRUCT,
        child_code,
        VolatileDataAccessType::Beneficiary,
        "SELFDESTRUCT",
    );
}

/// A frame too poor to afford `BALANCE`'s static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_balance_reverts_with_all_gas_kept() {
    let child_code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    assert_underfunded_guard_charged_nothing(
        CHILD,
        BALANCE,
        child_code,
        VolatileDataAccessType::Beneficiary,
        WARM_ACCESS_STATIC_GAS,
        "underfunded BALANCE",
    );
}

/// The same underfunded frame with volatile access enabled still runs out of gas on `BALANCE`.
#[test]
fn test_underfunded_balance_runs_out_of_gas() {
    let child_code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    assert_underfunded_opcode_runs_out_of_gas(
        CHILD,
        BALANCE,
        child_code,
        WARM_ACCESS_STATIC_GAS,
        "underfunded BALANCE",
    );
}

/// A frame too poor to afford `SELFDESTRUCT`'s 5,000 static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_selfdestruct_reverts_with_all_gas_kept() {
    let child_code =
        BytecodeBuilder::default().push_address(BENEFICIARY).append(SELFDESTRUCT).build();
    assert_underfunded_guard_charged_nothing(
        CHILD,
        SELFDESTRUCT,
        child_code,
        VolatileDataAccessType::Beneficiary,
        SELFDESTRUCT_STATIC_GAS,
        "underfunded SELFDESTRUCT",
    );
}

// ============================================================================
// CALL-FAMILY OPCODES (`wrap_call_volatile_check!`)
// ============================================================================

/// `CALL(beneficiary)` (100 static gas) must not charge the frame it rejects.
#[test]
fn test_guarded_call_charges_nothing() {
    let child_code =
        append_call(BytecodeBuilder::default(), BENEFICIARY, 100_000).append(POP).stop().build();
    assert_child_guard_charged_nothing(
        CALL,
        child_code,
        VolatileDataAccessType::Beneficiary,
        "CALL",
    );
}

/// `STATICCALL(beneficiary)` (100 static gas) must not charge the frame it rejects.
#[test]
fn test_guarded_staticcall_charges_nothing() {
    let child_code = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(BENEFICIARY)
        .push_number(100_000_u64) // gas
        .append(STATICCALL)
        .append(POP)
        .stop()
        .build();
    assert_child_guard_charged_nothing(
        STATICCALL,
        child_code,
        VolatileDataAccessType::Beneficiary,
        "STATICCALL",
    );
}

/// A frame too poor to afford `CALL`'s static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_call_reverts_with_all_gas_kept() {
    let child_code =
        append_call(BytecodeBuilder::default(), BENEFICIARY, 100_000).append(POP).stop().build();
    assert_underfunded_guard_charged_nothing(
        CHILD,
        CALL,
        child_code,
        VolatileDataAccessType::Beneficiary,
        WARM_ACCESS_STATIC_GAS,
        "underfunded CALL",
    );
}

/// A frame too poor to afford `STATICCALL`'s static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_staticcall_reverts_with_all_gas_kept() {
    let child_code = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(BENEFICIARY)
        .push_number(100_000_u64) // gas
        .append(STATICCALL)
        .append(POP)
        .stop()
        .build();
    assert_underfunded_guard_charged_nothing(
        CHILD,
        STATICCALL,
        child_code,
        VolatileDataAccessType::Beneficiary,
        WARM_ACCESS_STATIC_GAS,
        "underfunded STATICCALL",
    );
}

// ============================================================================
// SELFBALANCE (`volatile_data_ext::selfbalance`)
// ============================================================================

/// `SELFBALANCE` in the beneficiary's own frame (5 static gas) must not charge that frame.
///
/// The guard fires on the *executing* contract, so the beneficiary's code has to run — and a CALL
/// to the beneficiary would itself be rejected by the CALL-family guard. The transaction therefore
/// targets the beneficiary directly and disables volatile access from inside its code.
#[test]
fn test_guarded_selfbalance_charges_nothing() {
    let inspector = run_beneficiary_selfbalance_frame(100_000_000);

    assert_guard_charged_nothing(&inspector, VolatileDataAccessType::Beneficiary, "SELFBALANCE");
}

/// Runs the beneficiary's own frame, which disables volatile access and then reaches `SELFBALANCE`.
///
/// The frame is the transaction's top-level one, so `gas_limit` is what funds it — a CALL into the
/// beneficiary would be rejected by the CALL-family guard before its code ever ran.
fn run_beneficiary_selfbalance_frame(gas_limit: u64) -> GuardedFrameGasInspector {
    let beneficiary_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .append(SELFBALANCE)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, beneficiary_code);

    let mut inspector = GuardedFrameGasInspector::new(BENEFICIARY, SELFBALANCE);
    transact_inspected(
        MegaSpecId::REX4,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(BENEFICIARY).gas_limit(gas_limit).build_fill(),
        &mut inspector,
    );
    inspector
}

/// A frame too poor to afford `SELFBALANCE`'s static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_selfbalance_reverts_with_all_gas_kept() {
    let remaining_at_opcode = SELFBALANCE_STATIC_GAS - 1;
    let gas_limit = calibrate_funding(remaining_at_opcode, |gas| {
        run_beneficiary_selfbalance_frame(gas)
            .remaining_before_opcode
            .expect("SELFBALANCE was never reached")
    });

    let inspector = run_beneficiary_selfbalance_frame(gas_limit);
    assert_eq!(
        inspector.remaining_before_opcode,
        Some(remaining_at_opcode),
        "the frame should reach SELFBALANCE holding {remaining_at_opcode} gas, one short of its \
         {SELFBALANCE_STATIC_GAS} static gas",
    );

    assert_guard_charged_nothing(
        &inspector,
        VolatileDataAccessType::Beneficiary,
        "underfunded SELFBALANCE",
    );
}

// ============================================================================
// ORACLE SLOAD (`volatile_data_ext::sload`)
// ============================================================================

/// `SLOAD` inside the oracle contract (100 static gas) must not charge the frame it rejects.
#[test]
fn test_guarded_oracle_sload_charges_nothing() {
    let oracle_code =
        BytecodeBuilder::default().push_number(0_u64).append(SLOAD).append(POP).stop().build();

    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code =
        append_call(parent_code, ORACLE_CONTRACT_ADDRESS, 50_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let mut inspector = GuardedFrameGasInspector::new(ORACLE_CONTRACT_ADDRESS, SLOAD);
    let success = transact_inspected(
        MegaSpecId::REX4,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill(),
        &mut inspector,
    );
    assert!(success, "only the oracle frame should revert, not the whole tx");

    assert_guard_charged_nothing(&inspector, VolatileDataAccessType::Oracle, "oracle SLOAD");
}

/// An oracle frame too poor to afford `SLOAD`'s static gas still gets the guard's revert.
#[test]
fn test_underfunded_guarded_oracle_sload_reverts_with_all_gas_kept() {
    let oracle_code =
        BytecodeBuilder::default().push_number(0_u64).append(SLOAD).append(POP).stop().build();
    assert_underfunded_guard_charged_nothing(
        ORACLE_CONTRACT_ADDRESS,
        SLOAD,
        oracle_code,
        VolatileDataAccessType::Oracle,
        WARM_ACCESS_STATIC_GAS,
        "underfunded oracle SLOAD",
    );
}
