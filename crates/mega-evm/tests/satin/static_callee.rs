//! The halt reason of a creation inside a static call.
//!
//! A `CREATE` or `CREATE2` in a static frame halts with `StateChangeDuringStaticCall` before it
//! reads an operand, sizes its init code or charges anything, as in canonical revm. The
//! transaction does not expose an inner frame's halt reason, so these read it off the frame's
//! outcome as the caller receives it. The transactions are the differential corpus's
//! `create2_static_*` and `create_static_low_gas` scenarios.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm,
};
use revm::{
    bytecode::opcode::{CREATE, CREATE2, STATICCALL, STOP},
    interpreter::{interpreter::EthInterpreter, CallInputs, CallOutcome, InstructionResult},
    Database, Inspector,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000700000");
const CONTRACT: Address = address!("0000000000000000000000000000000000700001");
const STATIC_CALLEE: Address = address!("0000000000000000000000000000000000700002");

/// The result of the static callee's frame, as its caller receives it.
#[derive(Default)]
struct CalleeResult(Option<InstructionResult>);

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for CalleeResult {
    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == STATIC_CALLEE {
            self.0 = Some(outcome.result.result);
        }
    }
}

/// Runs `CONTRACT`, which `STATICCALL`s `STATIC_CALLEE` running `callee` with `forward_gas`, and
/// returns the callee's result.
fn static_callee_result(callee: Bytes, forward_gas: u64) -> Option<InstructionResult> {
    let caller_code = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(STATIC_CALLEE)
        .push_number(forward_gas)
        .append(STATICCALL)
        .append(STOP)
        .build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, caller_code)
        .account_code(STATIC_CALLEE, callee);
    let mut evm = MegaEvm::new(context(db)).with_inspector(CalleeResult::default());
    let result = evm.transact_raw(call(CALLER, CONTRACT, U256::ZERO, 100_000_000)).unwrap();
    assert!(result.result.is_success(), "the static call's failure does not fail the caller");
    evm.inspector().0
}

/// `CREATE2(value 0, offset 0, size, salt 0)`.
fn create2(initcode_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(initcode_len)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE2)
        .append(STOP)
        .build()
}

const STATIC_REJECTION: Option<InstructionResult> =
    Some(InstructionResult::StateChangeDuringStaticCall);

/// Oversized init code in a static frame reports the static rejection, not the size limit.
#[test]
fn test_create2_static_oversized_initcode_reports_static_rejection() {
    assert_eq!(static_callee_result(create2(600_000), 50_000_000), STATIC_REJECTION);
}

/// So does init code whose memory could never be paid for: nothing is sized.
#[test]
fn test_create2_static_hugely_oversized_initcode_halt_reason() {
    assert_eq!(static_callee_result(create2(10 * 1024 * 1024), 50_000_000), STATIC_REJECTION);
}

/// Empty init code in a static frame reports the static rejection.
#[test]
fn test_create2_static_zero_length_initcode_reports_static_rejection() {
    assert_eq!(static_callee_result(create2(0), 50_000_000), STATIC_REJECTION);
}

/// With little gas forwarded the static rejection still comes first: nothing is charged.
#[test]
fn test_create2_static_zero_length_low_gas_halt_reason() {
    assert_eq!(static_callee_result(create2(0), 10_000), STATIC_REJECTION);
}

/// Plain `CREATE` is rejected the same way.
#[test]
fn test_create_static_low_gas_halt_reason() {
    let create = BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(STOP)
        .build();
    assert_eq!(static_callee_result(create, 10_000), STATIC_REJECTION);
}

/// The rejection precedes the operand reads: a bare `CREATE2` reports it, not a stack underflow.
#[test]
fn test_create2_static_missing_operands_halt_reason() {
    let bare = BytecodeBuilder::default().append(CREATE2).append(STOP).build();
    assert_eq!(static_callee_result(bare, 50_000_000), STATIC_REJECTION);
}
