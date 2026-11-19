//! The legacy share: the part of a state charge regular gas pays before the reservoir.
//!
//! Four opcode sites charge state gas for state an opcode adds — a fresh slot's `SSTORE`, the
//! account a value `CALL` creates, the beneficiary a `SELFDESTRUCT` creates, the account a
//! `CREATE` or `CREATE2` creates — and at each, regular gas pays what the opcode cost before Satin
//! less what its regular entries charge now, up to the charge, and the reservoir pays the rest. So
//! each of them draws from regular gas what it drew before Satin, whatever the reservoir holds.
//!
//! No other state charge takes a share: not the account the transaction's own value or creation
//! adds, not the code a creation deposits, not an EIP-7702 authority or its delegation indicator,
//! and no history charge.
//!
//! Every case runs above the execution cap, where the reservoir could pay every charge alone, and
//! reads what regular gas paid of the state and history gas off the outcome: what they cost less
//! what the reservoir paid of them.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaTransactionOutcome,
};
use revm::bytecode::opcode::{
    CREATE, CREATE2, GAS, LOG0, MSTORE, POP, PUSH0, PUSH1, PUSH4, RETURN, SELFDESTRUCT, SSTORE,
    STOP, SUB, SWAP1,
};

use crate::common::{
    account_share, account_state_gas, authorizing_call, beneficiary_share, call, create,
    create_share, execute, runs_at_measurement_prices,
};

const CALLER: Address = address!("0000000000000000000000000000000000710000");
const CONTRACT: Address = address!("0000000000000000000000000000000000710001");
/// An address no account holds.
const NEW: Address = address!("0000000000000000000000000000000000710002");
/// An address an account holds.
const FUNDED: Address = address!("0000000000000000000000000000000000710003");
/// The account an EIP-7702 authorization delegates to.
const DELEGATE: Address = address!("0000000000000000000000000000000000710004");

/// A gas limit above the execution cap, with a reservoir that could pay every charge here alone.
const ABOVE_CAP: u64 = TX_GAS_LIMIT_CAP + 100_000_000;

fn db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CONTRACT, U256::from(1_000))
        .account_balance(FUNDED, U256::from(1))
}

/// Runs `code` as `CONTRACT`, called by the caller above the cap.
fn run(code: Bytes) -> MegaTransactionOutcome {
    let outcome =
        execute(db().account_code(CONTRACT, code), call(CALLER, CONTRACT, U256::ZERO, ABOVE_CAP));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    outcome
}

/// What regular gas paid of the state and history gas the transaction spent: what they cost less
/// what the reservoir paid of them.
fn paid_by_regular(outcome: &MegaTransactionOutcome, gas_limit: u64) -> u64 {
    let reservoir_paid = gas_limit - TX_GAS_LIMIT_CAP - outcome.gas.reservoir_remaining;
    (outcome.gas.state + outcome.gas.history)
        .checked_sub(reservoir_paid)
        .expect("the reservoir pays no more than the state and history gas")
}

/// `prefix`, then `GAS`, `op`, `GAS`, returning the gas the second `GAS` read less than the first:
/// what `op` drew from regular gas, with the second `GAS`'s 2 on top.
fn measuring(prefix: BytecodeBuilder, op: BytecodeBuilder) -> Bytes {
    prefix
        .append(GAS)
        .append_many(op.build_vec())
        .append(GAS)
        .append_many([SWAP1, SUB, PUSH0, MSTORE])
        .append_many([PUSH1, 32, PUSH0, RETURN])
        .build()
}

/// The word a run returned.
fn word(outcome: &MegaTransactionOutcome) -> u64 {
    let output = outcome.result.output().expect("an output");
    U256::from_be_slice(output).to::<u64>()
}

/// A value `CALL` that creates its target's account takes its share, 25,000 at the spec's prices,
/// from regular gas.
#[test]
fn test_a_value_call_creating_its_target_takes_its_legacy_share() {
    let code = BytecodeBuilder::default().call(NEW, U256::from(1)).append_many([POP, STOP]).build();
    let outcome = run(code);
    assert!(outcome.state.get(&NEW).is_some_and(|account| account.info.balance == U256::from(1)));
    assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), account_share());
    if !runs_at_measurement_prices() {
        assert_eq!(account_share(), 25_000);
    }
}

