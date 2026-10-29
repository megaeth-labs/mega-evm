//! SALT pricing: giving a state gas charge back at the price it was made at.
//!
//! A state charge is undone in two shapes, and neither of them reads a second table.
//!
//! One shape names the charge and asks the hook for its price again: the same opcode taking its
//! own write back (`SSTORE` 0 -> x -> 0), the upfront charge a caller made for a call or a
//! creation that did not add the leaf after all, the transaction-level refund, and revm's
//! settlement of a frame result built without running a frame. Because the site is re-priced
//! through the one hook, and because the multiplier of a bucket is read once per transaction,
//! the refill is the charge — a bucket cannot be read at one capacity for the charge and another
//! for the refill.
//!
//! The other shape asks for no price at all: a frame that fails rolls its own charges back by
//! the amounts it recorded while running. Nothing but the hook priced those amounts, so they
//! cancel exactly whatever the price was.
//!
//! These tests pin both: what is fully undone nets zero at every multiplier, what is kept
//! scales, and the environment is asked once.
//!
//! Against that, an applied EIP-7702 authorization is *not* undone by a frame that reverts
//! later: the delegation it wrote survives the revert, so the state gas it paid must survive it
//! too.

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    synthetic_frame_result,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaTransaction,
};
use revm::{
    bytecode::opcode::{CALL, PUSH0, RETURN, REVERT},
    context::result::EVMError,
    context_interface::{
        cfg::{gas::GasTracker, GasId, StateGasCharge, StateGasSite},
        Host,
    },
    handler::EthFrame,
    interpreter::{
        interpreter::EthInterpreter, CallInput, CallInputs, CallScheme, CallValue, CreateInputs,
        CreateScheme, FrameInput, InstructionResult,
    },
};

use crate::{
    common::{history, state_is_free},
    salt::{
        account_bucket, authorization_tx, call_contract, create_with, crowded_account,
        crowded_slot, db, entry, minimal_envs, run, salt_context, selfdestruct_to, slot_bucket,
        try_run, tx, tx_with_gas, SaltEnvs, AUTHORITY, CALLER, CONTRACT, EMPTY, GAS_LIMIT,
    },
};

/// The contract a probe's inner frame runs in.
const SUB: Address = alloy_primitives::address!("0000000000000000000000000000000000c00005");

/// The slot every probe that writes storage uses.
const SLOT: u64 = 7;

/// The gas a probe's inner call forwards: 2,000,000 on top of `unit` of state gas at the largest
/// multiplier the probes crowd a bucket to, so the inner frame can pay for what it adds at every
/// multiplier and byte price.
fn inner_gas(unit: u64) -> u64 {
    2_000_000 + 8 * unit
}

/// Runs `probe` at each multiplier and returns the state gas it netted, requiring the regular
/// ledger to be the same at all of them.
fn net_state_at(
    ms: [u64; 3],
    crowd: impl Fn(SaltEnvs, u64) -> SaltEnvs,
    probe: impl Fn() -> (MemoryDatabase, MegaTransaction),
) -> [u64; 3] {
    let mut states = [0; 3];
    let mut regular = None;
    for (i, m) in ms.into_iter().enumerate() {
        let (db, tx) = probe();
        let outcome = try_run(db, crowd(minimal_envs(), m), tx).expect("the probe is valid");
        states[i] = outcome.gas.state;
        let previous = *regular.get_or_insert(outcome.gas.regular);
        assert_eq!(outcome.gas.regular, previous, "regular gas must not move with m");
    }
    states
}

