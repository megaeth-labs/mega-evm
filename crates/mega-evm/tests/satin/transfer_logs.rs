//! EIP-7708 transfer logs: what the data size counts them at, where, and where they stop.
//!
//! Every value movement emits a `Transfer(from, to, amount)` log from `0xff…fe`, which revm
//! journals itself — no `LOG` opcode runs — and the receipt carries. Satin counts it by the rule a
//! `LOG3` carrying one word is counted, 160 bytes, on the lane of the frame whose journal
//! checkpoint holds the move, and charges it nothing:
//!
//! - a frame start that moves value — the transaction's own value, a value `CALL`, a creation's
//!   endowment — counts its log with the frame's records, before revm builds the frame, so a
//!   crossing answers the frame with the stop before any value moves;
//! - a `SELFDESTRUCT` that moves a balance counts its log with the beneficiary's record, once the
//!   opcode completed, and a crossing stops the destructing frame, which takes the move back.
//!
//! Every site runs below the execution cap and above it, and is pinned four ways: the exact bytes
//! it adds and the log the receipt carries; a frame budget one byte short of them, which stops the
//! frame that moves the value and lets its caller resume; the transaction's limit one byte short,
//! which stops the transaction; and a failure above the move, which takes the log and its bytes
//! back. Each stop is the one a `LOG3` of one word makes in the same place, where the same bytes
//! are counted: its twin below.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, keccak256, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    history_gas,
    system::{
        IMegaAccessControl, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{is_transfer_log, op_transaction, transfer_log, BytecodeBuilder, MemoryDatabase},
    transaction_body_bytes, EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaEvm,
    MegaLimitExceeded, MegaTransaction, MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{ADDRESS, CALL, GAS, POP, PUSH0, PUSH1, REVERT, SELFDESTRUCT, STOP},
    context::TxEnv,
};

use crate::common::context;

const CALLER: Address = address!("0000000000000000000000000000000000d00000");
/// The contract a nested site's transaction calls: it calls [`ACTOR`] and returns what that
/// returned.
const RELAY: Address = address!("0000000000000000000000000000000000d00001");
/// The contract that moves the value, or starts the frame that does.
const ACTOR: Address = address!("0000000000000000000000000000000000d00002");
/// A contract whose code stops.
const RECEIVER: Address = address!("0000000000000000000000000000000000d00003");
/// An account with no code.
const PAYEE: Address = address!("0000000000000000000000000000000000d00004");
/// A contract that destructs to [`RECEIVER`], holding [`VALUE`].
const DESTRUCTOR: Address = address!("0000000000000000000000000000000000d00005");
/// A contract that reverts.
const REVERTER: Address = address!("0000000000000000000000000000000000d00006");
/// The twin of a moving frame: a contract that writes slots and then emits a `LOG3` of one word.
const TWIN: Address = address!("0000000000000000000000000000000000d00007");
/// The identity precompile.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");

/// What every site moves.
const VALUE: u64 = 1_000;

const GAS_LIMITS: [u64; 2] = [20_000_000, TX_GAS_LIMIT_CAP + 100_000_000];

/// Where a value movement happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Site {
    /// The transaction's own value, to a contract.
    TxValue,
    /// A creation transaction's endowment.
    TxEndowment,
    /// A value `CALL` to a contract, which revm builds a frame for.
    Call,
    /// A value `CALL` to an account with no code, which revm answers without running.
    CallWithoutCode,
    /// A value `CALL` to a precompile.
    CallToPrecompile,
    /// A `CREATE`'s endowment.
    Create,
    /// A `CREATE2`'s endowment.
    Create2,
    /// A `SELFDESTRUCT` moving its account's balance to another account.
    SelfDestruct,
}

/// How a site's program runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    /// The move, as it is.
    Plain,
    /// The same bytes on the same lane with no value moved, the last of them a `LOG3` of one word
    /// where the transfer log is.
    Twin,
    /// The move, then a failure above it that takes it back.
    TakenBack,
}

