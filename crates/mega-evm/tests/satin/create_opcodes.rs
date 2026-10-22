//! What a `CREATE2` does with its operands before it starts a frame, as revm does it.
//!
//! Satin runs revm's own `CREATE2` and charges the records of the frame it starts afterwards, so
//! everything up to the frame's start is canonical: a missing operand is a `StackUnderflow`; an
//! init code over the limit is refused before its offset is read or memory is touched; an empty
//! init code reads no offset, touches no memory and hashes to `KECCAK_EMPTY`; and a non-empty one
//! pays for its words — the memory it expands, EIP-3860's cost and the hash — before the creation
//! starts.
//!
//! An inner frame's memory is not visible in the transaction's result, so [`CreateMemory`] reads it
//! after the `CREATE2` instruction ran, halted or not.

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use mega_evm::{
    constants::MAX_INITCODE_SIZE,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CREATE2, STOP},
    context::result::{ExecutionResult, HaltReason, OutOfGasError},
    interpreter::{interpreter::EthInterpreter, interpreter_types::Jumps, Interpreter},
    primitives::KECCAK_EMPTY,
    Database, Inspector,
};

use crate::common::{body_history, call, context};

const CALLER: Address = address!("0000000000000000000000000000000000e10000");
const CONTRACT: Address = address!("0000000000000000000000000000000000e10001");

/// The memory size of the frame that ran a `CREATE2`, read after the instruction.
#[derive(Default)]
struct CreateMemory {
    opcode: u8,
    after_create2: Option<usize>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for CreateMemory {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        self.opcode = interp.bytecode.opcode();
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if self.opcode == CREATE2 && self.after_create2.is_none() {
            self.after_create2 = Some(interp.memory.len());
        }
    }
}

/// `CREATE2` with the operands given, `value` last, then `STOP`.
fn create2(salt: Option<U256>, len: Option<U256>, offset: Option<U256>) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for operand in [salt, len, offset].into_iter().flatten() {
        code = code.push_u256(operand);
    }
    code.push_u256(U256::ZERO).append(CREATE2).append(STOP).build()
}

/// `CREATE2(value = 0, offset, len, salt = 0)`.
fn create2_of(len: U256, offset: U256) -> Bytes {
    create2(Some(U256::ZERO), Some(len), Some(offset))
}

/// Runs `CONTRACT` holding `code` at `gas_limit`, and what the frame's memory held after the
/// `CREATE2`.
fn run(code: Bytes, gas_limit: u64) -> (MegaTransactionOutcome, Option<usize>) {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(CONTRACT, code);
    let mut evm = MegaEvm::new(context(db)).with_inspector(CreateMemory::default());
    let outcome = evm
        .execute_transaction(call(CALLER, CONTRACT, U256::ZERO, gas_limit))
        .expect("the transaction is valid");
    (outcome, evm.inspector().after_create2)
}

/// A gas limit with room for any creation here: 100,000,000, on top of what the created account
/// and its records cost at the byte prices in effect, and below the execution cap.
fn roomy() -> u64 {
    100_000_000 + crate::common::account_state_gas() + body_history(0)
}

fn halted_with(outcome: &MegaTransactionOutcome) -> Option<HaltReason> {
    match &outcome.result {
        ExecutionResult::Halt { reason: MegaHaltReason::Base(reason), .. } => Some(reason.clone()),
        _ => None,
    }
}

/// A `CREATE2` missing an operand halts with `StackUnderflow`: only `value`; `value` and `offset`;
/// and every operand but the salt, with a 32 KiB length. revm pops the salt last, after it sized
/// the init code, so that frame's memory is expanded before it halts, as in revm; the halt burns
/// the frame's gas either way, so the order shows in no result.
#[test]
fn test_create2_with_missing_operands_halts_with_stack_underflow() {
    let len = U256::from(32 * 1024);
    let cases = [
        ("the offset missing", create2(None, None, None), 0),
        ("the length missing", create2(None, None, Some(U256::ZERO)), 0),
        ("the salt missing", create2(None, Some(len), Some(U256::ZERO)), 32 * 1024),
    ];
    for (name, code, expanded) in cases {
        let (outcome, memory) = run(code, roomy());
        assert_eq!(halted_with(&outcome), Some(HaltReason::StackUnderflow), "{name}");
        assert_eq!(memory, Some(expanded), "{name}: the memory at the halt");
    }
}

/// An empty init code reads no offset: at an offset no `usize` holds the creation succeeds,
/// deploying the empty code at the address `KECCAK_EMPTY` derives.
#[test]
fn test_create2_len_zero_offset_max_succeeds() {
    let (outcome, memory) = run(create2_of(U256::ZERO, U256::MAX), roomy());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(memory, Some(0));
    assert!(outcome.state.contains_key(&CONTRACT.create2(B256::ZERO, KECCAK_EMPTY)));
}

