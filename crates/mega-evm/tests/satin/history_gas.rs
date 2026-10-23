//! History gas: what a Satin transaction pays for the bytes it appends to the chain's history.
//!
//! Every charge is a byte count at the cost per history byte, and every assertion here writes the
//! count out, so a repricing moves the numbers without rewriting the test.

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE, TX_GAS_LIMIT_CAP},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LOG_BASE_SIZE, LOG_TOPIC_SIZE, TX_BASE_SIZE, TX_BODY_SIZE, TX_FIXED_WRITE_RECORDS,
    WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLCODE, CREATE, CREATE2, DELEGATECALL, GAS, LOG0, POP, PUSH0, PUSH1, RETURN,
        REVERT, STATICCALL, STOP,
    },
    context_interface::cfg::GasId,
};

use crate::common::{
    body_history, call, call_with_data, create, deploying, execute, history, history_is_free,
    history_rounds, reservoir_share, runs_at_measurement_prices,
};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const CALLEE: Address = address!("0000000000000000000000000000000000200001");
const CONTRACT: Address = address!("0000000000000000000000000000000000200002");

/// Room for the state gas of a new account and the code it deposits.
const GAS_LIMIT: u64 = 50_000_000;

/// The cost per history byte, spelled as the constant the tests price against.
const CPHB: u64 = COST_PER_HISTORY_BYTE;

/// The history a transaction's body costs when it carries `bytes` bytes beside its envelope.
const fn body(bytes: u64) -> u64 {
    (TX_BODY_SIZE + bytes) * CPHB
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)))
}

/// Init code that reverts without deploying anything.
fn reverting() -> Bytes {
    BytecodeBuilder::default().append_many([PUSH0, PUSH0, REVERT]).build()
}

/// Deployed code is both state and history: its bytes enter the world state and the chain's
/// history, so a deployment pays the per-byte state rate and the per-byte history rate on the
/// same length.
#[test]
fn test_deployed_code_pays_history_for_every_byte() {
    if runs_at_measurement_prices() {
        return;
    }
    let short = execute(funded(), create(CALLER, deploying(32), GAS_LIMIT));
    let long = execute(funded(), create(CALLER, deploying(64), GAS_LIMIT));

    assert!(short.result.is_success(), "{:?}", short.result);
    assert!(long.result.is_success(), "{:?}", long.result);
    assert_eq!(long.gas.history - short.gas.history, 32 * CPHB, "32 more bytes of history");
    assert_eq!(long.gas.state - short.gas.state, 32 * COST_PER_STATE_BYTE, "and of state");
}

/// A deployment that reverts appends no code, so it pays no code-deposit history at all.
#[test]
fn test_a_reverted_deployment_pays_no_code_deposit_history() {
    if runs_at_measurement_prices() {
        return;
    }
    let deployed = execute(funded(), create(CALLER, deploying(32), GAS_LIMIT));
    let reverted = execute(funded(), create(CALLER, reverting(), GAS_LIMIT));

    assert!(deployed.result.is_success(), "{:?}", deployed.result);
    assert!(!reverted.result.is_success(), "the creation reverts");
    assert_eq!(
        deployed.gas.history,
        body(deploying(32).len() as u64) + WRITE_RECORD_SIZE * CPHB + 32 * CPHB,
        "the body the init code travels in, the created account's record, the deployed bytes",
    );
    assert_eq!(
        reverted.gas.history,
        body(reverting().len() as u64),
        "the body alone: nothing was deployed, and the account's record went with the failure",
    );
}

/* ---------- the transaction body ---------- */