impl Site {
    const ALL: [Self; 8] = [
        Self::TxValue,
        Self::TxEndowment,
        Self::Call,
        Self::CallWithoutCode,
        Self::CallToPrecompile,
        Self::Create,
        Self::Create2,
        Self::SelfDestruct,
    ];

    /// The depth of the frame whose lane holds the log: the transaction's own, or the one the
    /// actor starts.
    const fn depth(self) -> u32 {
        match self {
            Self::TxValue | Self::TxEndowment => 0,
            _ => 2,
        }
    }

    /// The write records the move makes: the recipient's or the created account's, and the
    /// caller's own below the transaction's frame; a destruction's beneficiary alone.
    const fn records(self) -> u64 {
        match self {
            Self::TxValue | Self::TxEndowment | Self::SelfDestruct => 1,
            _ => 2,
        }
    }

    /// The bytes the move counts on its frame's lane: its records and its transfer log.
    const fn bytes(self) -> u64 {
        self.records() * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE
    }

    /// What a stop at the move leaves counted beside the body: the nonce record a creation
    /// stopped at its start leaves its creator; nothing otherwise.
    const fn left_by_a_stop(self) -> LimitUsage {
        match self {
            Self::Create | Self::Create2 => {
                LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1 }
            }
            _ => LimitUsage::ZERO,
        }
    }

    /// The account the value moves to. The sender and the actor start at nonce zero.
    fn recipient(self) -> Address {
        match self {
            Self::TxValue | Self::Call | Self::SelfDestruct => RECEIVER,
            Self::TxEndowment => CALLER.create(0),
            Self::CallWithoutCode => PAYEE,
            Self::CallToPrecompile => IDENTITY,
            Self::Create => ACTOR.create(0),
            Self::Create2 => ACTOR.create2(B256::ZERO, keccak256([])),
        }
    }

    /// The account the value moves from.
    const fn sender(self) -> Address {
        match self {
            Self::TxValue | Self::TxEndowment => CALLER,
            Self::SelfDestruct => DESTRUCTOR,
            _ => ACTOR,
        }
    }

    /// What the actor does: the move, or its twin.
    fn actor(self, variant: Variant) -> BytecodeBuilder {
        let code = BytecodeBuilder::default();
        let value = U256::from(VALUE);
        let twin_init = BytecodeBuilder::default().log3_word().stop().build();
        match (self, variant) {
            (
                Self::Call | Self::CallWithoutCode | Self::CallToPrecompile | Self::SelfDestruct,
                Variant::Twin,
            ) => code.call(TWIN, U256::ZERO),
            (Self::Call, _) => code.call(RECEIVER, value),
            (Self::CallWithoutCode, _) => code.call(PAYEE, value),
            (Self::CallToPrecompile, _) => code.call(IDENTITY, value),
            (Self::Create, Variant::Twin) => code.create(U256::ZERO, twin_init),
            (Self::Create, _) => code.create(value, []),
            (Self::Create2, Variant::Twin) => code.create2(U256::ZERO, twin_init, U256::ZERO),
            (Self::Create2, _) => code.create2(value, [], U256::ZERO),
            (Self::SelfDestruct, _) => code.call(DESTRUCTOR, U256::ZERO),
            (Self::TxValue | Self::TxEndowment, _) => unreachable!("the transaction moves it"),
        }
    }

    /// The database and the transaction of `variant`, at `gas_limit`.
    fn program(self, variant: Variant, gas_limit: u64) -> (MemoryDatabase, MegaTransaction) {
        // The twin a call reaches writes a slot for each record the move makes, then logs; a
        // creation's twin makes the move's records by creating, and its init code logs.
        let mut twin = BytecodeBuilder::default();
        for slot in 0..self.records() {
            twin = twin.sstore(U256::from(slot + 1), U256::from(1));
        }
        let db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_balance(ACTOR, U256::from(10 * VALUE))
            .account_balance(DESTRUCTOR, U256::from(VALUE))
            .account_code(RECEIVER, Bytes::from_static(&[STOP]))
            .account_code(REVERTER, Bytes::from_static(&[PUSH0, PUSH0, REVERT]))
            .account_code(DESTRUCTOR, BytecodeBuilder::default().selfdestruct(RECEIVER).build())
            .account_code(TWIN, twin.log3_word().stop().build());
        let tx = |kind, value: u64, data: Bytes| {
            OpTx(op_transaction(TxEnv {
                caller: CALLER,
                kind,
                value: U256::from(value),
                data,
                gas_limit,
                ..Default::default()
            }))
        };
        match (self, variant) {
            (Self::TxValue, Variant::Plain) => {
                (db, tx(TxKind::Call(RECEIVER), VALUE, Bytes::new()))
            }
            (Self::TxValue, Variant::Twin) => (db, tx(TxKind::Call(TWIN), 0, Bytes::new())),
            (Self::TxValue, Variant::TakenBack) => {
                (db, tx(TxKind::Call(REVERTER), VALUE, Bytes::new()))
            }
            (Self::TxEndowment, Variant::Plain) => (db, tx(TxKind::Create, VALUE, Bytes::new())),
            (Self::TxEndowment, Variant::Twin) => {
                (db, tx(TxKind::Create, 0, BytecodeBuilder::default().log3_word().stop().build()))
            }
            (Self::TxEndowment, Variant::TakenBack) => {
                (db, tx(TxKind::Create, VALUE, Bytes::from_static(&[PUSH0, PUSH0, REVERT])))
            }
            (_, variant) => {
                let actor = self.actor(variant).append(POP);
                let actor = match variant {
                    Variant::TakenBack => actor.revert_with_returndata(),
                    Variant::Plain | Variant::Twin => actor.return_returndata(),
                };
                let relay = BytecodeBuilder::default()
                    .call(ACTOR, U256::ZERO)
                    .append(POP)
                    .return_returndata()
                    .build();
                let db = db.account_code(ACTOR, actor.build()).account_code(RELAY, relay);
                (db, tx(TxKind::Call(RELAY), 0, Bytes::new()))
            }
        }
    }
}