/// `SSTORE` onto a fresh slot and straight back to zero: the restore refills exactly what the
/// set charged, so the transaction nets nothing on the state ledger however crowded the bucket
/// is — and the bucket is read once for both.
#[test]
fn test_a_slot_written_and_restored_nets_nothing_at_every_multiplier() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let code = || {
        BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .sstore(U256::from(SLOT), U256::ZERO)
            .stop()
            .build()
    };
    let crowd = |envs, m| crowded_slot(envs, CONTRACT, U256::from(SLOT), m);

    assert_eq!(
        net_state_at([1, 2, 8], crowd, || (db(code()), call_contract())),
        [0, 0, 0],
        "the restore gives back what the set charged",
    );

    let envs = crowd(minimal_envs(), 8);
    let outcome = run(db(code()), envs.clone(), call_contract());
    assert_eq!(outcome.gas.state, 0);
    assert_eq!(
        envs.bucket_queries(slot_bucket(CONTRACT, U256::from(SLOT))),
        1,
        "one capacity read priced both the charge and the refill",
    );
}

/// A creation whose init code reverts: the `CREATE` opcode charged its caller for the account
/// leaf upfront, and the frame that did not create it gives the charge back at the same price.
#[test]
fn test_a_reverting_creation_gives_its_upfront_charge_back() {
    let created = CONTRACT.create(0);
    let probe = || {
        // Init code `PUSH0 PUSH0 REVERT`: the creation fails, so no account leaf is added.
        let code = create_with(&[PUSH0, PUSH0, REVERT]).stop().build();
        (db(code), call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], move |envs, m| crowded_account(envs, created, m), probe),
        [0, 0, 0],
        "a creation that deployed nothing pays no state gas",
    );
}

/// A `CALL` that charged for a new account leaf and then failed gives the charge back: the
/// caller cannot afford the transfer, so no account is created.
#[test]
fn test_a_failed_value_call_gives_its_new_account_charge_back() {
    let probe = || {
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(1u64)
            .push_address(EMPTY)
            .push_number(1_000_000u64)
            .append(CALL)
            .stop()
            .build();
        // The contract holds nothing, so the transfer the CALL attempts cannot be made.
        let db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_code(CONTRACT, code);
        (db, call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_account(envs, EMPTY, m), probe),
        [0, 0, 0],
        "no account was created, so the upfront charge comes back",
    );
}

/// A charge made two frames deep and rolled back with its frame comes back whole: the inner
/// frame wrote a slot in a crowded bucket and reverted.
#[test]
fn test_a_charge_in_a_reverting_inner_frame_comes_back_whole() {
    let forwarded = inner_gas(entry(GasId::sstore_set_state_gas()));
    let probe = || {
        let sub = BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .revert_with_data([0xaa])
            .build();
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(SUB)
            .push_number(forwarded)
            .append(CALL)
            .stop()
            .build();
        (db(code).account_code(SUB, sub), call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_slot(envs, SUB, U256::from(SLOT), m), probe),
        [0, 0, 0],
        "the frame's rollback takes its state charge with it",
    );
}

/// What an inner frame keeps still scales: the same program with the inner frame succeeding
/// pays the crowded price, so the arms above are not passing because nothing was charged.
#[test]
fn test_a_charge_an_inner_frame_keeps_scales_with_its_bucket() {
    let forwarded = inner_gas(entry(GasId::sstore_set_state_gas()));
    let probe = || {
        let sub = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(SUB)
            .push_number(forwarded)
            .append(CALL)
            .stop()
            .build();
        (db(code).account_code(SUB, sub), call_contract())
    };

    let set = entry(GasId::sstore_set_state_gas());
    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_slot(envs, SUB, U256::from(SLOT), m), probe),
        [set, set * 2, set * 8],
    );
}