/// Calldata is part of the body, one history byte per byte, whatever the bytes are: a zero byte
/// takes the same space in a block as a non-zero one.
#[test]
fn test_calldata_costs_one_history_byte_per_byte() {
    if runs_at_measurement_prices() {
        return;
    }
    let run = |len: usize, byte: u8| {
        let db = funded().account_code(CALLEE, Bytes::new());
        let data = Bytes::from(vec![byte; len]);
        execute(db, call_with_data(CALLER, CALLEE, data, GAS_LIMIT))
    };
    let zeros = run(100, 0x00);
    let non_zeros = run(100, 0xff);
    let longer = run(200, 0x00);

    assert_eq!(zeros.gas.history, body(100));
    assert_eq!(non_zeros.gas.history, zeros.gas.history, "the value of a byte is not its size");
    assert_eq!(longer.gas.history - zeros.gas.history, 100 * CPHB, "a hundred bytes more");
}

/* ---------- the sites of completed writes ---------- */

/// The body of a `LOG` with `topics` topics over `data_len` bytes of memory, without the `STOP`
/// that ends a program.
fn log_body(topics: u8, data_len: u64) -> BytecodeBuilder {
    let mut code = BytecodeBuilder::default();
    for topic in 0..topics {
        code = code.push_number(u64::from(topic) + 1);
    }
    code.push_number(data_len).push_number(0u64).append(LOG0 + topics)
}

/// A program that emits one such log and stops.
fn logging(topics: u8, data_len: u64) -> Bytes {
    log_body(topics, data_len).stop().build()
}

/// A log costs history on top of what the EVM itself charges for the opcode: the schedule's own
/// price is untouched, and the history is the bytes.
#[test]
fn test_a_log_pays_history_on_top_of_the_schedules_price() {
    if runs_at_measurement_prices() {
        return;
    }
    let run = |code: Bytes| {
        let db = funded().account_code(CALLEE, code);
        execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
    };
    let quiet = run(BytecodeBuilder::default().stop().build());
    let logging = run(logging(1, 32));

    // The EVM's own price for the opcode: two pushes for the offset and length, one for the
    // topic, the `LOG` base, the per-topic and per-byte rates, and one word of memory.
    let params = mega_evm::satin_gas_params();
    let native =
        3 * 3 + 375 + params.get(GasId::logtopic()) + 32 * params.get(GasId::logdata()) + 3;
    assert_eq!(
        logging.gas.regular - quiet.gas.regular,
        native,
        "the schedule charges what it always did",
    );
    assert_eq!(
        logging.gas.history - quiet.gas.history,
        (LOG_BASE_SIZE + LOG_TOPIC_SIZE + 32) * CPHB
    );
}

/* ---------- a frame-start charge the caller cannot pay ---------- */

/// `CALL(GAS, target, 1 wei, [], [])`, discarding the flag, then `STOP`: a value transfer that
/// forwards everything the caller may forward, so the caller keeps 2 per cent of what it held.
fn transfers_everything_to(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// `CREATE` of empty init code carrying one wei, discarding the address, then `STOP`.
fn creates_everything() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

/// A funded caller holding `code`, with a balance to transfer.
fn caller_running(code: Bytes) -> MemoryDatabase {
    funded().account_balance(CALLEE, U256::from(10_000_000)).account_code(CALLEE, code)
}

/// The two records a value call's start makes cost more than a caller under roughly 352,000 gas
/// at the forward keeps after the 98 per cent it forwards. Such a caller pays for none of them by
/// halting: the frame it was starting does not run.
#[test]
fn test_a_call_whose_caller_cannot_pay_its_records_starts_no_frame() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = transfers_everything_to(CONTRACT);
    let short = execute(caller_running(code.clone()), call(CALLER, CALLEE, U256::ZERO, 560_000));
    let ample = execute(caller_running(code), call(CALLER, CALLEE, U256::ZERO, 1_000_000));

    assert!(!short.result.is_success(), "the caller halts rather than starting the frame");
    assert_eq!(short.usage.write_records, 0, "the frame that never ran wrote nothing");
    assert_eq!(short.gas.history, body(0), "and the transaction pays for its body alone");

    assert!(ample.result.is_success(), "{:?}", ample.result);
    assert_eq!(ample.usage.write_records, 2, "the caller's account and the recipient's");
    assert_eq!(ample.gas.history, body(0) + 2 * WRITE_RECORD_SIZE * CPHB);
}