/// 98% of `remaining`, the share a child frame is given.
fn share(remaining: u64) -> u64 {
    (u128::from(remaining) * u128::from(FRAME_DATA_SHARE_NUMERATOR) /
        u128::from(FRAME_DATA_SHARE_DENOMINATOR)) as u64
}

/// The budget of a frame at `depth` under a frame cap of `cap`, when no frame above it has kept
/// anything yet.
fn budget(depth: u32, cap: u64) -> u64 {
    (0..depth).fold(cap, |budget, _| share(budget))
}

/// The smallest frame cap under which the frame at `depth` has exactly `bytes` of budget.
fn cap_for(depth: u32, bytes: u64) -> u64 {
    let cap = (bytes..).find(|cap| budget(depth, *cap) >= bytes).unwrap();
    assert_eq!(budget(depth, cap), bytes, "a cap that gives exactly the bytes");
    assert_eq!(budget(depth, cap - 1), bytes - 1, "and one byte less under one less");
    cap
}

fn execute(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(tx)
        .expect("the transaction is valid")
}

/// The transfer logs the receipt carries.
fn transfer_logs(outcome: &MegaTransactionOutcome) -> Vec<alloy_primitives::Log> {
    outcome.result.logs().iter().filter(|log| is_transfer_log(log)).cloned().collect()
}

fn balance(outcome: &MegaTransactionOutcome, address: Address) -> U256 {
    outcome.state.get(&address).map(|account| account.info.balance).unwrap_or_default()
}

/// Asserts what the reservoir of a run above the execution cap says: the state and history it
/// spent came out of it, and nothing else did.
fn assert_reservoir_paid(case: &str, gas_limit: u64, outcome: &MegaTransactionOutcome) {
    if gas_limit <= TX_GAS_LIMIT_CAP {
        return;
    }
    let reservoir = gas_limit - TX_GAS_LIMIT_CAP;
    assert_eq!(
        outcome.gas.reservoir_remaining,
        reservoir - outcome.gas.state - outcome.gas.history,
        "{case}: the reservoir paid the state and history ledgers and nothing else",
    );
}

