//! The rewrite shapes an all-zero ledger used to admit.
//!
//! The measurement shim's contract is that a transaction an inspector rewrote never reaches a
//! block: the canonical path refuses one whose `InspectorLedger` is non-zero, so every rewrite has
//! to leave a mark on it. `measured_inspector.rs` and `inspector_cheat_matrix.rs` pin that per
//! mechanism and per callback × shape pair. This module pins the shapes that slipped *between*
//! those two questions — each one a rewrite the shim was handed, that changes what the transaction
//! produces, and that every lane read as nothing:
//!
//! - a frame's memory grown for free, by moving the interpreter's memory and the memo of how far it
//!   has been paid for in the same step, so that neither goes out of bounds and the next expanding
//!   opcode charges nothing;
//! - a `CallOutcome` / `CreateOutcome` metadata field — where the callee's return data lands, and
//!   which address a creation reports — rewritten without touching the `InterpreterResult` inside
//!   it, which is the only part the rewrite comparison used to read;
//! - two edits to the *same* signed lane in opposite directions, which a net-only reading cancels
//!   to zero;
//! - the same cancellation spread across two frames, where only one of the two survives to the
//!   receipt, so the net is zero and the effect is not.
//!
//! The first two are booked on `InspectorLedger::interventions`, from a snapshot the shim did not
//! used to take; the last two are what the per-lane gross activity counters exist for. Every test
//! here asserts the ledger the shim books *and* the effect the rewrite had, because a shape that no
//! longer changes anything is a shape that stopped testing the guard.

use crate::common::{transact, transact_inspected, CALLEE, CONTRACT, EMPTY_TARGET, ONE_ETH};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, MLOAD, MSTORE, MSTORE8, POP, RETURN, STOP},
    context::{Cfg, ContextTr},
    interpreter::{
        interpreter_types::{Jumps, MemoryTr},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter, InterpreterTypes,
    },
    Inspector,
};

/// A second callee, whose frame reverts.
const REVERTER: Address = EMPTY_TARGET;

/// The address a rewritten `CreateOutcome` reports instead of the one the code was deployed at.
const FAKE_DEPLOYMENT: Address = address!("00000000000000000000000000000000000f00d0");

/// Slot the fixtures write their observable result to.
const RESULT_SLOT: u64 = 0x11;

/// Refund a cancelling pair of refund edits moves, in each direction.
///
/// Small enough to stay well under the EIP-3529 cap on every fixture here, so that what survives
/// to the receipt is the whole of the surviving half rather than whatever the cap left of it.
const REFUND: i64 = 2_000;