/// The shortcut is for an empty init code alone: one byte at an offset no `usize` holds halts on
/// the offset.
#[test]
fn test_create2_len_nonzero_offset_max_halts() {
    let (outcome, _) = run(create2_of(U256::from(1), U256::MAX), roomy());
    assert_eq!(
        halted_with(&outcome),
        Some(HaltReason::OutOfGas(OutOfGasError::InvalidOperand)),
        "{:?}",
        outcome.result
    );
}

/// An empty init code at a large offset expands no memory and pays for none: the creation
/// succeeds, the frame's memory stays empty, and its regular gas is that of a small program.
#[test]
fn test_create2_len_zero_large_offset_skips_memory_expansion() {
    let (outcome, memory) = run(create2_of(U256::ZERO, U256::from(1u64 << 30)), roomy());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(memory, Some(0), "no memory was expanded");
    assert!(
        outcome.gas.regular < 100_000,
        "{}: no gigabyte of memory paid for",
        outcome.gas.regular
    );
}

/// An empty init code at offset zero deploys the empty code at the address `KECCAK_EMPTY`
/// derives, which is the hash of the empty slice revm would otherwise have read.
#[test]
fn test_create2_len_zero_offset_zero_succeeds() {
    assert_eq!(KECCAK_EMPTY, keccak256([]));
    let (outcome, _) = run(create2_of(U256::ZERO, U256::ZERO), roomy());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let created = CONTRACT.create2(B256::ZERO, KECCAK_EMPTY);
    assert_eq!(outcome.state[&created].info.nonce, 1, "the empty code is deployed there");
}

/// A 32 KiB init code pays for its words before the creation starts: the memory it expands
/// (`3w + w²/512`), EIP-3860's two gas a word and the hash's six. Against a one-word init code the
/// regular gas differs by exactly those; everything else, the created account and its records
/// included, is the same.
#[test]
fn test_create2_with_a_32_kib_init_code_pays_its_words() {
    let memory = |words: u64| 3 * words + words * words / 512;
    let words = 1_024;
    let expected = memory(words) - memory(1) + (2 + 6) * (words - 1);
    let small = run(create2_of(U256::from(32), U256::ZERO), roomy()).0;
    let (large, memory_size) = run(create2_of(U256::from(32 * words), U256::ZERO), roomy());
    assert!(small.result.is_success() && large.result.is_success(), "{:?}", large.result);
    assert_eq!(memory_size, Some(32 * 1_024));
    assert_eq!(large.gas.regular - small.gas.regular, expected);
    assert_eq!(large.gas.state, small.gas.state);
    assert_eq!(large.gas.history, small.gas.history);
}

/// Init code of exactly the Satin limit is created; one byte more halts with
/// `CreateInitCodeSizeLimit`.
#[test]
fn test_create2_init_code_at_the_limit_succeeds_and_one_byte_more_halts() {
    let at = run(create2_of(U256::from(MAX_INITCODE_SIZE), U256::ZERO), roomy()).0;
    assert!(at.result.is_success(), "{:?}", at.result);
    let over = run(create2_of(U256::from(MAX_INITCODE_SIZE + 1), U256::ZERO), roomy()).0;
    assert_eq!(halted_with(&over), Some(HaltReason::CreateInitCodeSizeLimit), "{:?}", over.result);
}

/// An init code over the limit halts with `CreateInitCodeSizeLimit` before memory is touched,
/// whether expanding memory to it would be affordable (just over the limit) or not (10 MiB).
#[test]
fn test_create2_oversized_init_code_halts_before_touching_memory() {
    for len in [MAX_INITCODE_SIZE as u64 + 1, 10 * 1_024 * 1_024] {
        let (outcome, memory) = run(create2_of(U256::from(len), U256::ZERO), roomy());
        assert_eq!(
            halted_with(&outcome),
            Some(HaltReason::CreateInitCodeSizeLimit),
            "{len} bytes: {:?}",
            outcome.result
        );
        assert_eq!(memory, Some(0), "{len} bytes: no memory was expanded");
    }
}

/// The size is checked before the offset is read: an oversized length at an offset no `usize`
/// holds halts with `CreateInitCodeSizeLimit`, not on the offset.
#[test]
fn test_create2_oversized_init_code_halts_before_its_offset_is_read() {
    let code = create2_of(U256::from(MAX_INITCODE_SIZE + 1), U256::from(u128::MAX));
    let (outcome, _) = run(code, roomy());
    assert_eq!(
        halted_with(&outcome),
        Some(HaltReason::CreateInitCodeSizeLimit),
        "{:?}",
        outcome.result
    );
}

/// A length within the limit whose memory the frame cannot afford halts on the memory, and the
/// transaction reports the halt: 500,000 bytes of init code on 200,000 of regular gas.
#[test]
fn test_create2_whose_init_code_memory_cannot_be_paid_halts_on_the_memory() {
    let (outcome, _) = run(create2_of(U256::from(500_000), U256::ZERO), 200_000 + body_history(0));
    assert_eq!(
        halted_with(&outcome),
        Some(HaltReason::OutOfGas(OutOfGasError::Memory)),
        "{:?}",
        outcome.result
    );
}