/// Asserts `outcome` is `site` stopped at its move by `stop`, with nothing of the move left: no
/// transfer log, no value moved, and only what a stop leaves beside the body counted.
fn assert_stopped_at_the_move(
    case: &str,
    site: Site,
    outcome: &MegaTransactionOutcome,
    body: u64,
    stop: MegaLimitExceeded,
) {
    assert!(!outcome.result.is_halt(), "{case}: a stop is a revert: {:?}", outcome.result);
    assert_eq!(
        outcome.result.output(),
        Some(&Bytes::from(stop.abi_encode())),
        "{case}: the stop is what the transaction reports",
    );
    assert!(transfer_logs(outcome).is_empty(), "{case}: no transfer log");
    assert_eq!(balance(outcome, site.recipient()), U256::ZERO, "{case}: no value moved");
    let left = site.left_by_a_stop();
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: body + left.data_size, write_records: left.write_records },
        "{case}: the move's bytes are not counted",
    );
}

/// Each site adds exactly its records and one transfer log to the data size, and the receipt
/// carries the log: `Transfer(from, to, amount)` from `0xff…fe`. The history the transaction
/// pays, and the history bytes it reports, are its body and its records alone.
#[test]
fn test_each_site_counts_its_transfer_log() {
    for site in Site::ALL {
        for gas_limit in GAS_LIMITS {
            let case = format!("{site:?} at {gas_limit}");
            let (db, tx) = site.program(Variant::Plain, gas_limit);
            let body = transaction_body_bytes(&tx);
            let outcome = execute(db, tx, EvmTxRuntimeLimits::no_limits());

            assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
            assert_eq!(
                transfer_logs(&outcome),
                [transfer_log(site.sender(), site.recipient(), U256::from(VALUE))],
                "{case}: the receipt's transfer log",
            );
            assert_eq!(balance(&outcome, site.recipient()), U256::from(VALUE), "{case}");
            assert_eq!(
                outcome.usage,
                LimitUsage { data_size: body + site.bytes(), write_records: site.records() },
                "{case}: the body, the records and one transfer log",
            );
            let history_bytes = body + site.records() * WRITE_RECORD_SIZE;
            assert_eq!(outcome.gas.history_bytes, history_bytes, "{case}: no history bytes");
            assert_eq!(outcome.gas.history, history_gas(history_bytes).unwrap(), "{case}");
            assert_reservoir_paid(&case, gas_limit, &outcome);
        }
    }
}

/// A frame budget holds the move at exactly its bytes, and one byte short of them stops the frame
/// that moves the value before anything moves, frame-locally: the caller resumes — the relay and
/// the actor return the stop as their output — and nothing is latched. The transaction's own
/// frame has no caller, so its stop reverts the transaction, still without a latch.
///
/// A `LOG3` of one word where the transfer log is, after the same bytes of records, stops the
/// same frame at the same budget with the same revert.
#[test]
fn test_each_site_stops_one_byte_short_of_its_frame_budget() {
    for site in Site::ALL {
        let cap = cap_for(site.depth(), site.bytes());
        for gas_limit in GAS_LIMITS {
            for variant in [Variant::Plain, Variant::Twin] {
                let case = format!("{site:?} {variant:?} at {gas_limit}");
                let run = |cap| {
                    let (db, tx) = site.program(variant, gas_limit);
                    let body = transaction_body_bytes(&tx);
                    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(cap);
                    (execute(db, tx, limits), body)
                };

                let (fits, _) = run(cap);
                assert!(fits.result.is_success(), "{case}: at the budget: {:?}", fits.result);
                assert_eq!(fits.limit_exceeded, None, "{case}");

                let (over, body) = run(cap - 1);
                assert_eq!(over.limit_exceeded, None, "{case}: a frame budget latches nothing");
                assert_eq!(
                    over.result.is_success(),
                    site.depth() > 0,
                    "{case}: the caller resumes: {:?}",
                    over.result
                );
                let stop = MegaLimitExceeded {
                    kind: LimitKind::DataSize.as_u8(),
                    limit: site.bytes() - 1,
                };
                match variant {
                    Variant::Plain => assert_stopped_at_the_move(&case, site, &over, body, stop),
                    Variant::Twin => {
                        assert_eq!(
                            over.result.output(),
                            Some(&Bytes::from(stop.abi_encode())),
                            "{case}: the same stop",
                        );
                        let left = site.left_by_a_stop();
                        assert_eq!(over.usage.data_size - body, left.data_size, "{case}");
                        assert!(over.result.logs().is_empty(), "{case}: the log went");
                    }
                    Variant::TakenBack => unreachable!(),
                }
                assert_reservoir_paid(&case, gas_limit, &over);
            }
        }
    }
}