fn db_with(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(crate::common::CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// The mainnet memory expansion cost of a memory `words` words long.
const fn memory_cost(words: u64) -> u64 {
    3 * words + words * words / 512
}

// --- a frame's memory, grown for free ------------------------------------------------------------

/// How far the free-expansion inspector grows the frame's memory, in words.
///
/// The fixture's own `MSTORE` lands inside it, so the expansion the EVM would have charged for is
/// exactly the one the inspector already did for nothing.
const STOLEN_WORDS: u64 = 129;

/// Grows the frame's memory and tells the EVM it is already paid for.
///
/// Both halves are needed and neither is a rewrite on its own. Moving the memory alone leaves the
/// memo behind, and the next expanding opcode charges for an expansion that already happened;
/// moving the memo alone leaves the memory behind, and the EVM reads out of bounds. Moving both
/// keeps every invariant the interpreter has and skips the charge, which is why the pair was the
/// hole and neither half was.
#[derive(Default)]
struct FreeExpansion {
    fired: u32,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for FreeExpansion {
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        if self.fired > 0 || interp.bytecode.opcode() != MSTORE {
            return;
        }
        let words = STOLEN_WORDS as usize;
        assert!(interp.memory.resize(words * 32), "the fixture must allow the memory to be grown",);
        // Priced through revm's own table, so the memo is exactly what the EVM would have written
        // had the frame paid; the assertion below restates the formula independently, which is
        // what makes the two a check rather than one number written twice.
        let cost = context.cfg().gas_params().memory_cost(words);
        interp.gas.memory_mut().set_words_num(words, cost);
        self.fired += 1;
    }
}

/// ★ A frame whose memory was grown for free is not an all-zero ledger.
///
/// The rewrite reaches through no argument the shim used to compare: the interpreter's gas counter
/// is untouched, no action is pending, no frame input and no frame result exists yet. What it
/// moves is the interpreter's memory and the memo beside it, and the transaction then pays less
/// than it would have — which is the one thing the guard exists to keep out of a block.
#[test]
fn test_a_frame_whose_memory_was_grown_for_free_is_booked() {
    // MSTORE(offset = STOLEN_WORDS * 32 - 32, value = 0xAA), which expands memory to exactly the
    // size the inspector already grew it to.
    let offset = (STOLEN_WORDS - 1) * 32;
    let code = BytecodeBuilder::default()
        .push_number(0xAAu64)
        .push_number(offset)
        .append(MSTORE)
        .append(STOP)
        .build();

    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7);
    let plain = transact(MegaSpecId::REX7, db_with(code.clone()), limits);
    let mut inspector = FreeExpansion::default();
    let cheated = transact_inspected(MegaSpecId::REX7, db_with(code), limits, &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach the expanding opcode exactly once");
    assert!(plain.is_success() && cheated.is_success(), "both runs must succeed");
    assert_eq!(
        plain.total_gas_spent - cheated.total_gas_spent,
        memory_cost(STOLEN_WORDS),
        "the expansion the inspector performed is the charge the EVM then skipped",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction that paid less because an inspector moved its memory must not read as \
         untouched: {:?}",
        cheated.inspector_ledger,
    );
}

// --- a call outcome's metadata -------------------------------------------------------------------

/// Where the fixture's `CALL` asks for its return data, and where the inspector moves it to.
const RETURN_AT: usize = 0;
const MOVED_TO: usize = 32;

/// Moves a finished call's return data somewhere else in the caller's memory.
///
/// The `InterpreterResult` inside the outcome — its classification, its output bytes, its gas —
/// comes back exactly as the EVM produced it. Only the range the caller will copy the output into
/// changes, which is not a field the result carries.
#[derive(Default)]
struct MoveReturnData {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for MoveReturnData {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address != CALLEE || self.fired > 0 {
            return;
        }
        outcome.memory_offset = MOVED_TO..MOVED_TO + 32;
        self.fired += 1;
    }
}

/// ★ A call outcome whose return range was moved is not an all-zero ledger.
#[test]
fn test_a_moved_return_range_is_booked() {
    // Size the caller's memory to two words, call the callee for one word of output at offset 0,
    // then store what landed there.
    let code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(32u64)
        .append(MSTORE)
        .push_number(32u64) // retSize
        .push_number(u64::try_from(RETURN_AT).unwrap()) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .push_number(u64::try_from(RETURN_AT).unwrap())
        .append(MLOAD)
        .push_number(RESULT_SLOT)
        .append(revm::bytecode::opcode::SSTORE)
        .append(STOP)
        .build();
    // The callee returns one word of 0x11s.
    let callee = BytecodeBuilder::default()
        .push_u256(U256::from(0x11u64))
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let db = || db_with(code.clone()).account_code(CALLEE, callee.clone());

    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7);
    let plain = transact(MegaSpecId::REX7, db(), limits);
    let mut inspector = MoveReturnData::default();
    let cheated = transact_inspected(MegaSpecId::REX7, db(), limits, &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach `call_end` for the callee once");
    assert_eq!(
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from(0x11u64),
        "without the rewrite the return data lands where the caller asked for it",
    );
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::ZERO,
        "with it, the caller reads a word the callee never wrote",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction whose state a rewritten return range changed must not read as untouched: \
         {:?}",
        cheated.inspector_ledger,
    );
}

/// Reports a different address than the one the creation deployed to.
#[derive(Default)]
struct MoveDeploymentAddress {
    fired: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for MoveDeploymentAddress {
    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if self.fired > 0 || outcome.address.is_none() {
            return;
        }
        outcome.address = Some(FAKE_DEPLOYMENT);
        self.fired += 1;
    }
}

/// ★ A creation outcome whose reported address was rewritten is not an all-zero ledger.
///
/// The code is still deployed where the EVM put it; only the address the caller's stack receives
/// changes, so the caller goes on to talk to an account that holds nothing.
#[test]
fn test_a_rewritten_deployment_address_is_booked() {
    // Init code that returns two bytes of runtime code.
    let init: [u8; 11] = [0x60, 0x00, 0x60, 0x00, 0x52, 0x60, 0x02, 0x60, 0x1e, 0xf3, 0x00];
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    let code = builder
        .push_number(init.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .push_number(RESULT_SLOT)
        .append(revm::bytecode::opcode::SSTORE)
        .append(STOP)
        .build();

    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7);
    let plain = transact(MegaSpecId::REX7, db_with(code.clone()), limits);
    let mut inspector = MoveDeploymentAddress::default();
    let cheated = transact_inspected(MegaSpecId::REX7, db_with(code), limits, &mut inspector);

    assert_eq!(inspector.fired, 1, "the fixture must reach `create_end` once");
    let deployed = plain.storage_value(CONTRACT, U256::from(RESULT_SLOT));
    assert_ne!(deployed, U256::ZERO, "the fixture's CREATE must succeed");
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        U256::from_be_slice(FAKE_DEPLOYMENT.as_slice()),
        "the caller must have been handed the address the inspector wrote",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a transaction told a contract lives somewhere it does not must not read as untouched: \
         {:?}",
        cheated.inspector_ledger,
    );
}