/// The same for a creation, whose start records the created account and the creator's nonce.
#[test]
fn test_a_creation_whose_creator_cannot_pay_its_records_starts_no_frame() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = creates_everything();
    let short = execute(caller_running(code.clone()), call(CALLER, CALLEE, U256::ZERO, 560_000));
    let ample = execute(caller_running(code), call(CALLER, CALLEE, U256::ZERO, 1_000_000));

    assert!(!short.result.is_success(), "the creator halts rather than starting the creation");
    assert_eq!(short.usage.write_records, 0, "the creation that never ran wrote nothing");
    assert_eq!(short.gas.history, body(0), "and the transaction pays for its body alone");

    assert!(ample.result.is_success(), "{:?}", ample.result);
    assert_eq!(ample.usage.write_records, 2, "the created account and the creator's nonce");
    assert_eq!(ample.gas.history, body(0) + 2 * WRITE_RECORD_SIZE * CPHB);
}

/// A creation onto an account the transaction already changed makes no record of the created
/// account — the transaction's earlier write made it — so its creator pays for its own nonce's
/// record alone. `CONTRACT` funds the address `CALLEE`'s `CREATE2` lands on with one wei, a call
/// answered without running, then calls `CALLEE` with the gas each case gives it. `CALLEE` runs
/// `CREATE2` of empty init code, which charges four `PUSH0` and the creation's own gas — no state
/// gas, for an account that exists — and forwards all but 2 per cent of what is left, keeping that
/// fiftieth to pay the start's records with. Given 50 times one record's price on top of its
/// charges, it starts the creation; one gas less, it does not.
#[test]
fn test_a_creation_onto_an_account_the_transaction_changed_charges_its_creator_one_record() {
    // Records that cost nothing leave no price to be short of, and records priced at a fraction of
    // a gas may round to nothing.
    if history_is_free() || history_rounds() {
        return;
    }
    let created = CALLEE.create2(B256::ZERO, keccak256([]));
    let price = history(WRITE_RECORD_SIZE);
    let charges = 4 * 2 + mega_evm::satin_gas_params().get(GasId::create());
    let run = |given: u64| {
        let funds_then_calls = BytecodeBuilder::default()
            .call(created, U256::from(1))
            .append_many([POP, PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(CALLEE)
            .push_number(given)
            .append_many([CALL, POP, STOP])
            .build();
        let creates = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, CREATE2, POP, STOP])
            .build();
        let db = funded()
            .account_code(CONTRACT, funds_then_calls)
            .account_balance(CONTRACT, U256::from(1))
            .account_code(CALLEE, creates);
        let outcome = execute(db, call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT));
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        outcome
    };

    let short = run(charges + 50 * price - 1);
    assert_eq!(short.state[&created].info.nonce, 0, "a gas short, the creation never started");
    assert_eq!(short.usage.write_records, 2, "the funding call's: CONTRACT's and the address's");
    assert_eq!(short.gas.history, body_history(0) + 2 * price);

    let exact = run(charges + 50 * price);
    assert_eq!(exact.state[&created].info.nonce, 1, "the creation ran");
    assert_eq!(exact.usage.write_records, 3, "and added its creator's record alone");
    assert_eq!(exact.gas.history, body_history(0) + 3 * price);
}

