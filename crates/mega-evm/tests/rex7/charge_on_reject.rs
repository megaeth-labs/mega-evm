//! REX7 charge-on-reject for `disableVolatileDataAccess` guards.
//!
//! Through REX6 a volatile guard that rejects its opcode charges nothing: the static gas table
//! zeroes the entry so the interpreter cannot pre-empt the guard, and the handler charges only
//! after the check declines. REX7 keeps the zeroed table — the guard must still be reachable —
//! but charges the static entry before rejecting, so the revert keeps the fee and segment
//! settlement records it as compute.
//!
//! Each test runs the same rejected shape on both specs. REX6 remaining and compute stay at the
//! historical zero-charge amounts; REX7 remaining drops by exactly the opcode's static entry and
//! compute / receipt `gas_used` rise by that same amount.
//!
//! A second set of tests pins the unaffordable-static-fee branch: the guarded frame is given
//! just enough gas to reach the opcode and not enough to pay the entry, so the result is
//! `OutOfGas` rather than a `VolatileDataAccessDisabled` revert.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaAccessControl, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    TestExternalEnvs, VolatileDataAccessType, ACCESS_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{
        BALANCE, BLOCKHASH, CALL, POP, SELFBALANCE, SELFDESTRUCT, SLOAD, STATICCALL, TIMESTAMP,
    },
    context::{tx::TxEnvBuilder, BlockEnv, ContextTr, TxEnv},
    handler::EvmTr,
    inspector::Inspector,
    interpreter::{
        interpreter_types::{InputsTr, Jumps},
        CallInputs, CallOutcome, InstructionResult, Interpreter, InterpreterTypes,
    },
};

const CALLER: Address = address!("0000000000000000000000000000000000310000");
const PARENT: Address = address!("0000000000000000000000000000000000310001");
const CHILD: Address = address!("0000000000000000000000000000000000310002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000310099");

const DISABLE_SELECTOR: [u8; 4] = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

const TIMESTAMP_STATIC_GAS: u64 = 2;
const BLOCKHASH_STATIC_GAS: u64 = 20;
const WARM_ACCESS_STATIC_GAS: u64 = 100;
const SELFBALANCE_STATIC_GAS: u64 = 5;
const SELFDESTRUCT_STATIC_GAS: u64 = 5_000;

struct GuardedFrameOutcome {
    remaining: u64,
    result: InstructionResult,
    output: Bytes,
}

/// Records the gas the guarded frame held on reaching `opcode`, and the outcome that frame
/// returned. `step` runs before the handler, so `remaining_before` is the pre-charge budget.
struct GuardedFrameGasInspector {
    frame: Address,
    opcode: u8,
    remaining_before: Option<u64>,
    outcome: Option<GuardedFrameOutcome>,
}