// --- two edits to one lane, in opposite directions -----------------------------------------------

/// Injects one gas before the frame reads its own remaining gas, and takes it back afterwards.
///
/// Both edits land on the interpreter counter, which is one signed lane. Their net is zero and
/// the transaction's envelope is unmoved — and in between them the frame read a number one higher
/// than the EVM would have given it, and wrote that number to storage.
#[derive(Default)]
struct CancellingCounterEdits {
    /// 0 before the injection, 1 between the two edits, 2 once both have landed.
    phase: u8,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CancellingCounterEdits {
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        match self.phase {
            0 if interp.bytecode.opcode() == GAS => {
                interp.gas.erase_cost(1);
                self.phase = 1;
            }
            1 => {
                assert!(interp.gas.record_regular_cost(1), "the frame must afford the give-back");
                self.phase = 2;
            }
            _ => {}
        }
    }
}

/// ★ Two edits to the same lane that cancel are not an all-zero ledger.
///
/// The net of the gas lane really is zero — the transaction spent exactly what it would have — so
/// nothing the conservation law reads has moved. What moved is the number the frame read in
/// between, and a guard that asks the net cannot see it. The gross activity counter is what does.
#[test]
fn test_cancelling_counter_edits_are_booked() {
    let code = BytecodeBuilder::default()
        .append(GAS)
        .push_number(RESULT_SLOT)
        .append(revm::bytecode::opcode::SSTORE)
        .append(STOP)
        .build();

    // No compute-gas limit, so the REX7 gas clamp hides nothing and the frame's own reading of
    // its remaining gas is the counter the injection moved.
    let limits = EvmTxRuntimeLimits::no_limits();
    let plain = transact(MegaSpecId::REX7, db_with(code.clone()), limits);
    let mut inspector = CancellingCounterEdits::default();
    let cheated = transact_inspected(MegaSpecId::REX7, db_with(code), limits, &mut inspector);

    assert_eq!(inspector.phase, 2, "both halves of the cancellation must have landed");
    assert_eq!(
        cheated.storage_value(CONTRACT, U256::from(RESULT_SLOT)),
        plain.storage_value(CONTRACT, U256::from(RESULT_SLOT)) + U256::from(1),
        "the frame must have read one gas more than the EVM would have given it",
    );
    assert_eq!(
        cheated.total_gas_spent, plain.total_gas_spent,
        "the two edits cancel, so the envelope the receipt reports is unmoved",
    );
    assert_eq!(
        cheated.inspector_conjured_gas, 0,
        "and so is the law's term: this is exactly the shape a net-only reading cannot see",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "but the transaction was rewritten, and the guard has to see that: {:?}",
        cheated.inspector_ledger,
    );
}

