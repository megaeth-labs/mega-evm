//! What the Satin gas schedule charges, measured on real transactions.
//!
//! The intrinsic numbers, the state gas a new slot and a new account draw, and the Amsterdam
//! opcodes the schedule brings with it. Each number is read off a transaction the engine ran, not
//! computed from the schedule, so a change to either side shows up here.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{ACCOUNT_STATE_GAS, SLOT_STATE_GAS},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaHaltReason,
};
use revm::{
    bytecode::opcode::{CLZ, CREATE, DUPN, EXCHANGE, MSTORE, PUSH0, PUSH1, RETURN, SLOTNUM, SWAPN},
    context::result::{ExecutionResult, HaltReason},
};

use crate::common::{call, call_with_data, context, create, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000900000");
const CALLEE: Address = address!("0000000000000000000000000000000000900001");

const GAS_LIMIT: u64 = 1_000_000;

/// The gas a transaction used and the state gas inside it.
struct Spend {
    gas_used: u64,
    state: u64,
}

/// Runs `tx` on `db` and returns what it spent. The transaction must succeed.
fn spend(db: MemoryDatabase, tx: mega_evm::MegaTransaction) -> Spend {
    let mut evm = MegaEvm::new(context(db));
    let result = evm.transact_raw(tx).expect("the transaction is valid").result;
    assert!(result.is_success(), "{result:?}");
    Spend { gas_used: result.gas().tx_gas_used(), state: result.gas().state_gas_spent_final() }
}

/// A database where `CALLEE` exists and runs `code`.
fn with_code(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(CALLEE, code)
}

/* ---------- the intrinsic numbers ---------- */

/// A call that reaches an existing account and does nothing pays the EIP-2780 sender base of
/// 12,000 plus 3,000 for reaching the recipient, and draws no state gas.
///
/// The 3,000 is EIP-2780's own fixed charge and not the schedule's cold-account entry, which
/// prices an access inside execution at 2,600.
#[test]
fn test_an_empty_call_costs_fifteen_thousand() {
    let spent = spend(with_code(Bytes::new()), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert_eq!(spent.gas_used, 15_000);
    assert_eq!(spent.state, 0);
}

/// A value transfer to an account that already exists adds the value charge, and still draws no
/// state gas: nothing is created.
#[test]
fn test_a_value_transfer_to_an_existing_account_costs_twenty_one_thousand() {
    let db = with_code(Bytes::new()).account_balance(CALLEE, U256::from(1));
    let spent = spend(db, call(CALLER, CALLEE, U256::from(7), GAS_LIMIT));
    assert_eq!(spent.gas_used, 21_000);
    assert_eq!(spent.state, 0);
}

/// A transfer to an account that does not exist creates it, which is the account's state gas on
/// top of the transfer's own cost.
#[test]
fn test_a_value_transfer_that_creates_the_recipient_draws_the_account_state_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let spent = spend(db, call(CALLER, CALLEE, U256::from(7), GAS_LIMIT));
    assert_eq!(spent.state, ACCOUNT_STATE_GAS);
    assert_eq!(spent.gas_used, 21_000 + ACCOUNT_STATE_GAS);
}

/// A transfer to the sender itself pays the sender base alone: neither the recipient charge nor
/// the value charge applies, because the recipient is the sender.
#[test]
fn test_a_self_transfer_costs_twelve_thousand() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let spent = spend(db, call(CALLER, CALLER, U256::from(7), GAS_LIMIT));
    assert_eq!(spent.gas_used, 12_000);
    assert_eq!(spent.state, 0);
}

/// A creation transaction pays the sender base plus the creation access charge, and the created
/// account's state gas on top. With empty init code nothing else is charged, so the fixed part is
/// what is left when the state gas is taken out.
#[test]
fn test_a_create_transaction_costs_twenty_four_thousand_plus_the_account_state_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let spent = spend(db, create(CALLER, Bytes::new(), GAS_LIMIT));
    assert_eq!(spent.state, ACCOUNT_STATE_GAS, "the created account");
    assert_eq!(spent.gas_used - spent.state, 24_000, "the fixed part");
}

/* ---------- the state entries ---------- */