/// A `SELFDESTRUCT` charges for the beneficiary's leaf inside the frame that runs it, and a frame
/// whose caller does not survive gives the charge back: the inner frame self-destructed, the outer
/// one reverted, and the beneficiary was never created after all.
#[test]
fn test_a_selfdestruct_whose_caller_reverts_gives_its_charge_back() {
    let forwarded = inner_gas(entry(GasId::new_account_state_gas()));
    let probe = |outer_reverts: bool| {
        move || {
            let sub = selfdestruct_to(EMPTY).build();
            let mut code = BytecodeBuilder::default()
                .push_number(0u64)
                .push_number(0u64)
                .push_number(0u64)
                .push_number(0u64)
                .push_number(0u64)
                .push_address(SUB)
                .push_number(forwarded)
                .append(CALL);
            code = if outer_reverts { code.revert_with_data([0xaa]) } else { code.stop() };
            let db = db(code.build())
                .account_code(SUB, sub)
                .account_balance(SUB, U256::from(1_000_000u64));
            (db, call_contract())
        }
    };
    let crowd = |envs, m| crowded_account(envs, EMPTY, m);

    assert_eq!(
        net_state_at([1, 2, 8], crowd, probe(true)),
        [0, 0, 0],
        "the reverting caller takes the beneficiary's leaf back with it",
    );

    // The control: the same program whose outer frame survives keeps the charge, scaled. Without
    // it the zeroes above could be passing because nothing was ever charged.
    let new_account = entry(GasId::new_account_state_gas());
    assert_eq!(
        net_state_at([1, 2, 8], crowd, probe(false)),
        [new_account, new_account * 2, new_account * 8],
    );
}

/// The whole transaction reverting is the same story one level up: the outermost frame's charge
/// goes back with it.
#[test]
fn test_a_transaction_that_reverts_pays_no_state_gas() {
    let probe = || {
        let code = BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .revert_with_data([0xaa])
            .build();
        (db(code), call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_slot(envs, CONTRACT, U256::from(SLOT), m), probe),
        [0, 0, 0],
    );
}

/// An applied EIP-7702 authorization is not a frame's doing: the delegation it wrote outlives a
/// frame that reverts afterwards, and so does the state gas it paid.
#[test]
fn test_an_applied_authorization_keeps_its_charge_through_a_revert() {
    let authority_charges =
        entry(GasId::new_account_state_gas()) + entry(GasId::tx_eip7702_state_gas_bytecode());
    let probe = || {
        let code = BytecodeBuilder::default().revert_with_data([0xaa]).build();
        (db(code), authorization_tx(CONTRACT))
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_account(envs, AUTHORITY, m), probe),
        [authority_charges, authority_charges * 2, authority_charges * 8],
        "the authority's leaf and its delegation bytes are still there after the revert",
    );

    // And the delegation really did survive: the authority carries the designation.
    let outcome = try_run(
        db(BytecodeBuilder::default().revert_with_data([0xaa]).build()),
        crowded_account(minimal_envs(), AUTHORITY, 8),
        authorization_tx(CONTRACT),
    )
    .expect("the transaction is valid");
    assert!(
        outcome.state.get(&AUTHORITY).is_some_and(|account| account
            .info
            .code
            .as_ref()
            .is_some_and(|code| !code.is_empty())),
        "the authority kept its delegation",
    );
}

/// A creation whose deployed code is refused before any deposit charge is made: EIP-3541 turns
/// down a runtime code starting `0xEF` at validation, so the only state gas in play is the
/// account leaf the transaction's own create target was charged for upfront, which comes back.
/// The arm below it is the one where a deposit charge really is made and then rolled back.
#[test]
fn test_a_refused_code_deposit_pays_no_state_gas() {
    let created = CALLER.create(0);
    let probe = || {
        // Init code returning a single `0xEF` byte, which EIP-3541 refuses to deploy.
        let init = BytecodeBuilder::default()
            .mstore(0, [0xefu8])
            .push_number(1u64)
            .push_number(0u64)
            .append(RETURN)
            .build();
        (db(Bytes::new()), tx(TxKind::Create, init, U256::ZERO))
    };

    assert_eq!(
        net_state_at([1, 2, 8], move |envs, m| crowded_account(envs, created, m), probe),
        [0, 0, 0],
        "nothing was deployed, so nothing is owed",
    );
}