/// A `SELFDESTRUCT` that creates its beneficiary takes its share, 16,000 at the spec's prices,
/// from regular gas: next to one to a beneficiary that exists, it draws its 9,000 of regular gas
/// and the share, 25,000 more, 30,000 in all, which is what it drew before Satin.
#[test]
fn test_a_selfdestruct_creating_its_beneficiary_takes_its_legacy_share() {
    let destroying = |beneficiary| {
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build()
    };
    let new = run(destroying(NEW));
    let existing = run(destroying(FUNDED));
    assert!(new.state.get(&NEW).is_some_and(|account| account.info.balance == U256::from(1_000)));
    assert_eq!(paid_by_regular(&new, ABOVE_CAP), beneficiary_share());
    assert_eq!(paid_by_regular(&existing, ABOVE_CAP), 0, "an existing beneficiary adds no state");
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(beneficiary_share(), 16_000);
    let drawn = |outcome: &MegaTransactionOutcome| {
        outcome.gas.regular + paid_by_regular(outcome, ABOVE_CAP)
    };
    assert_eq!(drawn(&new) - drawn(&existing), 9_000 + 16_000, "5,000 against 30,000");
}

/// A `CREATE` and a `CREATE2` take the share of the account they create, 20,000 at the spec's
/// prices, from regular gas, and each draws 32,000 from regular gas for an empty init code, what
/// it drew before Satin.
#[test]
fn test_a_creation_opcode_takes_its_legacy_share() {
    for (opcode, zeros) in [(CREATE, 3), (CREATE2, 4)] {
        let op =
            BytecodeBuilder::default().append_many(vec![PUSH0; zeros]).append_many([opcode, POP]);
        let outcome = run(measuring(BytecodeBuilder::default(), op));
        assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), create_share(), "{opcode:#x}");
        if !runs_at_measurement_prices() {
            assert_eq!(create_share(), 20_000);
            let pushes = 2 * zeros as u64;
            assert_eq!(
                word(&outcome),
                pushes + 32_000 + 2 + 2,
                "{opcode:#x}: the pushes, the POP, the GAS"
            );
        }
    }
}

/// A `CREATE` whose init code deposits code takes the share of the account alone: the code's state
/// gas takes none.
#[test]
fn test_a_code_deposit_takes_no_legacy_share() {
    // `PUSH1 1; PUSH0; RETURN`: one byte of code, a zero.
    let code = BytecodeBuilder::default()
        .append(PUSH4)
        .append_many([PUSH1, 1, PUSH0, RETURN])
        .append_many([PUSH0, MSTORE])
        .append_many([PUSH1, 4, PUSH1, 28, PUSH0, CREATE, POP, STOP])
        .build();
    let outcome = run(code);
    let created = CONTRACT.create(0);
    assert!(outcome
        .state
        .get(&created)
        .is_some_and(|account| account.info.code_hash != revm::primitives::KECCAK_EMPTY));
    assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), create_share());
}

/// The account the transaction's own value creates takes no share: the reservoir pays it whole.
#[test]
fn test_the_transactions_own_recipient_takes_no_legacy_share() {
    let outcome = execute(db(), call(CALLER, NEW, U256::from(1), ABOVE_CAP));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(account_state_gas() == 0 || outcome.gas.state > 0, "the recipient is a new account");
    assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), 0);
}

/// The account a creation transaction creates and the code it deposits take no share.
#[test]
fn test_a_creation_transactions_account_and_code_take_no_legacy_share() {
    let init_code = Bytes::from_static(&[PUSH1, 1, PUSH0, RETURN]);
    let outcome = execute(db(), create(CALLER, init_code, ABOVE_CAP));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(account_state_gas() == 0 || outcome.gas.state > 0, "the created account and its code");
    assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), 0);
}

/// An EIP-7702 authority the authorization creates and its delegation indicator take no share.
#[test]
fn test_an_authority_and_its_delegation_indicator_take_no_legacy_share() {
    let tx = authorizing_call(CALLER, FUNDED, U256::ZERO, ABOVE_CAP, DELEGATE, &[(NEW, 0)]);
    let outcome = execute(db(), tx);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(
        outcome
            .state
            .get(&NEW)
            .is_some_and(|account| account.info.code_hash != revm::primitives::KECCAK_EMPTY),
        "the authority is delegated",
    );
    assert!(
        account_state_gas() == 0 || outcome.gas.state > 0,
        "the authority's account and its delegation indicator"
    );
    assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), 0);
}

/// History takes no share: a log and the record of a slot that already held a value are paid by
/// the reservoir alone.
#[test]
fn test_history_takes_no_legacy_share() {
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, LOG0])
        .append_many([PUSH1, 2, PUSH1, 1, SSTORE, STOP])
        .build();
    let db =
        db().account_code(CONTRACT, code).account_storage(CONTRACT, U256::from(1), U256::from(1));
    let outcome = execute(db, call(CALLER, CONTRACT, U256::ZERO, ABOVE_CAP));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "nothing new");
    assert_eq!(paid_by_regular(&outcome, ABOVE_CAP), 0);
}
