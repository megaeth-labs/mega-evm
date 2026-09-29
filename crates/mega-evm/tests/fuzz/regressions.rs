//! The minimal cases the properties found, one named test each, so a finding is pinned without
//! its seed.

use crate::{
    gen::{
        case::Case,
        program::{End, Program},
        tx::{GasTier, Price, Shape, Tx},
        world::{AccountShape, Balance, BeneficiaryIs, BlockShape, Limits, Salt, World},
        Value, Who,
    },
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

/// A value transfer to an address with no account, at a gas limit that cannot cover EIP-2780's
/// charge for the new recipient, runs out of gas before its first frame and burns its whole gas
/// limit: nothing comes back from a reservoir the transaction never had, and the history ledger
/// reads the body alone.
///
/// Found by the gas-ledgers property: the fork drops the runtime phase's partial charges by
/// rebuilding the transaction's gas, and the settlement gave the recipient record's history back a
/// second time, into the reservoir, which the halt does not burn.
#[test]
fn test_an_out_of_gas_before_the_first_frame_burns_the_whole_gas_limit() {
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
    let execution = case.execute();
    let outcome = execution.as_ref().expect("the transaction is valid");
    let rendered = render(&execution);
    assert!(outcome.result.is_halt(), "{rendered}");
    let gas = outcome.result.gas();
    assert_eq!(
        gas.total_gas_spent(),
        GasTier::Tiny.limit(),
        "the whole gas limit burns\n{rendered}"
    );
    assert_eq!(gas.reservoir_remaining(), 0, "nothing comes back from a reservoir\n{rendered}");
    assert_eq!(outcome.gas.gas_used, GasTier::Tiny.limit(), "{rendered}");
    let body = mega_evm::transaction_body_bytes(&case.transaction());
    assert_eq!(outcome.gas.history_bytes, body, "the body alone is kept\n{rendered}");
    assert_eq!(
        outcome.gas.history,
        mega_evm::history_gas(body).expect("the body has a price"),
        "the history ledger reads the body\n{rendered}"
    );
    assert_eq!(outcome.usage.write_records, 0, "{rendered}");
}

/// A system deposit, which Regolith refuses, with a body over the data-size limit: op-revm
/// answers the refusal with a failed-deposit halt, and the outcome reports the halt and no stop,
/// as an out-of-gas before the first frame does; the body is what the deposit kept.
///
/// Found by the survivors property: the body's latch, set before op-revm validated the deposit,
/// was reported beside the halt.
#[test]
fn test_a_failed_deposit_reports_no_stop() {
    use crate::gen::world::{DataLimit, DetentionCap, StateLimit};
    let mut world = plain_world();
    world.limits = Limits::Custom {
        data: DataLimit::Body,
        frame_data: None,
        kv: None,
        frame_kv: None,
        state: StateLimit::Unlimited,
        block_env_cap: DetentionCap::Default,
        oracle_cap: DetentionCap::Default,
    };
    let case = Case {
        world,
        tx: Tx {
            shape: Shape::Deposit {
                to: Who::Caller,
                mint: Value::Zero,
                data_len: 1,
                system: true,
                create: false,
            },
            value: Value::Zero,
            gas: GasTier::Tiny,
            price: Price::Free,
            nonce_ok: true,
        },
        main: stop(),
        a: stop(),
        b: stop(),
    };
    let execution = case.execute();
    let outcome = execution.as_ref().expect("the deposit is included");
    let rendered = render(&execution);
    assert!(
        matches!(&outcome.result, revm::context::result::ExecutionResult::Halt { reason, .. }
            if *reason == mega_evm::MegaHaltReason::FailedDeposit),
        "{rendered}"
    );
    assert_eq!(outcome.limit_exceeded, None, "the halt is what the deposit reports\n{rendered}");
    let body = mega_evm::transaction_body_bytes(&case.transaction());
    assert_eq!(
        outcome.usage,
        mega_evm::LimitUsage { data_size: body, write_records: 0 },
        "the body is what the deposit kept\n{rendered}"
    );
    assert_eq!(outcome.gas.gas_used, GasTier::Tiny.limit(), "{rendered}");
}

/// A creation transaction that runs out of gas before its first frame, on the history of the
/// record its own frame would make, bumps its sender's nonce as revm's unwind does for a creation
/// whose runtime gas phase ran out: an included out-of-gas creation cannot be replayed.
///
/// Found by the gas-ledgers property: Satin's pre-execution bailed out with a plain checkpoint
/// revert, which took the bump the creation frame makes back and made none of its own.
#[test]
fn test_an_out_of_gas_creation_bumps_the_sender_s_nonce() {
    use crate::gen::program::{AccountRead, Forward, Op, Scheme, SystemContract, Target};
    let mut world = plain_world();
    world.caller.balance = Balance::Small;
    let case = Case {
        world,
        tx: Tx {
            shape: Shape::Create,
            value: Value::Zero,
            gas: GasTier::Tiny,
            price: Price::Free,
            nonce_ok: true,
        },
        main: Program {
            ops: vec![
                Op::Account {
                    target: Target::System(SystemContract::Oracle),
                    read: AccountRead::Balance,
                },
                Op::Call {
                    scheme: Scheme::Call,
                    target: Target::Who(Who::Caller),
                    value: Value::Zero,
                    forward: Forward::All,
                    args_len: 0,
                },
            ],
            end: End::Stop,
        },
        a: stop(),
        b: stop(),
    };
    let execution = case.execute();
    let outcome = execution.as_ref().expect("the transaction is valid");
    let rendered = render(&execution);
    assert!(outcome.result.is_halt(), "{rendered}");
    assert_eq!(outcome.result.gas().total_gas_spent(), GasTier::Tiny.limit(), "{rendered}");
    assert_eq!(
        outcome.state[&Who::Caller.address()].info.nonce,
        1,
        "the nonce moves by one\n{rendered}"
    );
}