/// A slot's first write to a non-zero value draws the slot's state gas; writing it again does
/// not, because the slot is no longer new.
#[test]
fn test_a_new_slot_draws_the_slot_state_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let new = spend(with_code(code.clone()), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert_eq!(new.state, SLOT_STATE_GAS);

    let db = with_code(code).account_storage(CALLEE, U256::ZERO, U256::from(1));
    let again = spend(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert_eq!(again.state, 0, "the slot already holds the value");
}

/// Deployed code is priced by the byte on the state dimension, on top of the created account.
#[test]
fn test_deployed_code_draws_state_gas_by_the_byte() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let empty = spend(db(), create(CALLER, Bytes::new(), GAS_LIMIT));
    let thirty_two = spend(db(), create(CALLER, deploying(32), GAS_LIMIT));

    assert_eq!(empty.state, ACCOUNT_STATE_GAS);
    assert_eq!(thirty_two.state - empty.state, 32 * 1_530, "32 bytes at the state byte price");
}

/// Init code that returns `len` bytes of zeros as the deployed code.
fn deploying(len: u64) -> Bytes {
    BytecodeBuilder::default().push_number(len).append_many([PUSH0, RETURN]).build()
}

/// A creation that reverts deposits nothing, so it draws no state gas at all: neither the
/// created account's nor the code's. The state gas a creation draws is charged on the deposit,
/// not on the attempt.
#[test]
fn test_a_reverted_creation_draws_no_state_gas() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let mut evm = MegaEvm::new(context(db));
    let init_code = BytecodeBuilder::default().mstore(0, [0u8; 32]).revert().build();
    let result = evm
        .transact_raw(create(CALLER, init_code, GAS_LIMIT))
        .expect("the transaction is valid")
        .result;

    assert!(!result.is_success(), "{result:?}");
    assert_eq!(result.gas().state_gas_spent_final(), 0);
}

/// Init code that returns a single `0xEF` byte as the deployed code, which EIP-3541 rejects.
fn depositing_an_ef_byte() -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, [0xEF])
        .push_number(1u64)
        .append_many([PUSH0, RETURN])
        .build()
}

/// EIP-3541 rejects deployed code whose first byte is `0xEF`, and the rejection lands before the
/// deposit is charged: the creation halts and draws no state gas, neither the account's nor the
/// byte's.
#[test]
fn test_a_creation_rejected_for_its_first_byte_draws_no_state_gas() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let mut evm = MegaEvm::new(context(db));
    let result = evm
        .transact_raw(create(CALLER, depositing_an_ef_byte(), GAS_LIMIT))
        .expect("the transaction is valid")
        .result;

    assert!(
        matches!(
            result,
            ExecutionResult::Halt {
                reason: MegaHaltReason::Base(HaltReason::CreateContractStartingWithEF),
                ..
            }
        ),
        "{result:?}"
    );
    assert_eq!(result.gas().state_gas_spent_final(), 0);
}

/// The same from a `CREATE` frame: the frame fails and returns the zero address, the transaction
/// around it goes through, and the state gas the frame took upfront is back.
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
    let spent = spend(with_code(factory), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));

    assert_eq!(spent.state, 0, "the created account and its byte are both taken back");
}

/// A creation that deposits nothing — empty runtime code — still pays for the account it
/// created, and nothing for code.
#[test]
fn test_a_creation_depositing_no_code_pays_for_the_account_alone() {
    if runs_at_measurement_prices() {
        return;
    }
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let spent = spend(db, create(CALLER, deploying(0), GAS_LIMIT));
    assert_eq!(spent.state, ACCOUNT_STATE_GAS);
}

/* ---------- the Amsterdam opcodes ---------- */

/// Runs `code` in `CALLEE` and returns the result, so a gated opcode shows up as a halt.
fn run_code(code: Vec<u8>) -> ExecutionResult<mega_evm::MegaHaltReason> {
    let mut evm = MegaEvm::new(context(with_code(Bytes::from(code))));
    evm.transact_raw(call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
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

/// `SLOTNUM` pushes what the host reports, which is zero until the block executor sets it.
#[test]
fn test_slotnum_pushes_the_host_slot_number() {
    let code = BytecodeBuilder::default()
        .append(SLOTNUM)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let mut evm = MegaEvm::new(context(with_code(code)));
    let result = evm
        .transact_raw(call_with_data(CALLER, CALLEE, Bytes::new(), GAS_LIMIT))
        .expect("the transaction is valid")
        .result;
    assert!(result.is_success(), "{result:?}");
    assert_eq!(result.output().map(Bytes::as_ref), Some([0u8; 32].as_slice()));
}
