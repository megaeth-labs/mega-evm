//! The minimal cases the properties found, and the ones a review of them found, one named test
//! each, so a finding is pinned without its seed.
//!
//! Four findings first pinned here now live as deterministic tests:
//!
//! - `test_an_out_of_gas_before_the_first_frame_matches_op_revm` pins the out-of-gas before the
//!   first frame. Found by the gas-ledgers property: the fork drops the runtime phase's partial
//!   charges by rebuilding the transaction's gas, and the settlement gave the recipient record's
//!   history back a second time, into the reservoir, which the halt does not burn.
//! - `test_a_system_deposit_over_the_limit_reports_the_failed_deposit` pins a failed deposit that
//!   reports no stop. Found by the survivors property: the stop the body's count set, before
//!   op-revm validated the deposit, was reported beside the halt.
//! - `test_an_out_of_gas_creation_before_the_first_frame_matches_op_revm` pins the nonce bump of an
//!   out-of-gas creation. Found by the gas-ledgers property: Satin's pre-execution bailed out with
//!   a plain checkpoint revert, which took the bump the creation frame makes back and made none of
//!   its own.
//! - `test_a_failed_deposit_keeps_the_hints_it_forwarded` pins a failed deposit that keeps a
//!   forwarded hint. Found by comparing every case's outcome before and after the failed deposit's
//!   settlement was added: that settlement reset the whole count to the body, the forwarded hint
//!   with it.
//!
//! A case whose gas limit falls short of a charge is pinned at the spec's byte prices, where it
//! was found: its gas limit is the one the minimal case carries, and the charge moves with the
//! prices. At other prices those tests return early, with the note the byte-price grid counts; the
//! properties themselves hold at any price and run there.

use crate::{
    gen::{
        case::Case,
        program::{End, Forward, InitCode, Op, Program, Scheme, Target},
        tx::{GasTier, Price, Shape, Tx},
        world::{AccountShape, Balance, BeneficiaryIs, BlockShape, Limits, Salt, World},
        Value, Who,
    },
    harness::{history_rounds, runs_at_measurement_prices},
    render::render,
};

/// A world of empty programs, with the sender funded and nothing else.
fn plain_world() -> World {
    let account = |balance| AccountShape { balance, nonce: 0, slots: vec![] };
    World {
        caller: account(Balance::Rich),
        contract: account(Balance::Zero),
        a: account(Balance::Zero),
        b: account(Balance::Zero),
        beneficiary_balance: Balance::Zero,
        beneficiary: BeneficiaryIs::Distinct,
        delegation: None,
        salt: Salt {
            default_multiplier: 1,
            crowded_accounts: vec![],
            crowded_slots: vec![],
            failing_account: None,
        },
        oracle: vec![],
        limits: Limits::Default,
        block: BlockShape { basefee: 0, slot_num: 0, blob: true },
        system_nonce: 0,
    }
}

fn stop() -> Program {
    Program { ops: vec![], end: End::Stop }
}

