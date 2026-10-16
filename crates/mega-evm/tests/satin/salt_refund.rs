//! SALT pricing: giving a state gas charge back at the price it was made at.
//!
//! A state charge is undone in two shapes, and neither of them reads a second table.
//!
//! One shape names the charge and asks the hook for its price again: the same opcode taking its
//! own write back (`SSTORE` 0 -> x -> 0), the upfront charge a caller made for a call or a
//! creation that did not add the leaf after all, the transaction-level refund, and the
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
    settle_frame_result, synthetic_frame_result,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaTransaction, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, PUSH0, RETURN, REVERT},
    context_interface::{
        cfg::{gas::GasTracker, GasId, StateGasCharge, StateGasSite},
        Host,
    },
    interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, CreateInputs, CreateScheme, FrameInput,
        InstructionResult,
    },
};

use crate::salt::{
    account_bucket, authorization_tx, call_contract, capacity, create_with, crowded_account,
    crowded_slot, db, entry, minimal_envs, run, salt_context, selfdestruct_to, slot_bucket,
    try_run, tx, SaltEnvs, AUTHORITY, CALLER, CONTRACT, EMPTY, GAS_LIMIT,
};

/// The contract a probe's inner frame runs in.
const SUB: Address = alloy_primitives::address!("0000000000000000000000000000000000c00005");

/// The slot every probe that writes storage uses.
const SLOT: u64 = 7;

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
            .push_number(2_000_000u64)
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
    let probe = || {
        let sub = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(SUB)
            .push_number(2_000_000u64)
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
                .push_number(2_000_000u64)
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

/// A crowded bucket cannot make a transaction owe more than it charged: the refill of a charge
/// is the charge, so a restore never leaves the state ledger negative or the reservoir richer
/// than the sender's budget.
#[test]
fn test_a_restore_never_gives_back_more_than_it_charged() {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .sstore(U256::from(SLOT), U256::ZERO)
        .stop()
        .build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);

    let outcome: MegaTransactionOutcome =
        MegaEvm::new(salt_context(db(code), envs)).execute_transaction(call_contract()).unwrap();

    assert_eq!(outcome.gas.state, 0, "the state ledger nets to zero, not below it");
    assert!(
        outcome.gas.reservoir_remaining <= GAS_LIMIT,
        "a refill cannot put more in the reservoir than the transaction brought",
    );
    assert!(outcome.gas.gas_used > 0 && outcome.gas.gas_used <= GAS_LIMIT);
}

/// The whole matrix at one multiplier, side by side, so the three refund classes are readable in
/// one place: a rollback gives the charge back, an authorization keeps it, and a bucket is read
/// once whatever happens to the charge afterwards.
#[test]
fn test_the_refund_classes_side_by_side() {
    let set = entry(GasId::sstore_set_state_gas());
    let authority_charges =
        entry(GasId::new_account_state_gas()) + entry(GasId::tx_eip7702_state_gas_bytecode());

    let kept = run(
        db(BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build()),
        crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8),
        call_contract(),
    );
    assert_eq!(kept.gas.state, set * 8, "a write that stays pays the crowded price");

    let rolled_back = try_run(
        db(BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .revert_with_data([0xaa])
            .build()),
        crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8),
        call_contract(),
    )
    .expect("the transaction is valid");
    assert_eq!(rolled_back.gas.state, 0, "a write that is rolled back pays nothing");

    let authorized = try_run(
        db(BytecodeBuilder::default().revert_with_data([0xaa]).build()),
        crowded_account(minimal_envs(), AUTHORITY, 8),
        authorization_tx(CONTRACT),
    )
    .expect("the transaction is valid");
    assert_eq!(
        authorized.gas.state,
        authority_charges * 8,
        "an applied authorization pays even though the frame reverted",
    );
}

/// The capacity a bucket is read at is fixed for the transaction: one read serves the charge and
/// the refill, so the two cannot disagree even if the environment's answer would.
#[test]
fn test_the_capacity_behind_a_charge_and_its_refill_is_read_once() {
    let bucket = slot_bucket(CONTRACT, U256::from(SLOT));
    let envs = minimal_envs().with_bucket_capacity(bucket, capacity(8));
    let code = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .sstore(U256::from(SLOT), U256::ZERO)
        .sstore(U256::from(SLOT), U256::from(2))
        .sstore(U256::from(SLOT), U256::ZERO)
        .stop()
        .build();

    let outcome = run(db(code), envs.clone(), call_contract());

    assert_eq!(outcome.gas.state, 0, "every set was restored");
    assert_eq!(envs.bucket_queries(bucket), 1, "two charges and two refills, one capacity read");
}

/* The refund route that runs without a frame. */