/// Every call kind reaches the start's charge after its forward. A caller given its charges, an
/// explicit forward and one gas less than what the reservoir leaves of its records' price keeps too
/// little once it forwarded, and starts no frame; given that gas in full it starts the frame and
/// pays the price, the reservoir first and then its own gas. Below the execution cap there is no
/// reservoir; above it the reservoir holds half the price. A `DELEGATECALL` or a `STATICCALL`
/// moves no value, and a `CALLCODE` moves it from the calling account to itself: none makes a
/// record, so from the same gas each starts, and pays no history.
///
/// `CALLEE` calls `CONTRACT` with the gas each case gives it and returns the call's flag, so the
/// flag says whether `CONTRACT`'s own start happened. `TARGET` holds a balance and no code, so the
/// start is answered without running, and no account is created.
#[test]
fn test_every_call_kind_charges_its_records_after_its_forward() {
    if runs_at_measurement_prices() {
        return;
    }
    const TARGET: Address = address!("0000000000000000000000000000000000200003");
    let params = mega_evm::satin_gas_params();
    let cold_access = params.get(GasId::warm_storage_read_cost()) +
        params.get(GasId::cold_account_additional_cost());
    let transfer = params.get(GasId::transfer_value_cost());
    // `scheme(forward, TARGET, value, [], [])` and its charges before the forward: four `PUSH0`, a
    // `PUSH1` of the value where the scheme carries one, the `PUSH20` and the `PUSH8` of the
    // forward, then the cold access and the transfer.
    let start = |scheme: u8, forward: u64| {
        let code = BytecodeBuilder::default().append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
        let (code, charges) = if matches!(scheme, CALL | CALLCODE) {
            (code.push_number(1_u8), 8 + 3 + 3 + 3 + cold_access + transfer)
        } else {
            (code, 8 + 3 + 3 + cold_access)
        };
        (
            code.push_address(TARGET).push_number(forward).append_many([scheme, STOP]).build(),
            charges,
        )
    };
    let run = |code: Bytes, given: u64, reservoir: Option<u64>| {
        let caller = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(CONTRACT)
            .push_number(given)
            .append(CALL)
            .return_top()
            .build();
        let db = funded()
            .account_code(CALLEE, caller)
            .account_code(CONTRACT, code)
            .account_balance(CONTRACT, U256::from(1))
            .account_balance(TARGET, U256::from(1));
        let gas_limit =
            reservoir.map_or(GAS_LIMIT, |reservoir| TX_GAS_LIMIT_CAP + body(0) + reservoir);
        let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, gas_limit));
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let started = U256::from_be_slice(outcome.result.output().unwrap()) == U256::from(1);
        (started, outcome)
    };

    for (scheme, records) in [(CALL, 2)] {
        let price = records * WRITE_RECORD_SIZE * CPHB;
        for reservoir in [None, Some(price / 2)] {
            let case = format!("scheme {scheme:#04x}, reservoir {reservoir:?}");
            // What the caller's own gas pays once the reservoir it was given is spent — its
            // caller's share of the transaction's — and a forward that leaves it one gas less than
            // that, the most the 98/100 rule lets it forward.
            let given = reservoir.map_or(0, reservoir_share);
            let spill = price - given;
            let forward = 49 * (spill - 1);
            let (code, charges) = start(scheme, forward);

            let (started, short) = run(code.clone(), charges + forward + spill - 1, reservoir);
            assert!(!started, "{case}: one gas short, no frame");
            assert_eq!(short.usage.write_records, 0, "{case}");
            assert_eq!(short.gas.history, body(0), "{case}: the body alone");
            if let Some(reservoir) = reservoir {
                assert_eq!(short.gas.reservoir_remaining, reservoir, "{case}: the reservoir too");
            }

            let (started, exact) = run(code, charges + forward + spill, reservoir);
            assert!(started, "{case}: the gas in full, the frame starts");
            assert_eq!(exact.usage.write_records, records, "{case}");
            assert_eq!(exact.gas.history, body(0) + price, "{case}: the body and the records");
            // The share paid first. The part of the reservoir its caller kept comes back with the
            // transaction's own frame, which merged the spill: it unwinds that spill onto regular
            // gas before any of it joins the reservoir, so none is left.
            if reservoir.is_some() {
                assert_eq!(
                    exact.gas.reservoir_remaining, 0,
                    "{case}: the spill took what was kept"
                );
            }

            // A value `CALLCODE` moves the value from the calling account to itself and makes
            // no record, as the two schemes that carry no value make none.
            for scheme in [CALLCODE, DELEGATECALL, STATICCALL] {
                let case = format!("{case}, then scheme {scheme:#04x} from the same gas");
                let (code, _) = start(scheme, forward);
                let (started, outcome) = run(code, charges + forward + spill - 1, reservoir);
                assert!(started, "{case}: no records, no charge to fall short of");
                assert_eq!(outcome.usage.write_records, 0, "{case}");
                assert_eq!(outcome.gas.history, body(0), "{case}");
            }
        }
    }
}