/// The transaction's limit one byte short of the move stops the transaction there: it latches, the
/// stop names the data size and the limit, the whole transaction reverts, and the move's bytes are
/// reported in the figure that crossed. A `LOG3` of one word where the transfer log is, after the
/// same bytes of records, crosses the same way. At the transaction's own frame, whose budget is
/// the limit, a limit of exactly the bytes holds.
#[test]
fn test_each_site_stops_one_byte_short_of_the_transaction_limit() {
    for site in Site::ALL {
        for gas_limit in GAS_LIMITS {
            for variant in [Variant::Plain, Variant::Twin] {
                let case = format!("{site:?} {variant:?} at {gas_limit}");
                let run = |over: u64| {
                    let (db, tx) = site.program(variant, gas_limit);
                    let body = transaction_body_bytes(&tx);
                    let limit = body + site.bytes() - over;
                    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
                    (execute(db, tx, limits), body, limit)
                };

                let (outcome, body, limit) = run(1);
                assert_eq!(
                    outcome.limit_exceeded,
                    Some(LimitCheck::ExceedsLimit {
                        kind: LimitKind::DataSize,
                        limit,
                        used: body + site.bytes(),
                        frame_local: false,
                    }),
                    "{case}: the transaction is stopped at the move",
                );
                assert!(!outcome.result.is_success() && !outcome.result.is_halt(), "{case}");
                let stop = MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit };
                assert_eq!(
                    outcome.result.output(),
                    Some(&Bytes::from(stop.abi_encode())),
                    "{case}: the stop is what the transaction reports",
                );
                assert!(outcome.result.logs().is_empty(), "{case}: every log went");
                assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 });
                if variant == Variant::Plain {
                    let to = site.recipient();
                    assert_eq!(balance(&outcome, to), U256::ZERO, "{case}: no value moved");
                }
                assert_reservoir_paid(&case, gas_limit, &outcome);

                if site.depth() == 0 {
                    let (fits, _, _) = run(0);
                    assert!(fits.result.is_success(), "{case}: at the limit: {:?}", fits.result);
                }
            }
        }
    }
}

/// A failure above the move takes the transfer log back with the move, and its bytes with them:
/// the actor reverting after it, or the transaction's own frame failing. What is left counted is
/// the body.
#[test]
fn test_a_failure_takes_each_sites_transfer_log_back() {
    for site in Site::ALL {
        for gas_limit in GAS_LIMITS {
            let case = format!("{site:?} at {gas_limit}");
            let (db, tx) = site.program(Variant::TakenBack, gas_limit);
            let body = transaction_body_bytes(&tx);
            let outcome = execute(db, tx, EvmTxRuntimeLimits::no_limits());

            assert_eq!(outcome.limit_exceeded, None, "{case}");
            assert!(!outcome.result.is_halt(), "{case}: {:?}", outcome.result);
            assert_eq!(outcome.result.is_success(), site.depth() > 0, "{case}: the relay resumes");
            assert!(outcome.result.logs().is_empty(), "{case}: the log went with the move");
            assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 }, "{case}");
            if site == Site::SelfDestruct {
                assert_eq!(balance(&outcome, DESTRUCTOR), U256::from(VALUE), "{case}");
            }
            assert_eq!(balance(&outcome, site.recipient()), U256::ZERO, "{case}");
            assert_reservoir_paid(&case, gas_limit, &outcome);
        }
    }
}