/// Adds a refund to one child frame's result and takes the same amount out of another's.
///
/// The frame that gets the addition returns, so its refund reaches the receipt. The frame that
/// gets the subtraction reverts, so revm discards its whole refund counter — the subtraction never
/// reaches anything. Net zero on the lane, one refund's worth of difference on the receipt.
#[derive(Default)]
struct CancellingRefundsAcrossFrames {
    added: u32,
    removed: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CancellingRefundsAcrossFrames {
    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == CALLEE && self.added == 0 {
            outcome.result.gas.record_refund(REFUND);
            self.added += 1;
        } else if inputs.target_address == REVERTER && self.removed == 0 {
            assert!(
                outcome.result.gas.refunded() >= REFUND,
                "the reverting callee must hold a refund of its own to take from, got {}",
                outcome.result.gas.refunded(),
            );
            outcome.result.gas.record_refund(-REFUND);
            self.removed += 1;
        }
    }
}

/// ★ A cancellation split across a surviving frame and a discarded one is not an all-zero ledger.
///
/// This is the previous shape with the asymmetry made explicit: the two halves are equal and
/// opposite where the ledger books them, and only one of them is still standing by the time the
/// receipt is built.
#[test]
fn test_cancelling_refunds_across_frames_are_booked() {
    let call_to = |builder: BytecodeBuilder, target: Address| {
        builder
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(target)
            .push_number(200_000u64)
            .append(CALL)
            .append(POP)
    };
    let code = call_to(call_to(BytecodeBuilder::default(), CALLEE), REVERTER).append(STOP).build();
    // Both callees set a slot and clear it again, so each ends holding a refund the EVM produced.
    let clearing = |builder: BytecodeBuilder| {
        builder
            .sstore(U256::from(RESULT_SLOT), U256::from(1u64))
            .sstore(U256::from(RESULT_SLOT), U256::ZERO)
    };
    let returning = clearing(BytecodeBuilder::default()).append(STOP).build();
    let reverting = clearing(BytecodeBuilder::default()).revert().build();
    let db = || {
        db_with(code.clone())
            .account_code(CALLEE, returning.clone())
            .account_code(REVERTER, reverting.clone())
    };

    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7);
    let plain = transact(MegaSpecId::REX7, db(), limits);
    let mut inspector = CancellingRefundsAcrossFrames::default();
    let cheated = transact_inspected(MegaSpecId::REX7, db(), limits, &mut inspector);

    assert_eq!((inspector.added, inspector.removed), (1, 1), "both halves must have landed");
    assert!(
        plain.total_gas_spent >= 5 * u64::try_from(REFUND).unwrap(),
        "the fixture must burn enough that the EIP-3529 cap does not hide the difference",
    );
    assert_eq!(
        plain.gas_used - cheated.gas_used,
        u64::try_from(REFUND).unwrap(),
        "only the surviving frame's half reaches the receipt, so the sender pays that much less",
    );
    assert!(
        !cheated.inspector_ledger.is_zero(),
        "a receipt an inspector moved must not read as untouched: {:?}",
        cheated.inspector_ledger,
    );
}