/// A value transfer that runs out of gas before its first frame, with a non-zero operator fee
/// scalar and constant, pays that fee on the whole gas limit.
///
/// The fee is `gas × scalar × 100 + constant` on the gas the receipt counts, less any reservoir.
/// Handing the first frame's record history back a second time leaves that gas in the reservoir,
/// and the fee is then computed on less than the gas the halt burned. An empty envelope pays no
/// operator fee whatever the parameters, so the transaction carries one byte that is not a deposit.
/// The recipient account is created empty: the frame never runs, so the value does not move.
#[test]
fn test_an_out_of_gas_before_the_first_frame_pays_the_operator_fee_on_the_whole_gas_limit() {
    if runs_at_measurement_prices() {
        return;
    }
    use alloy_primitives::{Bytes, U256};
    use op_revm::constants::{OPERATOR_FEE_JOVIAN_MULTIPLIER, OPERATOR_FEE_RECIPIENT};

    const SCALAR: u64 = 2;
    const CONSTANT: u64 = 50;

    let case = Case {
        world: plain_world(),
        tx: Tx {
            shape: Shape::Call { to: Who::Fresh, data_len: 0, access_list: vec![] },
            value: Value::One,
            gas: GasTier::Tiny,
            price: Price::Free,
            nonce_ok: true,
        },
        main: stop(),
        a: stop(),
        b: stop(),
    };
    let mut evm = case.evm();
    evm.ctx_mut().modify_chain(|chain| {
        // A block number the info does not name makes op-revm load the info from the database,
        // which has no scalars, and the parameters above would not be the ones the fee uses.
        chain.l2_block = Some(case.block().number);
        chain.operator_fee_scalar = Some(U256::from(SCALAR));
        chain.operator_fee_constant = Some(U256::from(CONSTANT));
    });
    let mut tx = case.transaction();
    tx.0.enveloped_tx = Some(Bytes::from_static(&[0x01]));

    let execution = Case::execute_on(&mut evm, tx);
    let outcome = execution.as_ref().expect("the transaction is valid");
    let rendered = render(&execution);
    assert!(outcome.result.is_halt(), "{rendered}");
    let gas = outcome.result.gas();
    let gas_limit = GasTier::Tiny.limit();
    assert_eq!(gas.total_gas_spent(), gas_limit, "the whole gas limit burns\n{rendered}");
    assert_eq!(
        outcome.gas.reservoir_remaining, 0,
        "nothing comes back from a reservoir\n{rendered}"
    );
    assert_eq!(outcome.gas.gas_used, gas_limit, "{rendered}");
    let body = mega_evm::transaction_body_bytes(&case.transaction());
    assert_eq!(outcome.gas.history_bytes, body, "the body alone is kept\n{rendered}");
    assert_eq!(
        outcome.gas.history,
        mega_evm::history_gas(body).expect("the body has a price"),
        "the history ledger reads the body\n{rendered}"
    );
    assert_eq!(outcome.usage.write_records, 0, "{rendered}");

    let fee =
        U256::from(gas_limit) * U256::from(SCALAR) * U256::from(OPERATOR_FEE_JOVIAN_MULTIPLIER) +
            U256::from(CONSTANT);
    let caller = case.tx.sender();
    assert_eq!(
        outcome.state[&caller].info.balance,
        case.world.caller.balance.wei() - fee,
        "the sender pays the operator fee on the whole gas limit, and the value does not move\n{rendered}"
    );
    assert_eq!(
        outcome.state[&OPERATOR_FEE_RECIPIENT].info.balance, fee,
        "the operator fee recipient is paid that fee\n{rendered}"
    );
    let fresh = outcome.state.get(&Who::Fresh.address()).expect("the recipient account is created");
    assert_eq!(
        fresh.info.balance,
        U256::ZERO,
        "the recipient is created empty: the frame never runs, so the value does not move\n{rendered}"
    );
}

/// A program that funds the account its `CREATE2` lands on, named by [`Target::Created`], then
/// creates it, keeps one record for the account and one for itself: the transfer changed both, so
/// neither the creation nor the bump of the program's nonce adds a record. The creation lands on
/// the funded address, which pins that the target resolves to the address the op creates at.
///
/// Before the created account followed the net-change rule, the creation recorded it a second
/// time: three records where two stand, and a record's history more.
#[test]
fn test_a_program_that_funds_the_account_it_then_creates_keeps_one_record_for_it() {
    use alloy_primitives::{keccak256, B256, U256};
    use mega_evm::{LimitUsage, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE};

    const SALT: u8 = 2;
    let main = Program {
        ops: vec![
            Op::Call {
                scheme: Scheme::Call,
                target: Target::Created { nth: 0 },
                value: Value::One,
                forward: Forward::All,
                args_len: 0,
            },
            Op::Create { value: Value::Zero, salt: Some(SALT), init: InitCode::Empty },
        ],
        end: End::Stop,
    };
    let mut world = plain_world();
    world.contract.balance = Balance::Small;
    let case = Case {
        world,
        tx: Tx {
            shape: Shape::Call { to: Who::Contract, data_len: 0, access_list: vec![] },
            value: Value::Zero,
            gas: GasTier::Large,
            price: Price::Free,
            nonce_ok: true,
        },
        main,
        a: stop(),
        b: stop(),
    };
    let execution = case.execute();
    let outcome = execution.as_ref().expect("the transaction is valid");
    let rendered = render(&execution);
    assert!(outcome.result.is_success(), "{rendered}");

    let created = Who::Contract
        .address()
        .create2(B256::from(U256::from(SALT)), keccak256(InitCode::Empty.assemble()));
    let account = outcome.state.get(&created).expect("the account is created");
    assert_eq!(
        (account.info.nonce, account.info.balance),
        (1, U256::from(1)),
        "the creation landed on the account the program funded\n{rendered}"
    );
    let body = mega_evm::transaction_body_bytes(&case.transaction());
    assert_eq!(
        outcome.usage,
        LimitUsage {
            data_size: body + 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE,
            write_records: 2,
        },
        "the contract and the account it funded and created, and the transfer log\n{rendered}"
    );
    if !history_rounds() {
        assert_eq!(
            outcome.gas.history,
            mega_evm::history_gas(body + 2 * WRITE_RECORD_SIZE).expect("the bytes have a price"),
            "the history of the body and two records: the creation's charge for the account came \
             back\n{rendered}"
        );
    }
}