/// A charge kept beside one given back: the kept one scales, the other nets out, and the
/// transaction pays exactly the difference.
#[test]
fn test_a_kept_charge_and_a_restored_one_settle_independently() {
    const KEPT: u64 = 3;
    let probe = || {
        let code = BytecodeBuilder::default()
            .sstore(U256::from(KEPT), U256::from(1))
            .sstore(U256::from(SLOT), U256::from(1))
            .sstore(U256::from(SLOT), U256::ZERO)
            .stop()
            .build();
        (db(code), call_contract())
    };
    let crowd = |envs: SaltEnvs, m: u64| {
        let envs = crowded_slot(envs, CONTRACT, U256::from(KEPT), m);
        crowded_slot(envs, CONTRACT, U256::from(SLOT), m)
    };

    let set = entry(GasId::sstore_set_state_gas());
    assert_eq!(net_state_at([1, 2, 8], crowd, probe), [set, set * 2, set * 8]);
}

/* The refund route that runs without a frame. */

/// The address a synthetic frame would have added a leaf at.
const SYNTHETIC_TARGET: Address = address!("0000000000000000000000000000000000c00008");

/// The regular gas a caller forwards to the frame the probes below answer without running it.
const FORWARDED: u64 = 9_000;

/// The caller's own regular gas limit in those probes, wide enough for a crowded charge to spill
/// onto it in full: 5,000,000 on top of a new account at the crowded price.
fn caller_limit() -> u64 {
    5_000_000 + entry(GasId::new_account_state_gas()) * SYNTHETIC_MULTIPLIER
}

/// The capacity the synthetic-settlement probes crowd their site to.
const SYNTHETIC_MULTIPLIER: u64 = 8;

/// What one synthetic settlement did: the caller's tracker afterwards, the price the charge was
/// made at, and how much of that charge spilled onto regular gas when it was made.
struct Settled {
    caller: GasTracker,
    price: u64,
    spilled: u64,
}

/// Charges `charge` through the pricing hook of a context reading `envs`, hands the frame the
/// reservoir the caller has left, and settles a synthetic failure for it into the caller through
/// revm's own frame return — the route a result takes when a mechanism answers a frame rather
/// than running it.
fn settle_a_synthetic_failure(
    envs: &SaltEnvs,
    reservoir: u64,
    charge: StateGasCharge,
    input: impl FnOnce(u64) -> FrameInput,
) -> Settled {
    let mut ctx = salt_context(db(Bytes::new()), envs.clone());
    let price = ctx.state_gas_charge(charge).expect("the bucket is readable");

    let mut caller = GasTracker::new(caller_limit(), caller_limit() - FORWARDED, reservoir);
    assert!(caller.record_state_cost(price), "the caller can pay the crowded charge");
    let spilled = caller.state_gas_spilled();

    // The calling opcode hands the frame a copy of the reservoir its charge left, which the
    // frame's return replaces with what the frame gives back.
    let share = caller.reservoir();
    let result = synthetic_frame_result(&input(share), InstructionResult::Revert, Bytes::new());
    let mut frame = EthFrame::<EthInterpreter>::default();
    *frame.interpreter.gas.tracker_mut() = caller;
    frame
        .return_result::<_, EVMError<core::convert::Infallible>>(&mut ctx, result)
        .expect("the refund is priced through the same hook");

    Settled { caller: *frame.interpreter.gas.tracker(), price, spilled }
}

/// The inputs of a `CALL` whose calling opcode charged the caller for the account it would add.
fn charged_call_input(reservoir: u64) -> FrameInput {
    FrameInput::Call(Box::new(CallInputs {
        input: CallInput::Bytes(Bytes::new()),
        return_memory_offset: 0..0,
        gas_limit: FORWARDED,
        bytecode_address: SYNTHETIC_TARGET,
        known_bytecode: Default::default(),
        target_address: SYNTHETIC_TARGET,
        caller: CALLER,
        value: CallValue::Transfer(U256::from(1)),
        scheme: CallScheme::Call,
        is_static: false,
        reservoir,
        charged_new_account_state_gas: true,
    }))
}