/// The address a synthetic frame would have added a leaf at.
const SYNTHETIC_TARGET: Address = address!("0000000000000000000000000000000000c00008");

/// The regular gas a caller forwards to the frame the probes below answer without running it.
const FORWARDED: u64 = 9_000;

/// The caller's own regular gas limit in those probes, wide enough for a crowded charge to spill
/// onto it in full.
const CALLER_LIMIT: u64 = 5_000_000;

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
/// reservoir the caller has left, and settles a synthetic failure for it through the public
/// helper — the route a mechanism takes when it answers a frame rather than running it.
fn settle_a_synthetic_failure(
    envs: &SaltEnvs,
    reservoir: u64,
    charge: StateGasCharge,
    input: impl FnOnce(u64) -> FrameInput,
) -> Settled {
    let mut ctx = salt_context(db(Bytes::new()), envs.clone());
    let price = ctx.state_gas_charge(charge).expect("the bucket is readable");

    let mut caller = GasTracker::new(CALLER_LIMIT, CALLER_LIMIT - FORWARDED, reservoir);
    assert!(caller.record_state_cost(price), "the caller can pay the crowded charge");
    let spilled = caller.state_gas_spilled();

    let mut result =
        synthetic_frame_result(&input(caller.reservoir()), InstructionResult::Revert, Bytes::new());
    settle_frame_result::<_, revm::context::result::EVMError<core::convert::Infallible>>(
        &mut ctx,
        &mut caller,
        &mut result,
    )
    .expect("the refund is priced through the same hook");

    Settled { caller, price, spilled }
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
    assert_eq!(caller.remaining(), CALLER_LIMIT, "{site}: the regular pool comes back whole");
    assert_eq!(caller.reservoir(), reservoir, "{site}: and the reservoir is where it started");
    assert_eq!(caller.state_gas_spent(), 0, "{site}: the state ledger nets zero");
    assert_eq!(caller.state_gas_spilled(), 0, "{site}: nothing is left spilled");
    assert_eq!(
        envs.bucket_queries(account_bucket(SYNTHETIC_TARGET)),
        1,
        "{site}: one capacity read priced the charge and the refund",
    );
}

/// A synthetic `CALL` failure settled through the public helper gives back a SALT-priced upfront
/// charge, both when the reservoir paid for it and when it spilled onto regular gas.
#[test]
fn test_a_synthetic_call_failure_refunds_the_crowded_upfront_charge() {
    let crowded = entry(GasId::new_account_state_gas()) * SYNTHETIC_MULTIPLIER;
    let charge = StateGasCharge::one(
        GasId::new_account_state_gas(),
        StateGasSite::account(SYNTHETIC_TARGET),
    );

    // The reservoir covers the whole charge.
    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, CALLER_LIMIT, charge, charged_call_input);
    assert_eq!(settled.price, crowded, "the charge was priced at the crowded bucket");
    assert_eq!(settled.spilled, 0, "and the reservoir paid for all of it");
    assert_restores_both_pools("a call from the reservoir", &envs, CALLER_LIMIT, &settled);

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
    let crowded = entry(GasId::create_state_gas()) * SYNTHETIC_MULTIPLIER;
    let charge =
        StateGasCharge::one(GasId::create_state_gas(), StateGasSite::account(SYNTHETIC_TARGET));

    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, CALLER_LIMIT, charge, charged_create_input);
    assert_eq!(settled.price, crowded, "the charge was priced at the crowded bucket");
    assert_eq!(settled.spilled, 0);
    assert_restores_both_pools("a creation from the reservoir", &envs, CALLER_LIMIT, &settled);

    let reservoir = crowded / 4;
    let envs = crowded_account(minimal_envs(), SYNTHETIC_TARGET, SYNTHETIC_MULTIPLIER);
    let settled = settle_a_synthetic_failure(&envs, reservoir, charge, charged_create_input);
    assert_eq!(settled.price, crowded);
    assert_eq!(settled.spilled, crowded - reservoir);
    assert_restores_both_pools("a creation that spilled", &envs, reservoir, &settled);
}

/* A deposit charge that is made and then rolled back. */

/// How many bytes of runtime code the creation below deploys when its deposit is meant to fail.
/// At the crowded price a byte costs `code_deposit_state_gas x 8`, so this many bytes is far
/// beyond what the transaction brought.
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
    let created = CONTRACT.create(0);
    let crowd = |envs| {
        let envs = crowded_account(envs, created, SYNTHETIC_MULTIPLIER);
        crowded_slot(envs, created, U256::from(SLOT), SYNTHETIC_MULTIPLIER)
    };

    let envs = crowd(minimal_envs());
    let outcome = run(creation_depositing(UNAFFORDABLE_CODE), envs.clone(), call_contract());
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