/// Nothing moves to another account, so nothing is logged or counted for a log: a `CALLCODE` and a
/// `CALL` to itself move value from an account to itself; a transaction's value to its own sender
/// likewise; a destruction to itself burns the balance of an account created in the transaction
/// — this revm emits no log for a burn — and leaves an older account's balance where it is; a
/// value call the caller cannot fund, or a system contract refuses, moves nothing at all.
#[test]
fn test_no_transfer_log_where_no_value_moves_to_another_account() {
    let value = U256::from(VALUE);
    let burner = BytecodeBuilder::default().append(ADDRESS).append(SELFDESTRUCT).build();
    let refused = BytecodeBuilder::default()
        .mstore(0, IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR)
        .append_many([PUSH0, PUSH0])
        .push_number(4u64)
        .append_many([PUSH0, PUSH1, 1])
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append(GAS)
        .append(CALL);
    let db = |actor: BytecodeBuilder| {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_balance(ACTOR, U256::from(10 * VALUE))
            .account_code(ACTOR, actor.append(POP).stop().build())
            .account_code(RECEIVER, Bytes::from_static(&[STOP]))
            .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
            .account_balance(DESTRUCTOR, value)
            .account_code(DESTRUCTOR, burner.clone())
    };
    let call = |to, value: u64| {
        OpTx(op_transaction(TxEnv {
            caller: CALLER,
            kind: TxKind::Call(to),
            value: U256::from(value),
            gas_limit: 20_000_000,
            ..Default::default()
        }))
    };
    let actor = BytecodeBuilder::default;
    let cases: [(&str, MemoryDatabase, MegaTransaction, u64, usize); 8] = [
        ("a CALLCODE", db(actor().callcode(RECEIVER, value)), call(ACTOR, 0), 1, 0),
        ("a CALL to itself", db(actor().call(ACTOR, value)), call(ACTOR, 0), 1, 0),
        ("a zero-value CALL", db(actor().call(RECEIVER, U256::ZERO)), call(ACTOR, 0), 0, 0),
        ("a transaction's value to its sender", db(actor()), call(CALLER, VALUE), 0, 0),
        (
            "a creation that destructs to itself: its endowment's log alone",
            db(actor().create(value, &burner)),
            call(ACTOR, 0),
            2,
            1,
        ),
        ("an older account destructing to itself", db(actor()), call(DESTRUCTOR, 0), 0, 0),
        (
            "a value CALL the caller cannot fund",
            db(actor().call(RECEIVER, U256::from(100 * VALUE))),
            call(ACTOR, 0),
            0,
            0,
        ),
        ("a value call a system contract refuses", db(refused), call(ACTOR, 0), 0, 0),
    ];
    for (name, db, tx, records, logs) in cases {
        let body = transaction_body_bytes(&tx);
        let outcome = execute(db, tx, EvmTxRuntimeLimits::no_limits());
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.result.logs().len(), logs, "{name}: the logs");
        assert_eq!(transfer_logs(&outcome).len(), logs, "{name}: the transfer logs");
        assert_eq!(
            outcome.usage,
            LimitUsage {
                data_size: body + records * WRITE_RECORD_SIZE + logs as u64 * TRANSFER_LOG_SIZE,
                write_records: records,
            },
            "{name}",
        );
    }
    let burned = execute(
        db(actor().create(value, &burner)),
        call(ACTOR, 0),
        EvmTxRuntimeLimits::no_limits(),
    );
    let created = ACTOR.create(burned.state[&ACTOR].info.nonce - 1);
    assert_eq!(balance(&burned, created), U256::ZERO, "the endowment was burned");
    assert_eq!(balance(&burned, ACTOR), U256::from(9 * VALUE));
    let older = execute(db(actor()), call(DESTRUCTOR, 0), EvmTxRuntimeLimits::no_limits());
    assert_eq!(balance(&older, DESTRUCTOR), value, "an older account keeps its balance");
}