/// The inputs of a `CREATE` whose calling opcode charged the caller for the account it would
/// deploy.
fn charged_create_input(reservoir: u64) -> FrameInput {
    let mut inputs = CreateInputs::new(
        CALLER,
        CreateScheme::Create,
        U256::ZERO,
        Bytes::new(),
        FORWARDED,
        reservoir,
    );
    inputs.set_charged_create_state_gas(true);
    inputs.set_charged_state_gas_address(SYNTHETIC_TARGET);
    FrameInput::Create(Box::new(inputs))
}

/// Requires a settlement to have left the caller whole: the forwarded gas back on the regular
/// pool with whatever the charge spilled onto it, the reservoir at the value it held before the
/// charge, the state ledger net zero, and one capacity read behind the charge and the refill
/// alike.
fn assert_restores_both_pools(site: &str, envs: &SaltEnvs, reservoir: u64, settled: &Settled) {
    let caller = &settled.caller;
    assert_eq!(caller.remaining(), caller_limit(), "{site}: the regular pool comes back whole");
    assert_eq!(caller.reservoir(), reservoir, "{site}: and the reservoir is where it started");
    assert_eq!(caller.state_gas_spent(), 0, "{site}: the state ledger nets zero");
    assert_eq!(caller.state_gas_spilled(), 0, "{site}: nothing is left spilled");
    assert_eq!(
        envs.bucket_queries(account_bucket(SYNTHETIC_TARGET)),
        1,
        "{site}: one capacity read priced the charge and the refund",
    );
}

/// A synthetic `CALL` failure settled into its caller gives back a SALT-priced upfront
/// charge, both when the reservoir paid for it and when it spilled onto regular gas.
#[test]
fn test_a_synthetic_call_failure_refunds_the_crowded_upfront_charge() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let crowded = entry(GasId::new_account_state_gas()) * SYNTHETIC_MULTIPLIER;
    let charge = StateGasCharge::one(
        GasId::new_account_state_gas(),
        StateGasSite::account(SYNTHETIC_TARGET),
    );

    // The reservoir covers the whole charge.
    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, caller_limit(), charge, charged_call_input);
    assert_eq!(settled.price, crowded, "the charge was priced at the crowded bucket");
    assert_eq!(settled.spilled, 0, "and the reservoir paid for all of it");
    assert_restores_both_pools("a call from the reservoir", &envs, caller_limit(), &settled);

    // The reservoir covers part of it and the rest spills onto regular gas.
    let reservoir = crowded / 4;
    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, reservoir, charge, charged_call_input);
    assert_eq!(settled.price, crowded);
    assert_eq!(settled.spilled, crowded - reservoir, "the rest of the charge spilled");
    assert_restores_both_pools("a call that spilled", &envs, reservoir, &settled);
}

/// The same for a synthetic `CREATE` failure, whose upfront charge is the creation entry at the
/// address the frame would have deployed to.
#[test]
fn test_a_synthetic_creation_failure_refunds_the_crowded_upfront_charge() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let crowded = entry(GasId::create_state_gas()) * SYNTHETIC_MULTIPLIER;
    let charge =
        StateGasCharge::one(GasId::create_state_gas(), StateGasSite::account(SYNTHETIC_TARGET));

    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, caller_limit(), charge, charged_create_input);
    assert_eq!(settled.price, crowded, "the charge was priced at the crowded bucket");
    assert_eq!(settled.spilled, 0);
    assert_restores_both_pools("a creation from the reservoir", &envs, caller_limit(), &settled);

    let reservoir = crowded / 4;
    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, reservoir, charge, charged_create_input);
    assert_eq!(settled.price, crowded);
    assert_eq!(settled.spilled, crowded - reservoir);
    assert_restores_both_pools("a creation that spilled", &envs, reservoir, &settled);
}

/* A deposit charge that is made and then rolled back. */

/// How many bytes of runtime code the creation below deploys when its deposit is meant to fail.
/// At the crowded price a byte costs `code_deposit_state_gas x 8` and its history; at the spec's
/// prices this many bytes is far beyond what [`GAS_LIMIT`] brings.
const UNAFFORDABLE_CODE: u64 = 20_000;