/// Whether a write record is paid for may not depend on how much gas its frame's caller had
/// left. Across the gas limits that straddle the point where a caller can no longer pay for the
/// records of the frame it starts, the history a transaction pays beyond its body is exactly the
/// records it kept.
///
/// Above the execution cap the same holds whatever the reservoir: one that runs out before the
/// body is paid for, one that runs out partway through the records, and ample ones. Each pays
/// what it can of the state and history ledgers and spills the rest onto regular gas.
#[test]
fn test_no_gas_limit_buys_a_write_record_for_nothing() {
    if runs_at_measurement_prices() {
        return;
    }
    let reservoirs = [1, body(0) / 2, body(0) + WRITE_RECORD_SIZE * CPHB, 1_000_000, 100_000_000];
    let above_the_cap = reservoirs.map(|reservoir| TX_GAS_LIMIT_CAP + reservoir);
    for code in [transfers_everything_to(CONTRACT), creates_everything()] {
        for limit in [300_000, 400_000, 600_000, 800_000, 1_000_000, 5_000_000]
            .into_iter()
            .chain(above_the_cap)
        {
            let outcome =
                execute(caller_running(code.clone()), call(CALLER, CALLEE, U256::ZERO, limit));
            if let Some(reservoir) = limit.checked_sub(TX_GAS_LIMIT_CAP) {
                assert!(outcome.result.is_success(), "{:?}", outcome.result);
                assert_eq!(
                    outcome.gas.reservoir_remaining,
                    reservoir.saturating_sub(outcome.gas.state + outcome.gas.history),
                    "at a {limit} gas limit: the reservoir paid what it could of state and history",
                );
            }
            assert_eq!(
                outcome.gas.history,
                body(0) + outcome.usage.write_records * WRITE_RECORD_SIZE * CPHB,
                "at a {limit} gas limit: the records kept are the history paid",
            );
            assert_eq!(
                outcome.gas.history_bytes,
                TX_BODY_SIZE + outcome.usage.write_records * WRITE_RECORD_SIZE,
                "at a {limit} gas limit: and the bytes reported",
            );
        }
    }
}

/* ---------- the writes every transaction makes ---------- */

/// The transaction body carries the writes every transaction makes whatever it runs: the sender's
/// account, and the four accounts its fees are credited to.
///
/// They are in the body rather than charged where they happen because the body is priced before
/// the transaction runs, when how many of the four are actually credited is not yet known — a
/// zero fee credits none, and a beneficiary that is one of the vaults is one account, not two.
/// The body carries the bound; nothing counts them again afterwards.
#[test]
fn test_the_body_carries_the_writes_every_transaction_makes() {
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(TX_BODY_SIZE, TX_BASE_SIZE + TX_FIXED_WRITE_RECORDS * WRITE_RECORD_SIZE);
    assert_eq!(TX_BODY_SIZE, 110 + 5 * 40);

    // A transaction that actually pays a fee credits the beneficiary and the fee vaults, and
    // records none of them: what it pays is what a fee-free transaction pays.
    let db = || funded().account_code(CALLEE, Bytes::new());
    let free = execute(db(), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    let paid = execute(db(), {
        let mut tx = call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT);
        tx.0.base.gas_price = 10;
        tx
    });

    assert!(free.result.is_success() && paid.result.is_success());
    assert_eq!(free.gas.history, body(0), "the body, and the body alone");
    assert_eq!(paid.gas.history, free.gas.history, "the fee recipients are already in the body");
    assert_eq!(paid.usage.write_records, 0, "and none of them is recorded again");
}