/// A deposit's value moves in its first frame and is logged there, from the depositor to the
/// recipient; its mint is credited before any frame and is logged nowhere. The log is counted in
/// the data size like any other, though a deposit pays no history, and a user's deposit is held
/// to the data-size limit at it.
#[test]
fn test_a_deposits_value_is_logged_and_its_mint_is_not() {
    const DEPOSITOR: Address = address!("0000000000000000000000000000000000d000de");
    let deposit = |mint: u128, value: u64| {
        let mut tx = OpTx(op_transaction(TxEnv {
            caller: DEPOSITOR,
            kind: TxKind::Call(RECEIVER),
            value: U256::from(value),
            gas_limit: 1_000_000,
            gas_price: 0,
            ..Default::default()
        }));
        tx.0.deposit.source_hash = B256::repeat_byte(0x11);
        tx.0.deposit.mint = Some(mint);
        tx
    };
    let db = || MemoryDatabase::default().account_code(RECEIVER, Bytes::from_static(&[STOP]));

    let tx = deposit(1_000, 400);
    let body = transaction_body_bytes(&tx);
    let outcome = execute(db(), tx, EvmTxRuntimeLimits::no_limits());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.result.logs(),
        [transfer_log(DEPOSITOR, RECEIVER, U256::from(400))],
        "the value's log, and none for the mint",
    );
    assert_eq!(balance(&outcome, DEPOSITOR), U256::from(600));
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: body + WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE, write_records: 1 },
    );
    assert_eq!((outcome.gas.history, outcome.gas.history_bytes), (0, 0), "a deposit pays none");

    let minted = execute(db(), deposit(1_000, 0), EvmTxRuntimeLimits::no_limits());
    assert!(minted.result.is_success(), "{:?}", minted.result);
    assert!(minted.result.logs().is_empty(), "a mint alone is logged nowhere");
    assert_eq!(balance(&minted, DEPOSITOR), U256::from(1_000));

    let limit = body + WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE - 1;
    let stopped = execute(
        db(),
        deposit(1_000, 400),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
    );
    assert_eq!(
        stopped.limit_exceeded.map(|stop| (stop.exceeded_limit(), stop.is_frame_local())),
        Some((true, false)),
        "a user's deposit is stopped at its transfer log",
    );
    assert!(stopped.result.logs().is_empty());
    assert_eq!(balance(&stopped, DEPOSITOR), U256::from(1_000), "the mint stays, the value not");
}

/// A system transaction moves no value in the shape the sequencer builds it, so it logs nothing.
/// One that does move value is logged and counted like any other, and — the protocol's own work —
/// no data-size limit stops it at the log.
#[test]
fn test_a_system_transactions_value_is_logged_and_no_limit_stops_it() {
    let db = || {
        MemoryDatabase::default()
            .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(VALUE))
            .account_code(ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[STOP]))
    };
    let system = |value: u64| {
        OpTx(op_transaction(TxEnv {
            caller: MEGA_SYSTEM_ADDRESS,
            kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
            value: U256::from(value),
            gas_limit: 1_000_000,
            ..Default::default()
        }))
    };

    let quiet = execute(db(), system(0), EvmTxRuntimeLimits::no_limits());
    assert!(quiet.result.is_success(), "{:?}", quiet.result);
    assert!(quiet.result.logs().is_empty(), "a valueless system transaction logs nothing");

    let tx = system(VALUE);
    let body = transaction_body_bytes(&tx);
    for limits in [
        EvmTxRuntimeLimits::no_limits(),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(0).with_frame_data_size_limit(0),
    ] {
        let outcome = execute(db(), tx.clone(), limits);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(
            outcome.result.logs(),
            [transfer_log(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, U256::from(VALUE))],
        );
        assert_eq!(
            outcome.usage,
            LimitUsage {
                data_size: body + WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE,
                write_records: 1
            },
            "counted all the same",
        );
    }
}
