//! Shared fixtures for the tests that attach an inspector.
//!
//! [`crate::common`] drives the transaction and checks the conservation law on every run. What is
//! left over — the REX7 limits, the bytecode shapes an inspector needs something to reach into, the
//! one-lane ledgers a test asserts against, and the two ways a refused rewrite surfaces — lives
//! here, because a rewrite is only ever pinned by comparing an inspected run against the
//! uninspected one over the same fixture.

use alloy_primitives::{Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, InspectorLedger, Lane, MegaContext, MegaEvm, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, DUP1, JUMPDEST, JUMPI, MSTORE8, POP, STOP, SUB, SWAP1},
    Inspector,
};
use std::{boxed::Box, string::String, vec::Vec};

use crate::common::{call_contract_tx, context, plain_filler, CALLEE, DEFAULT_TX_GAS_LIMIT};

/// The spec every fixture here runs under, and its default runtime limits.
pub(crate) fn limits() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)
}

/// [`limits`] with the per-transaction compute budget lowered to `limit`.
pub(crate) fn limits_with_compute(limit: u64) -> EvmTxRuntimeLimits {
    limits().with_tx_compute_gas_limit(limit)
}

// --- bytecode ------------------------------------------------------------------------------

/// A `CALL` to `target` forwarding `gas` and `value`, with empty argument and return ranges.
pub(crate) fn append_call(
    builder: BytecodeBuilder,
    target: Address,
    gas: u64,
    value: u64,
) -> BytecodeBuilder {
    builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// The minimal frame that makes one inner call and ignores what it returned.
pub(crate) fn call_then_stop(target: Address, gas: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), target, gas, 0).append(POP).append(STOP).build()
}

/// A straight run of `pairs` plain opcodes that always succeeds.
pub(crate) fn plain_run_code(pairs: usize) -> Bytes {
    plain_filler(BytecodeBuilder::default(), pairs).append(STOP).build()
}

/// A countdown loop of plain opcodes with no checkpoint anywhere in the body, so the whole run is
/// one settlement segment and the gas clamp is the only thing enforcing the compute limit inside
/// it.
pub(crate) fn countdown_loop_code(iterations: u16) -> Bytes {
    let mut code = Vec::new();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = u8::try_from(code.len()).expect("loop target must fit in a PUSH1");
    code.push(JUMPDEST);
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(SWAP1);
    code.push(SUB);
    code.push(DUP1);
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(JUMPI);
    code.push(STOP);
    Bytes::from(code)
}

/// Writes `init_code` into memory a byte at a time and `CREATE`s from it, so a test can choose the
/// constructor without a second account.
pub(crate) fn deploy_then_stop(init_code: &[u8]) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for (offset, byte) in init_code.iter().enumerate() {
        builder = builder.push_number(u64::from(*byte)).push_number(offset as u64).append(MSTORE8);
    }
    builder
        .push_number(init_code.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

/// [`crate::common::base_db`] with `callee` installed at [`CALLEE`].
pub(crate) fn db_with_callee(code: Bytes, callee: Bytes) -> MemoryDatabase {
    crate::common::base_db(code).account_code(CALLEE, callee)
}

// --- ledgers -------------------------------------------------------------------------------

/// The ledger of a rewrite that moved gas on exactly one lane.
///
/// Separate constructors rather than one, because which lane a shape moves is exactly what decides
/// whether the conservation law can see it: only the gas, envelope, result and reservoir lanes are
/// terms of it.
pub(crate) fn ledger_gas(gas: i128) -> InspectorLedger {
    InspectorLedger { gas: Lane::once(gas), ..InspectorLedger::default() }
}

pub(crate) fn ledger_env(env: i128) -> InspectorLedger {
    InspectorLedger { env: Lane::once(env), ..InspectorLedger::default() }
}

pub(crate) fn ledger_result(result: i128) -> InspectorLedger {
    InspectorLedger { result: Lane::once(result), ..InspectorLedger::default() }
}

pub(crate) fn ledger_refund(refund: i128) -> InspectorLedger {
    InspectorLedger { refund: Lane::once(refund), ..InspectorLedger::default() }
}

pub(crate) fn ledger_reservoir(reservoir: i128) -> InspectorLedger {
    InspectorLedger { reservoir: Lane::once(reservoir), ..InspectorLedger::default() }
}

pub(crate) fn ledger_state_gas(state_gas: i128) -> InspectorLedger {
    InspectorLedger { state_gas: Lane::once(state_gas), ..InspectorLedger::default() }
}

/// The ledger of a rewrite that moves no gas: the shim saw the argument it was handed come back
/// changed, and that is the whole of what it books.
///
/// These are the cells that would otherwise be indistinguishable from an observation-only run, and
/// the reason the canonical block path could not tell them apart before this lane existed.
pub(crate) fn ledger_intervention() -> InspectorLedger {
    InspectorLedger { interventions: 1, ..InspectorLedger::default() }
}

// --- refusals ------------------------------------------------------------------------------

/// [`crate::common::transact_inspected`] surfacing the `EVMError` instead of panicking on it.
pub(crate) fn try_transact_inspected<I>(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    inspector: &mut I,
) -> Result<(), String>
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let mut db = db;
    let mut evm =
        MegaEvm::new(context(&mut db, MegaSpecId::REX7, limits)).with_inspector(inspector);
    evm.execute_transaction(call_contract_tx(DEFAULT_TX_GAS_LIMIT))
        .map(|_| ())
        .map_err(|e| std::format!("{e:?}"))
}

/// Drives `run` and asserts the shim refused the rewrite, however this build surfaces a refusal:
/// a debug build asserts (the shape is a detector, and a corpus that produces it should stop), a
/// release build fails the transaction with the same message.
pub(crate) fn assert_refused(message: &str, run: impl Fn() -> Result<(), String>) {
    if cfg!(debug_assertions) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
        std::panic::set_hook(previous);
        let payload = panicked.expect_err("the detector must fire in debug builds");
        let caught = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(caught.contains(message), "the assertion must name the shape; got {caught:?}");
    } else {
        let error = run().expect_err("the refusal must surface as an EVMError in release builds");
        assert!(error.contains(message), "the error must name the shape; got {error:?}");
    }
}

/// The message the shim refuses a resurrected creation with, on both of the paths that can catch
/// it.
pub(crate) const REVIVED_CREATION: &str =
    "inspector rewrote a failed contract creation into a successful one";

/// Init code that reverts immediately, so the creation it is handed to fails.
pub(crate) const REVERTING_INIT_CODE: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xfd];

/// A slot as the produced state has it, taking the slot as the small integer the fixtures use.
pub(crate) fn slot_of(outcome: &crate::common::Outcome, address: Address, slot: u64) -> U256 {
    outcome.storage_value(address, U256::from(slot))
}
