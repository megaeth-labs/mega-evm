//! What the Satin gas schedule charges, measured on real transactions.
//!
//! What init code costs as transaction data, the state gas a `CREATE` frame rejected for its
//! deployed code gives back, and the Amsterdam opcodes the schedule brings with it. Each number is
//! read off a transaction the engine ran, not computed from the schedule, so a change to either
//! side shows up here.
//!
//! Every transaction also pays the history gas of its body, which is not a schedule entry and is
//! priced by the history mechanism; the figures below take it from the transaction's own ledgers.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm,
};
use revm::{
    bytecode::opcode::{CLZ, CREATE, DUPN, EXCHANGE, MSTORE, PUSH0, PUSH1, RETURN, SLOTNUM, SWAPN},
    context::result::ExecutionResult,
};

use crate::common::{
    account_state_gas, call, call_with_data, context, create, history, slot_state_gas,
};

const CALLER: Address = address!("0000000000000000000000000000000000900000");
const CALLEE: Address = address!("0000000000000000000000000000000000900001");

/// 1,000,000 of regular gas on top of what the transactions here add at the byte prices in effect:
/// a new account and a fresh slot, a kilobyte of history, and 50 times the history of two records,
/// which a `CREATE` that forwards all but 2 per cent of its gas pays for the frame it starts from
/// the fiftieth it keeps.
fn gas_limit() -> u64 {
    1_000_000 +
        account_state_gas() +
        slot_state_gas() +
        history(1_000) +
        50 * history(2 * mega_evm::WRITE_RECORD_SIZE)
}

/// The gas a transaction used, the state gas inside it, the history gas the schedule does not
/// price, and the EIP-7623 floor the receipt is held to.
struct Spend {
    gas_used: u64,
    state: u64,
    history: u64,
    floor: u64,
}

/// Runs `tx` on `db` and returns what it spent. The transaction must succeed.
fn spend(db: MemoryDatabase, tx: mega_evm::MegaTransaction) -> Spend {
    let outcome =
        MegaEvm::new(context(db)).execute_transaction(tx).expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    Spend {
        gas_used: outcome.gas.gas_used,
        state: outcome.gas.state,
        history: outcome.gas.history,
        floor: outcome.result.gas().floor_gas(),
    }
}

/// A database where `CALLEE` exists and runs `code`.
fn with_code(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(CALLEE, code)
}

/* ---------- the intrinsic numbers ---------- */

/// Init code is transaction data twice over: every byte is a calldata token, and EIP-3860 charges
/// two gas for each 32-byte word of it. Sixty-four bytes of zeros is two words; sixty-five is
/// three, so the word charge steps by two where the token charge steps by four.
///
/// The receipt is held to the EIP-7623 floor, which the body's history lifts the bill past at the
/// spec's prices; where a history byte is cheap enough, the floor is the bill.
#[test]
fn test_init_code_costs_its_calldata_tokens_and_an_eip3860_word() {
    let fixed = 24_000;
    let token = 4;
    let word = 2;
    for (len, words) in [(64u64, 2u64), (65, 3)] {
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
        // Every byte is `STOP`, so the init code deposits nothing and costs nothing to run.
        let spent = spend(db, create(CALLER, Bytes::from(vec![0u8; len as usize]), gas_limit()));
        let charged = fixed + token * len + word * words;
        assert_eq!(
            spent.gas_used,
            (charged + spent.state + spent.history).max(spent.floor),
            "{len} bytes of init code"
        );
    }
}

/* ---------- the state entries ---------- */

/// Init code that returns a single `0xEF` byte as the deployed code, which EIP-3541 rejects.
fn depositing_an_ef_byte() -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, [0xEF])
        .push_number(1u64)
        .append_many([PUSH0, RETURN])
        .build()
}

/// A `CREATE` frame whose deployed code starts with `0xEF` fails and returns the zero address, the
/// transaction around it goes through, and the state gas the frame took upfront is back.
#[test]
fn test_a_create_frame_rejected_for_its_first_byte_draws_no_state_gas() {
    let init_code = depositing_an_ef_byte();
    // `CREATE` pops value, offset and length in that order, so they are pushed the other way
    // round; the assertion that follows halts the frame unless the created address is zero.
    let factory = BytecodeBuilder::default()
        .mstore(0, &init_code)
        .push_number(init_code.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .assert_stack_value(0, U256::ZERO)
        .stop()
        .build();
    let spent = spend(with_code(factory), call(CALLER, CALLEE, U256::ZERO, gas_limit()));

    assert_eq!(spent.state, 0, "the created account and its byte are both taken back");
}

/* ---------- the Amsterdam opcodes ---------- */

/// Runs `code` in `CALLEE` and returns the result, so a gated opcode shows up as a halt.
fn run_code(code: Vec<u8>) -> ExecutionResult<mega_evm::MegaHaltReason> {
    let mut evm = MegaEvm::new(context(with_code(Bytes::from(code))));
    evm.transact_raw(call(CALLER, CALLEE, U256::ZERO, gas_limit()))
        .expect("the transaction is valid")
        .result
}

/// `DUPN`, `SWAPN`, `EXCHANGE` and `SLOTNUM` are gated on the Amsterdam spec upstream; the Satin
/// table activates them on the Karst base, so each one runs instead of halting.
#[test]
fn test_the_amsterdam_opcodes_run() {
    // Two pushes and fifteen `DUP1`s, then `DUPN 0x80`: a stack deep enough for the immediate.
    let mut deep = vec![PUSH1, 0x01, PUSH1, 0x00];
    deep.extend([0x80u8; 15]); // DUP1
    for opcode in [
        vec![SLOTNUM, PUSH0, MSTORE],
        [deep.clone(), vec![DUPN, 0x80]].concat(),
        [deep.clone(), vec![PUSH1, 0x02, SWAPN, 0x80]].concat(),
        vec![PUSH1, 0x00, PUSH1, 0x01, PUSH1, 0x02, EXCHANGE, 0x8E],
    ] {
        let result = run_code(opcode.clone());
        assert!(result.is_success(), "{:#04x} halted: {result:?}", opcode[opcode.len() - 2]);
    }
}

/// `CLZ` is gated on Osaka, which the Karst base already is, so it needs no activation; it is
/// listed here so the whole Amsterdam opcode set is covered in one place.
#[test]
fn test_clz_runs_on_the_base_spec() {
    let result = run_code(vec![PUSH1, 0x01, CLZ, PUSH0, MSTORE]);
    assert!(result.is_success(), "{result:?}");
}

/// `SLOTNUM` pushes zero, whatever slot number the block environment carries: the value is the
/// engine's own and depends on no field the node supplies.
#[test]
fn test_slotnum_pushes_zero_whatever_the_block_carries() {
    let code = BytecodeBuilder::default().append(SLOTNUM).return_top().build();
    for (slot_num, name) in [(0x0123_4567_89ab_u64, "a slot the node set"), (0, "no slot set")] {
        let block = revm::context::BlockEnv { slot_num, ..crate::common::block() };
        let mut evm = MegaEvm::new(context(with_code(code.clone())).with_block(block));
        let result = evm
            .transact_raw(call_with_data(CALLER, CALLEE, Bytes::new(), gas_limit()))
            .expect("the transaction is valid")
            .result;
        assert!(result.is_success(), "{name}: {result:?}");
        assert_eq!(
            result.output().map(|output| U256::from_be_slice(output)),
            Some(U256::ZERO),
            "{name}",
        );
    }
}