impl GuardedFrameGasInspector {
    fn new(frame: Address, opcode: u8) -> Self {
        Self { frame, opcode, remaining_before: None, outcome: None }
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for GuardedFrameGasInspector {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        if interp.input.target_address() == self.frame && interp.bytecode.opcode() == self.opcode {
            self.remaining_before = Some(interp.gas.remaining());
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

struct RejectedGuard {
    remaining_before: u64,
    remaining_after: u64,
    compute_gas: u64,
    gas_used: u64,
    output: Bytes,
}

fn call_disable(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .mstore(0x0, DISABLE_SELECTOR)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(4_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(CALL)
        .append(POP)
}

fn append_call(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

fn transact_rejected(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
    inspector: &mut GuardedFrameGasInspector,
) -> (bool, u64, u64) {
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
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    (result.result.is_success(), compute_gas, result.result.tx_gas_used())
}

fn run_child_reject(
    spec: MegaSpecId,
    opcode: u8,
    child: Address,
    child_code: Bytes,
) -> RejectedGuard {
    let parent_code = append_call(call_disable(BytecodeBuilder::default()), child, 50_000_000)
        .append(POP)
        .stop()
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(child, child_code);
    let mut inspector = GuardedFrameGasInspector::new(child, opcode);
    let (success, compute_gas, gas_used) = transact_rejected(
        spec,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill(),
        &mut inspector,
    );
    assert!(success, "{spec}: only the guarded child should revert");
    take_rejected(spec, inspector, compute_gas, gas_used)
}

fn run_selfbalance_reject(spec: MegaSpecId) -> RejectedGuard {
    let code =
        call_disable(BytecodeBuilder::default()).append(SELFBALANCE).append(POP).stop().build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, code);
    let mut inspector = GuardedFrameGasInspector::new(BENEFICIARY, SELFBALANCE);
    let (_success, compute_gas, gas_used) = transact_rejected(
        spec,
        &mut db,
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(BENEFICIARY)
            .gas_limit(100_000_000)
            .build_fill(),
        &mut inspector,
    );
    take_rejected(spec, inspector, compute_gas, gas_used)
}

fn take_rejected(
    spec: MegaSpecId,
    inspector: GuardedFrameGasInspector,
    compute_gas: u64,
    gas_used: u64,
) -> RejectedGuard {
    let remaining_before = inspector
        .remaining_before
        .unwrap_or_else(|| panic!("{spec}: guarded opcode was never reached"));
    let outcome = inspector.outcome.unwrap_or_else(|| panic!("{spec}: guarded frame never ended"));
    assert_eq!(
        outcome.result,
        InstructionResult::Revert,
        "{spec}: guard must revert, got {:?}",
        outcome.result
    );
    RejectedGuard {
        remaining_before,
        remaining_after: outcome.remaining,
        compute_gas,
        gas_used,
        output: outcome.output,
    }
}

fn assert_charge_on_reject(
    label: &str,
    static_gas: u64,
    access_type: VolatileDataAccessType,
    r6: RejectedGuard,
    r7: RejectedGuard,
) {
    assert_charge_on_reject_inner(label, static_gas, access_type, r6, r7, true)
}

/// Same as [`assert_charge_on_reject`], but skips the in-frame remaining-before check.
///
/// Needed when the guarded frame is the transaction recipient and that recipient is the
/// beneficiary: REX7 clamps interpreter-visible gas to the detained headroom, so
/// `remaining_before` is the clamped counter and is not the amount the revert hands back.
fn assert_charge_on_reject_after_restore(
    label: &str,
    static_gas: u64,
    access_type: VolatileDataAccessType,
    r6: RejectedGuard,
    r7: RejectedGuard,
) {
    assert_charge_on_reject_inner(label, static_gas, access_type, r6, r7, false)
}

fn assert_charge_on_reject_inner(
    label: &str,
    static_gas: u64,
    access_type: VolatileDataAccessType,
    r6: RejectedGuard,
    r7: RejectedGuard,
    check_in_frame_debit: bool,
) {
    assert_eq!(
        r6.remaining_after, r6.remaining_before,
        "{label}: REX6 must reject without charging; held {} returned {}",
        r6.remaining_before, r6.remaining_after
    );
    if check_in_frame_debit {
        assert_eq!(
            r7.remaining_after,
            r7.remaining_before - static_gas,
            "{label}: REX7 must charge exactly {static_gas}; held {} returned {}",
            r7.remaining_before,
            r7.remaining_after
        );
    }
    assert_eq!(
        r7.remaining_after,
        r6.remaining_after - static_gas,
        "{label}: REX7 true remaining must be REX6's minus {static_gas}; REX6={} REX7={}",
        r6.remaining_after,
        r7.remaining_after
    );
    assert_eq!(
        r7.compute_gas,
        r6.compute_gas + static_gas,
        "{label}: REX7 compute must include the rejected static fee; REX6={} REX7={}",
        r6.compute_gas,
        r7.compute_gas
    );
    assert_eq!(
        r7.gas_used,
        r6.gas_used + static_gas,
        "{label}: REX7 gas_used must include the rejected static fee; REX6={} REX7={}",
        r6.gas_used,
        r7.gas_used
    );

    let expected = IMegaAccessControl::VolatileDataAccessDisabled { accessType: access_type };
    let encoded = expected.abi_encode();
    assert_eq!(r6.output.as_ref(), encoded.as_slice(), "{label}: REX6 revert data");
    assert_eq!(r7.output.as_ref(), encoded.as_slice(), "{label}: REX7 revert data");
}

/// Unconditional block-env family: `TIMESTAMP` (static 2).
#[test]
fn test_rejected_timestamp_charges_static_gas_only_on_rex7() {
    let code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();
    let r6 = run_child_reject(MegaSpecId::REX6, TIMESTAMP, CHILD, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, TIMESTAMP, CHILD, code);
    assert_charge_on_reject(
        "TIMESTAMP",
        TIMESTAMP_STATIC_GAS,
        VolatileDataAccessType::Timestamp,
        r6,
        r7,
    );
}

/// Same family, different static entry, so a shared constant would fail here.
#[test]
fn test_rejected_blockhash_charges_static_gas_only_on_rex7() {
    let code =
        BytecodeBuilder::default().push_number(0_u64).append(BLOCKHASH).append(POP).stop().build();
    let r6 = run_child_reject(MegaSpecId::REX6, BLOCKHASH, CHILD, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, BLOCKHASH, CHILD, code);
    assert_charge_on_reject(
        "BLOCKHASH",
        BLOCKHASH_STATIC_GAS,
        VolatileDataAccessType::BlockHash,
        r6,
        r7,
    );
}

/// Beneficiary-conditional family: `BALANCE(beneficiary)` (static 100).
#[test]
fn test_rejected_balance_charges_static_gas_only_on_rex7() {
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    let r6 = run_child_reject(MegaSpecId::REX6, BALANCE, CHILD, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, BALANCE, CHILD, code);
    assert_charge_on_reject(
        "BALANCE",
        WARM_ACCESS_STATIC_GAS,
        VolatileDataAccessType::Beneficiary,
        r6,
        r7,
    );
}

/// CALL family: `CALL(beneficiary)` (static 100, the zeroed-table entry).
#[test]
fn test_rejected_call_charges_static_gas_only_on_rex7() {
    let code =
        append_call(BytecodeBuilder::default(), BENEFICIARY, 100_000).append(POP).stop().build();
    let r6 = run_child_reject(MegaSpecId::REX6, CALL, CHILD, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, CALL, CHILD, code);
    assert_charge_on_reject(
        "CALL",
        WARM_ACCESS_STATIC_GAS,
        VolatileDataAccessType::Beneficiary,
        r6,
        r7,
    );
}

/// CALL family, different arity: `STATICCALL(beneficiary)`.
#[test]
fn test_rejected_staticcall_charges_static_gas_only_on_rex7() {
    let code = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(BENEFICIARY)
        .push_number(100_000_u64)
        .append(STATICCALL)
        .append(POP)
        .stop()
        .build();
    let r6 = run_child_reject(MegaSpecId::REX6, STATICCALL, CHILD, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, STATICCALL, CHILD, code);
    assert_charge_on_reject(
        "STATICCALL",
        WARM_ACCESS_STATIC_GAS,
        VolatileDataAccessType::Beneficiary,
        r6,
        r7,
    );
}

/// Oracle-conditional `SLOAD` (static 100).
#[test]
fn test_rejected_oracle_sload_charges_static_gas_only_on_rex7() {
    let code =
        BytecodeBuilder::default().push_number(0_u64).append(SLOAD).append(POP).stop().build();
    let r6 = run_child_reject(MegaSpecId::REX6, SLOAD, ORACLE_CONTRACT_ADDRESS, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, SLOAD, ORACLE_CONTRACT_ADDRESS, code);
    assert_charge_on_reject(
        "oracle SLOAD",
        WARM_ACCESS_STATIC_GAS,
        VolatileDataAccessType::Oracle,
        r6,
        r7,
    );
}

/// `SELFBALANCE` in the beneficiary's own frame (static 5). A CALL into the beneficiary would
/// itself be rejected, so the transaction targets the beneficiary directly.
#[test]
fn test_rejected_selfbalance_charges_static_gas_only_on_rex7() {
    let r6 = run_selfbalance_reject(MegaSpecId::REX6);
    let r7 = run_selfbalance_reject(MegaSpecId::REX7);
    assert_charge_on_reject_after_restore(
        "SELFBALANCE",
        SELFBALANCE_STATIC_GAS,
        VolatileDataAccessType::Beneficiary,
        r6,
        r7,
    );
}

/// A child that reaches `opcode` holding less than its static fee must OOG rather than
/// produce the disable revert. `forward_gas` is the CALL stipend the parent hands the child
/// — enough to reach the opcode, not enough to pay the entry.
fn run_child_underfunded(
    opcode: u8,
    child: Address,
    child_code: Bytes,
    forward_gas: u64,
) -> (u64, GuardedFrameOutcome) {
    let parent_code = append_call(call_disable(BytecodeBuilder::default()), child, forward_gas)
        .append(POP)
        .stop()
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(PARENT, parent_code)
        .account_code(child, child_code);
    let mut inspector = GuardedFrameGasInspector::new(child, opcode);
    let (success, _compute_gas, _gas_used) = transact_rejected(
        MegaSpecId::REX7,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill(),
        &mut inspector,
    );
    assert!(success, "only the underfunded child should fail");
    let remaining_before = inspector
        .remaining_before
        .unwrap_or_else(|| panic!("underfunded opcode {opcode} was never reached"));
    let outcome = inspector.outcome.unwrap_or_else(|| panic!("underfunded frame never ended"));
    (remaining_before, outcome)
}

fn assert_oog_not_disable_revert(
    label: &str,
    static_gas: u64,
    remaining_before: u64,
    outcome: &GuardedFrameOutcome,
) {
    assert!(
        remaining_before < static_gas,
        "{label}: remaining_before={remaining_before} must be below static {static_gas}"
    );
    assert_eq!(
        outcome.result,
        InstructionResult::OutOfGas,
        "{label}: unaffordable static fee must OOG, got {:?}",
        outcome.result
    );
    assert!(
        outcome.output.is_empty(),
        "{label}: OOG must not carry a disable-revert payload, got {:?}",
        outcome.output
    );
}

/// Unconditional family: child holds 1 gas, `TIMESTAMP` costs 2.
#[test]
fn test_rejected_timestamp_oog_when_static_gas_unaffordable() {
    let code = BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build();
    let (remaining_before, outcome) = run_child_underfunded(TIMESTAMP, CHILD, code, 1);
    assert_oog_not_disable_revert("TIMESTAMP", TIMESTAMP_STATIC_GAS, remaining_before, &outcome);
}

/// Same family, larger static entry: `PUSH1 0` (3) then `BLOCKHASH` (20), forwarded 4.
#[test]
fn test_rejected_blockhash_oog_when_static_gas_unaffordable() {
    let code =
        BytecodeBuilder::default().push_number(0_u64).append(BLOCKHASH).append(POP).stop().build();
    let (remaining_before, outcome) = run_child_underfunded(BLOCKHASH, CHILD, code, 4);
    assert_oog_not_disable_revert("BLOCKHASH", BLOCKHASH_STATIC_GAS, remaining_before, &outcome);
}

/// Conditional account-read family: `PUSH20 beneficiary` (3) then `BALANCE` (100), forwarded 4.
#[test]
fn test_rejected_balance_oog_when_static_gas_unaffordable() {
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    let (remaining_before, outcome) = run_child_underfunded(BALANCE, CHILD, code, 4);
    assert_oog_not_disable_revert("BALANCE", WARM_ACCESS_STATIC_GAS, remaining_before, &outcome);
}

/// CALL family: setup is five `PUSH1 0` + `PUSH20` + `PUSH3` (21), then `CALL` (100).
#[test]
fn test_rejected_call_oog_when_static_gas_unaffordable() {
    let code =
        append_call(BytecodeBuilder::default(), BENEFICIARY, 100_000).append(POP).stop().build();
    let (remaining_before, outcome) =
        run_child_underfunded(CALL, CHILD, code, 21 + WARM_ACCESS_STATIC_GAS - 1);
    assert_oog_not_disable_revert("CALL", WARM_ACCESS_STATIC_GAS, remaining_before, &outcome);
}

/// Oracle `SLOAD`: `PUSH1 0` (3) then `SLOAD` (100), forwarded 4.
#[test]
fn test_rejected_oracle_sload_oog_when_static_gas_unaffordable() {
    let code =
        BytecodeBuilder::default().push_number(0_u64).append(SLOAD).append(POP).stop().build();
    let (remaining_before, outcome) =
        run_child_underfunded(SLOAD, ORACLE_CONTRACT_ADDRESS, code, 4);
    assert_oog_not_disable_revert(
        "oracle SLOAD",
        WARM_ACCESS_STATIC_GAS,
        remaining_before,
        &outcome,
    );
}

/// `SELFDESTRUCT`: `PUSH20` (3) then static `5000`, forwarded 4.
#[test]
fn test_rejected_selfdestruct_oog_when_static_gas_unaffordable() {
    let code = BytecodeBuilder::default().push_address(BENEFICIARY).append(SELFDESTRUCT).build();
    let (remaining_before, outcome) = run_child_underfunded(SELFDESTRUCT, CHILD, code, 4);
    assert_oog_not_disable_revert(
        "SELFDESTRUCT",
        SELFDESTRUCT_STATIC_GAS,
        remaining_before,
        &outcome,
    );
}

/// `SELFBALANCE` is only rejected in the beneficiary's own frame. `GAS` after `disable`
/// stores the true remaining; the transaction gas limit is then cut so the frame holds
/// `static − 1` at `SELFBALANCE`.
#[test]
fn test_rejected_selfbalance_oog_when_static_gas_unaffordable() {
    use revm::bytecode::opcode::{GAS, SSTORE};
    const HIGH_LIMIT: u64 = 100_000_000;
    const GAS_SLOT: u64 = 0x67;

    let calibrate_code = call_disable(BytecodeBuilder::default())
        .append(GAS)
        .push_u256(U256::from(GAS_SLOT))
        .append(SSTORE)
        .stop()
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, calibrate_code);
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX7)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(BENEFICIARY).gas_limit(HIGH_LIMIT).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("calibration must execute");
    assert!(result.result.is_success(), "calibration run must succeed: {:?}", result.result);
    let true_after_gas: u64 = result
        .state
        .get(&BENEFICIARY)
        .and_then(|account| account.storage.get(&U256::from(GAS_SLOT)))
        .map(|slot| slot.present_value())
        .expect("calibration SSTORE must land")
        .try_into()
        .expect("remaining fits in u64");
    // `GAS` charges 2 and then pushes the post-charge remaining. `SELFBALANCE` sits where
    // `GAS` sat, so the true remaining there is that reading plus 2.
    let true_at_selfbalance = true_after_gas + 2;
    let gas_limit = HIGH_LIMIT - true_at_selfbalance + (SELFBALANCE_STATIC_GAS - 1);

    let code =
        call_disable(BytecodeBuilder::default()).append(SELFBALANCE).append(POP).stop().build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(BENEFICIARY, code);
    let mut inspector = GuardedFrameGasInspector::new(BENEFICIARY, SELFBALANCE);
    let _ = transact_rejected(
        MegaSpecId::REX7,
        &mut db,
        TxEnvBuilder::default().caller(CALLER).call(BENEFICIARY).gas_limit(gas_limit).build_fill(),
        &mut inspector,
    );
    let remaining_before = inspector.remaining_before.expect("SELFBALANCE must be reached");
    let outcome = inspector.outcome.expect("beneficiary frame must end");
    assert_oog_not_disable_revert(
        "SELFBALANCE",
        SELFBALANCE_STATIC_GAS,
        remaining_before,
        &outcome,
    );
}

/// `SELFDESTRUCT(beneficiary)` (static 5,000) — shared REX5+ wrapper, runtime-gated on REX7.
#[test]
fn test_rejected_selfdestruct_charges_static_gas_only_on_rex7() {
    let code = BytecodeBuilder::default().push_address(BENEFICIARY).append(SELFDESTRUCT).build();
    let r6 = run_child_reject(MegaSpecId::REX6, SELFDESTRUCT, CHILD, code.clone());
    let r7 = run_child_reject(MegaSpecId::REX7, SELFDESTRUCT, CHILD, code);
    assert_charge_on_reject(
        "SELFDESTRUCT",
        SELFDESTRUCT_STATIC_GAS,
        VolatileDataAccessType::Beneficiary,
        r6,
        r7,
    );
}