/// A deposit credits no fee recipient at all, and is exempt from history besides: it pays for
/// neither the body nor anything the body carries.
#[test]
fn test_a_deposit_pays_for_none_of_the_bodys_writes() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut tx = call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT);
    tx.0.deposit.source_hash = alloy_primitives::B256::repeat_byte(0x11);
    let outcome = execute(funded().account_code(CALLEE, Bytes::new()), tx);

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.history, 0);
    assert_eq!(outcome.usage.write_records, 0);
}

/// The body carries a bound on the fee recipients, not a count of them, so a beneficiary that is
/// one of the fee vaults is not something anything has to notice: the two transactions pay the
/// same history, and neither reads a recipient to find out.
#[test]
fn test_a_beneficiary_that_is_a_fee_vault_changes_nothing() {
    if runs_at_measurement_prices() {
        return;
    }
    use op_revm::constants::BASE_FEE_RECIPIENT;
    let db = || funded().account_code(CALLEE, Bytes::new());
    let paying = |beneficiary| {
        let mut tx = call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT);
        tx.0.base.gas_price = 10;
        let ctx = crate::common::context(db())
            .with_block(revm::context::BlockEnv { beneficiary, ..crate::common::block() });
        mega_evm::MegaEvm::new(ctx).execute_transaction(tx).expect("the transaction is valid")
    };
    let distinct = paying(address!("00000000000000000000000000000000000c0ffe"));
    let a_vault = paying(BASE_FEE_RECIPIENT);

    assert!(distinct.result.is_success() && a_vault.result.is_success());
    assert_eq!(distinct.gas.history, body(0));
    assert_eq!(a_vault.gas.history, distinct.gas.history);
    assert_eq!(a_vault.usage.write_records, 0);
}

/// The record the transaction's own frame makes — its value's recipient — is charged before the
/// frame and given back once: by the frame itself, when the recipient sends the value back and its
/// account returns to what the transaction loaded, so a frame that then succeeds keeps none of it
/// and the settlement gives nothing back again; by the settlement, when the frame fails, its own
/// give-back discarded with it. A recipient that keeps the value keeps the record and its history.
///
/// Independence: constants — the body and the record are the byte table's, at the price in effect.
#[test]
fn test_the_first_frames_record_is_given_back_once() {
    let one = U256::from(1);
    let returning = |end: u8| {
        BytecodeBuilder::default()
            .call(CALLER, one)
            .append(POP)
            .append_many([PUSH0, PUSH0, end])
            .build()
    };
    let cases = [
        ("returns the value and succeeds", returning(RETURN), true, 0),
        ("keeps the value", BytecodeBuilder::default().stop().build(), true, 1),
        ("returns the value and reverts", returning(REVERT), false, 0),
    ];
    for (name, code, succeeds, records) in cases {
        let outcome =
            execute(funded().account_code(CONTRACT, code), call(CALLER, CONTRACT, one, GAS_LIMIT));
        assert_eq!(outcome.result.is_success(), succeeds, "{name}: {:?}", outcome.result);
        assert_eq!(
            outcome.gas.history,
            body_history(0) + history(WRITE_RECORD_SIZE * records),
            "{name}: the body, and the recipient's record where it is kept"
        );
        assert_eq!(outcome.usage.write_records, records, "{name}");
        assert_eq!(
            outcome.gas.history_bytes,
            TX_BODY_SIZE + WRITE_RECORD_SIZE * records,
            "{name}: the history bytes follow"
        );
    }
}