/// How many it deploys when the deposit is meant to go through.
const AFFORDABLE_CODE: u64 = 32;

/// A creation whose init code writes a slot in a crowded bucket and then returns `deployed`
/// bytes of runtime code, run from [`CONTRACT`] so the transaction outlives the creation.
fn creation_depositing(deployed: u64) -> MemoryDatabase {
    let init = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .push_number(deployed)
        .append(PUSH0)
        .append(RETURN)
        .build();
    db(create_with(&init).stop().build())
}

/// A creation that runs out of gas paying for its code deposit takes back both of the charges it
/// made: the slot its init code wrote in a crowded bucket, and the account leaf the `CREATE`
/// opcode charged its caller for upfront. The outer frame stops after the failed creation, so
/// the transaction settles and reports what it kept.
#[test]
fn test_a_creation_that_cannot_pay_its_code_deposit_keeps_no_state_gas() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let created = CONTRACT.create(0);
    let crowd = |envs| {
        let envs = crowded_account(envs, created, SYNTHETIC_MULTIPLIER);
        crowded_slot(envs, created, U256::from(SLOT), SYNTHETIC_MULTIPLIER)
    };

    // What the program spends with nothing to deposit, and at least what the deposit costs: its
    // state gas at the crowded price and its history.
    let without_the_deposit =
        run(creation_depositing(0), crowd(minimal_envs()), call_contract()).gas.gas_used;
    let deposit = entry(GasId::code_deposit_state_gas()) * SYNTHETIC_MULTIPLIER * UNAFFORDABLE_CODE +
        history(UNAFFORDABLE_CODE);
    // A deposit that costs less than the rest of the transaction leaves no gas limit at which the
    // creation runs its init code and then cannot pay for the deposit.
    if deposit < without_the_deposit {
        return;
    }
    // The deposit's price on top of the rest, below the execution cap as every probe here runs:
    // forwarded 98/100 of it, the creation runs its init code and falls short of the deposit.
    let gas_limit = (without_the_deposit + deposit).min(GAS_LIMIT);

    let envs = crowd(minimal_envs());
    let call = tx_with_gas(TxKind::Call(CONTRACT), Bytes::new(), U256::ZERO, gas_limit);
    let outcome = run(creation_depositing(UNAFFORDABLE_CODE), envs.clone(), call);
    assert_eq!(outcome.gas.state, 0, "the failed creation kept neither charge");
    assert_eq!(
        envs.bucket_queries(slot_bucket(created, U256::from(SLOT))),
        1,
        "the init code did write the slot, and it was priced before the deposit failed",
    );
    assert_eq!(envs.bucket_queries(account_bucket(created)), 1, "and so was the account leaf");
    assert!(
        outcome.state.get(&created).is_none_or(|account| account
            .info
            .code
            .as_ref()
            .is_none_or(|code| code.is_empty())),
        "and nothing was deployed",
    );

    // The control: the same program with a deposit it can pay keeps all three charges, scaled.
    let envs = crowd(minimal_envs());
    let outcome = run(creation_depositing(AFFORDABLE_CODE), envs, call_contract());
    assert_eq!(
        outcome.gas.state,
        (entry(GasId::create_state_gas()) +
            entry(GasId::sstore_set_state_gas()) +
            entry(GasId::code_deposit_state_gas()) * AFFORDABLE_CODE) *
            SYNTHETIC_MULTIPLIER,
        "the creation, the slot its init code wrote and the bytes it deposited",
    );
}

/// A `CREATE` whose init code `SELFDESTRUCT`s the account it is creating, in the same transaction.
///
/// EIP-8037's gas refills for `SELFDESTRUCT` say that destruction leaves the account, its code
/// and its storage out of the trie, and produces no state-gas refill. Satin's destruction table
/// gives back the parts the transaction made: the account was absent at the transaction's start,
/// so its leaf's state gas comes back to the destroying frame at the price it was charged, priced
/// in the created address's bucket, and the transaction keeps none, at the minimum multiplier and
/// above it.
#[test]
fn test_a_created_account_selfdestructed_in_the_same_transaction_gives_its_state_gas_back() {
    // A charge that costs nothing is the same number whether or not it is given back.
    if state_is_free() {
        return;
    }
    // `ADDRESS SELFDESTRUCT`: the init code destroys the account being created. The beneficiary
    // is that account, which the creation itself just added, so no second account is charged.
    let init = selfdestruct_to_self();
    let created = CONTRACT.create(0);

    assert_eq!(
        net_state_at(
            [1, 2, 8],
            move |envs, m| crowded_account(envs, created, m),
            || (db(create_with(&init).stop().build()), call_contract()),
        ),
        [0, 0, 0],
        "the created account's state gas comes back at the price it was charged",
    );

    let envs = crowded_account(minimal_envs(), created, 8);
    let outcome = run(db(create_with(&init).stop().build()), envs.clone(), call_contract());
    assert_eq!(outcome.gas.state, 0);
    assert_eq!(
        envs.bucket_queries(account_bucket(created)),
        1,
        "one capacity read priced both the charge and the give-back",
    );
    assert!(
        outcome.state.get(&created).is_some_and(|account| account.is_selfdestructed()),
        "the same-transaction destruction removes the account from the trie",
    );
}

/// A contract created in the transaction and destroyed by a later call gives back every state
/// charge the transaction made for it, each at the price it was charged in its own bucket: the
/// account leaf, the slot its init code wrote and the code it deposited, the leaf and the code in
/// the account's bucket and the slot in the slot's. The transaction keeps none of them, at the
/// minimum multiplier and above it, with either bucket crowded, and each bucket is read once.
#[test]
fn test_a_contract_destroyed_after_its_creation_gives_back_its_leaf_slot_and_code() {
    use revm::bytecode::opcode::{ADDRESS, DUP6, GAS, POP, SELFDESTRUCT};
    if state_is_free() {
        return;
    }
    let init = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .return_with_data([ADDRESS, SELFDESTRUCT])
        .build();
    let created = CONTRACT.create(0);
    // The creation leaves the created address on the stack, and a call to it destroys it.
    let code = || {
        create_with(&init)
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0, DUP6, GAS, CALL, POP, POP])
            .stop()
            .build()
    };
    type Crowd = fn(SaltEnvs, Address, u64) -> SaltEnvs;
    let crowds: [(&str, Crowd); 2] = [
        ("the account's bucket", crowded_account),
        ("the slot's bucket", |envs, address, m| crowded_slot(envs, address, U256::from(SLOT), m)),
    ];
    for (crowded, crowd) in crowds {
        assert_eq!(
            net_state_at(
                [1, 2, 8],
                move |envs, m| crowd(envs, created, m),
                || { (db(code()), call_contract()) }
            ),
            [0, 0, 0],
            "{crowded}: every part comes back at the price it was charged",
        );
    }

    let envs =
        crowded_slot(crowded_account(minimal_envs(), created, 8), created, U256::from(SLOT), 8);
    let outcome = run(db(code()), envs.clone(), call_contract());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(outcome.state[&created].is_selfdestructed(), "the call destroyed the contract");
    assert_eq!(outcome.gas.state, 0);
    for (bucket, read) in [
        (account_bucket(created), "the account's"),
        (slot_bucket(created, U256::from(SLOT)), "the slot's"),
    ] {
        assert_eq!(
            envs.bucket_queries(bucket),
            1,
            "{read} bucket priced its charges and give-backs"
        );
    }
}

/// Init code that self-destructs the account being created: `ADDRESS SELFDESTRUCT`.
fn selfdestruct_to_self() -> Vec<u8> {
    use revm::bytecode::opcode::{ADDRESS, SELFDESTRUCT};
    vec![ADDRESS, SELFDESTRUCT]
}
