//! SALT pricing: the shared setup of the SALT tests, and what one state gas charge costs.
//!
//! Every EIP-8037 state gas charge is priced `schedule entry x m`, where `m` is the capacity of
//! the SALT bucket the charge lands in, counted in minimum buckets. This module pins that on
//! each of the sites a charge is made at, by running the same probe over three environments —
//! one where the bucket is at the minimum, one twice as large, one eight times as large — and
//! reading the ledgers the transaction reports:
//!
//! - the state ledger is the schedule's entry times `m`, and
//! - the regular ledger does not move: `m` scales the state dimension and nothing else.
//!
//! The refund side is in `salt_refund`, the failed-lookup side in `salt_failure`.

use alloy_eips::{
    eip4788::SYSTEM_ADDRESS,
    eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    satin_gas_params,
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    BucketId, ExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionError, MegaTransactionOutcome, SaltEnv, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, PUSH0, RETURN},
    context::{result::EVMError, tx::TxEnvBuilder, TxEnv},
    context_interface::cfg::GasId,
};

use crate::common::{block, runs_at_measurement_prices};

/// The sender of every probe.
pub(crate) const CALLER: Address = address!("0000000000000000000000000000000000c00000");
/// The contract a probe's code runs in.
pub(crate) const CONTRACT: Address = address!("0000000000000000000000000000000000c00001");
/// An account that does not exist, so reaching it with value creates it.
pub(crate) const EMPTY: Address = address!("0000000000000000000000000000000000c00002");
/// An authority that does not exist, so an authorization on it creates it.
pub(crate) const AUTHORITY: Address = address!("0000000000000000000000000000000000c00003");
/// The contract an authorization delegates to.
pub(crate) const DELEGATE: Address = address!("0000000000000000000000000000000000c00004");

/// Plenty for every probe, and below the execution cap, so the reservoir is empty and each state
/// charge spills onto the regular budget rather than being absorbed.
pub(crate) const GAS_LIMIT: u64 = 30_000_000;

/// The external environments of a SALT test. The error type is a string so a test can name the
/// failure a broken SALT backend reports.
pub(crate) type SaltEnvs = TestExternalEnvs<String>;

/// Every bucket at the minimum capacity, which is `m = 1` everywhere.
pub(crate) fn minimal_envs() -> SaltEnvs {
    TestExternalEnvs::new()
}

/// The bucket an account's own state lives in.
pub(crate) fn account_bucket(account: Address) -> BucketId {
    <SaltEnvs as SaltEnv>::bucket_id_for_account(account)
}

/// The bucket a storage slot lives in.
pub(crate) fn slot_bucket(address: Address, key: U256) -> BucketId {
    <SaltEnvs as SaltEnv>::bucket_id_for_slot(address, key)
}

/// The capacity a bucket has at multiplier `m`.
pub(crate) fn capacity(m: u64) -> u64 {
    MIN_BUCKET_SIZE as u64 * m
}

/// `envs` with the bucket of `account`'s own state at multiplier `m`.
pub(crate) fn crowded_account(envs: SaltEnvs, account: Address, m: u64) -> SaltEnvs {
    envs.with_bucket_capacity(account_bucket(account), capacity(m))
}

/// `envs` with the bucket of the slot `key` of `address` at multiplier `m`.
pub(crate) fn crowded_slot(envs: SaltEnvs, address: Address, key: U256, m: u64) -> SaltEnvs {
    envs.with_bucket_capacity(slot_bucket(address, key), capacity(m))
}

/// A Satin context over `db` reading `envs`, with zero L1 fees.
pub(crate) fn salt_context(
    db: MemoryDatabase,
    envs: SaltEnvs,
) -> MegaContext<MemoryDatabase, SaltEnvs> {
    MegaContext::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs { salt_env: envs.clone(), oracle_env: envs },
    )
    .with_block(block())
    .with_chain(zero_fee_l1_block_info())
}

/// The error a SALT test's transaction can fail with.
pub(crate) type SaltError = EVMError<core::convert::Infallible, MegaTransactionError>;

/// Runs `tx` on a fresh EVM over `db` reading `envs`, without committing.
pub(crate) fn try_run(
    db: MemoryDatabase,
    envs: SaltEnvs,
    tx: MegaTransaction,
) -> Result<MegaTransactionOutcome, SaltError> {
    MegaEvm::new(salt_context(db, envs)).execute_transaction(tx)
}

/// Runs `tx` and requires it to succeed.
pub(crate) fn run(
    db: MemoryDatabase,
    envs: SaltEnvs,
    tx: MegaTransaction,
) -> MegaTransactionOutcome {
    let outcome = try_run(db, envs, tx).expect("the probe is a valid transaction");
    assert!(outcome.result.is_success(), "the probe must succeed: {:?}", outcome.result);
    outcome
}

/// A funded sender and a contract running `code` with a balance of its own, so a probe's inner
/// `CALL` can carry value.
pub(crate) fn db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CONTRACT, U256::from(10u64.pow(9)))
        .account_code(CONTRACT, code)
}

/// A zero-value call from `CALLER` to `CONTRACT`.
pub(crate) fn call_contract() -> MegaTransaction {
    tx(TxKind::Call(CONTRACT), Bytes::new(), U256::ZERO)
}

/// A transaction from `CALLER`.
pub(crate) fn tx(kind: TxKind, data: Bytes, value: U256) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind,
        data,
        value,
        gas_limit: GAS_LIMIT,
        ..Default::default()
    }))
}

/// A type-4 transaction from `CALLER` to `to`, delegating `AUTHORITY` to `DELEGATE`.
pub(crate) fn authorization_tx(to: Address) -> MegaTransaction {
    OpTx(op_transaction(
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(to)
            .chain_id(Some(1))
            .gas_limit(GAS_LIMIT)
            .authorization_list_recovered(vec![RecoveredAuthorization::new_unchecked(
                Authorization { chain_id: U256::ZERO, address: DELEGATE, nonce: 0 },
                RecoveredAuthority::Valid(AUTHORITY),
            )])
            .build_fill(),
    ))
}

/// `CALL(gas, target, value, 0, 0, 0, 0)`.
pub(crate) fn value_call(target: Address) -> BytecodeBuilder {
    BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(1u64)
        .push_address(target)
        .push_number(1_000_000u64)
        .append(CALL)
}

/// `CREATE(0, 0, len)` of an init code already in memory at offset 0.
pub(crate) fn create_empty_contract() -> BytecodeBuilder {
    // Init code `PUSH0 PUSH0 RETURN`: deploys zero bytes of runtime code.
    let init: [u8; 3] = [PUSH0, PUSH0, RETURN];
    BytecodeBuilder::default()
        .mstore(0, init)
        .push_number(init.len() as u64)
        .push_number(32u64 - init.len() as u64)
        .push_number(0u64)
        .append(CREATE)
}

/// One state gas entry of the Satin schedule.
pub(crate) fn entry(id: GasId) -> u64 {
    satin_gas_params().get(id)
}

/// What a probe spent on state gas at each of the three multipliers, with the regular ledger
/// checked to be the same at all three: `m` scales the state dimension and nothing else.
///
/// `probe` builds the transaction and the database; `crowd` puts the bucket the probe charges in
/// at multiplier `m`.
pub(crate) fn state_gas_at(
    m: u64,
    crowd: impl Fn(SaltEnvs, u64) -> SaltEnvs,
    probe: impl Fn() -> (MemoryDatabase, MegaTransaction),
) -> (u64, u64) {
    let (db, tx) = probe();
    let outcome = run(db, crowd(minimal_envs(), m), tx);
    (outcome.gas.state, outcome.gas.regular)
}

/// Pins one charge site: the state ledger is `unit_price x units x m` at every multiplier, and
/// the regular ledger does not move.
pub(crate) fn assert_scales(
    site: &str,
    unscaled: u64,
    crowd: impl Fn(SaltEnvs, u64) -> SaltEnvs + Copy,
    probe: impl Fn() -> (MemoryDatabase, MegaTransaction) + Copy,
) {
    let (state_at_one, regular_at_one) = state_gas_at(1, crowd, probe);
    assert_eq!(state_at_one, unscaled, "{site}: the minimum bucket pays the schedule's entry");
    for m in [2, 8] {
        let (state, regular) = state_gas_at(m, crowd, probe);
        assert_eq!(state, unscaled * m, "{site}: state gas at m = {m}");
        assert_eq!(regular, regular_at_one, "{site}: regular gas must not scale with m");
    }
}

/* The six sites a state gas charge is made at. */

/// `SSTORE` onto a slot that was zero: the slot's own bucket prices it.
#[test]
fn test_the_sstore_set_charge_scales_with_the_slot_s_bucket() {
    const SLOT: u64 = 7;
    assert_scales(
        "SSTORE set",
        entry(GasId::sstore_set_state_gas()),
        |envs, m| crowded_slot(envs, CONTRACT, U256::from(SLOT), m),
        || {
            let code =
                BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
            (db(code), call_contract())
        },
    );
}

/// A `CALL` carrying value to an account that does not exist: the account's bucket prices it.
#[test]
fn test_the_new_account_charge_of_a_call_scales_with_the_account_s_bucket() {
    assert_scales(
        "CALL to a new account",
        entry(GasId::new_account_state_gas()),
        |envs, m| crowded_account(envs, EMPTY, m),
        || (db(value_call(EMPTY).stop().build()), call_contract()),
    );
}

/// `CREATE`: the bucket of the address it deploys to prices the account it adds.
#[test]
fn test_the_create_charge_scales_with_the_created_address_s_bucket() {
    let created = CONTRACT.create(0);
    assert_scales(
        "CREATE",
        entry(GasId::create_state_gas()),
        move |envs, m| crowded_account(envs, created, m),
        || (db(create_empty_contract().stop().build()), call_contract()),
    );
}

/// The EIP-2780 runtime phase charges the transaction's own recipient before the first frame,
/// and reads the recipient's bucket to price it.
#[test]
fn test_the_top_level_recipient_charge_scales_with_the_recipient_s_bucket() {
    assert_scales(
        "the transaction's recipient",
        entry(GasId::new_account_state_gas()),
        |envs, m| crowded_account(envs, EMPTY, m),
        || (db(Bytes::new()), tx(TxKind::Call(EMPTY), Bytes::new(), U256::from(1))),
    );
}

/// The same phase charges a creation transaction's target, in the target's own bucket.
#[test]
fn test_the_create_transaction_target_charge_scales_with_its_bucket() {
    let created = CALLER.create(0);
    assert_scales(
        "a creation transaction's target",
        entry(GasId::create_state_gas()),
        move |envs, m| crowded_account(envs, created, m),
        || {
            let init = Bytes::from_static(&[PUSH0, PUSH0, RETURN]);
            (db(Bytes::new()), tx(TxKind::Create, init, U256::ZERO))
        },
    );
}

/// An EIP-7702 authorization on an authority that does not exist pays for the account leaf it
/// adds and for the delegation bytes it writes, both in the authority's bucket.
#[test]
fn test_the_eip7702_authority_charges_scale_with_the_authority_s_bucket() {
    assert_scales(
        "an EIP-7702 authority",
        entry(GasId::new_account_state_gas()) + entry(GasId::tx_eip7702_state_gas_bytecode()),
        |envs, m| crowded_account(envs, AUTHORITY, m),
        || (db(Bytes::new()), authorization_tx(CONTRACT)),
    );
}

/// Deployed code is charged per byte, in the bucket of the address it is deployed to — the same
/// bucket that priced the creation itself.
#[test]
fn test_the_code_deposit_charge_scales_with_the_deployed_address_s_bucket() {
    const DEPLOYED: u64 = 32;
    let created = CALLER.create(0);
    assert_scales(
        "a code deposit",
        entry(GasId::create_state_gas()) + entry(GasId::code_deposit_state_gas()) * DEPLOYED,
        move |envs, m| crowded_account(envs, created, m),
        || {
            let init = BytecodeBuilder::default()
                .push_number(DEPLOYED)
                .append_many([PUSH0, RETURN])
                .build();
            (db(Bytes::new()), tx(TxKind::Create, init, U256::ZERO))
        },
    );
}

/// The numbers the sites above pay at the minimum bucket, written out: the schedule's state
/// entries at `MegaETH`'s cost per state byte.
#[test]
fn test_the_minimum_bucket_pays_the_spec_s_own_state_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(entry(GasId::sstore_set_state_gas()), 97_920);
    assert_eq!(entry(GasId::new_account_state_gas()), 183_600);
    assert_eq!(entry(GasId::create_state_gas()), 183_600);
    assert_eq!(entry(GasId::code_deposit_state_gas()), 1_530);
    assert_eq!(entry(GasId::tx_eip7702_state_gas_bytecode()), 35_190);
}

/* What the multiplier does not touch. */

/// A crowded bucket somewhere else changes nothing: the charge reads the bucket of the account
/// or slot it lands on, and no other.
#[test]
fn test_a_crowded_bucket_elsewhere_does_not_change_the_price() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let unscaled = run(db(code.clone()), minimal_envs(), call_contract()).gas;

    // Crowd the contract's own account bucket, another slot of the same contract, and an
    // unrelated account — none of them is the site this charge lands on.
    let envs = crowded_account(minimal_envs(), CONTRACT, 64);
    let envs = crowded_slot(envs, CONTRACT, U256::from(SLOT + 1), 64);
    let envs = crowded_account(envs, EMPTY, 64);
    let elsewhere = run(db(code), envs, call_contract()).gas;

    assert_eq!(elsewhere, unscaled, "only the charge's own bucket prices it");
}

/// The engine reads a bucket once per transaction: a program writing many slots of one bucket
/// asks the SALT environment for its capacity a single time.
#[test]
fn test_a_bucket_is_read_once_per_transaction() {
    const SLOTS: u64 = 16;
    // Put every slot this program writes in one bucket, so one capacity query serves them all.
    let mut code = BytecodeBuilder::default();
    let mut envs = minimal_envs();
    for slot in 0..SLOTS {
        code = code.sstore(U256::from(slot), U256::from(1));
        envs = envs.with_bucket_capacity(slot_bucket(CONTRACT, U256::from(slot)), capacity(4));
    }
    let buckets: Vec<BucketId> =
        (0..SLOTS).map(|slot| slot_bucket(CONTRACT, U256::from(slot))).collect();

    let mut evm = MegaEvm::new(salt_context(db(code.stop().build()), envs.clone()));
    let outcome = evm.execute_transaction(call_contract()).expect("the probe is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    assert_eq!(
        outcome.gas.state,
        entry(GasId::sstore_set_state_gas()) * 4 * SLOTS,
        "every slot paid the crowded price",
    );
    for bucket in &buckets {
        assert_eq!(envs.bucket_queries(*bucket), 1, "bucket {bucket} was read more than once");
    }
    assert_eq!(
        envs.total_bucket_queries(),
        buckets.iter().copied().collect::<std::collections::BTreeSet<_>>().len() as u32,
        "one query per distinct bucket, whatever the number of charges",
    );
}

/// A multiplier large enough to overflow the product saturates instead of wrapping: an absurd
/// capacity makes a charge unaffordable, never free.
#[test]
fn test_an_absurd_capacity_saturates_rather_than_wrapping() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs =
        minimal_envs().with_bucket_capacity(slot_bucket(CONTRACT, U256::from(SLOT)), u64::MAX);

    let outcome = try_run(db(code), envs, call_contract()).expect("the transaction is valid");
    assert!(
        matches!(outcome.result, revm::context::result::ExecutionResult::Halt { .. }),
        "an unaffordable charge halts: {:?}",
        outcome.result,
    );
}

/* The system-origin rule. */

/// The pre-block system calls and every other system call price at the minimum bucket, whatever
/// the bucket they write into actually holds: a state change the protocol mandates cannot be
/// priced out by the growth of a region it does not control.
#[test]
fn test_a_system_call_prices_at_the_minimum_bucket() {
    use alloy_evm::Evm;

    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);

    let mut evm = MegaEvm::new(salt_context(db(code), envs.clone()));
    let result = evm
        .transact_system_call(SYSTEM_ADDRESS, CONTRACT, Bytes::new())
        .expect("the system call is valid");
    assert!(result.result.is_success(), "{:?}", result.result);

    assert_eq!(
        result.result.gas().state_gas_spent_final(),
        entry(GasId::sstore_set_state_gas()),
        "a system call pays the schedule's own entry",
    );
    assert_eq!(envs.total_bucket_queries(), 0, "and never reads the SALT environment");
    assert!(evm.ctx().is_system_originated());
}

/// A system call is system-originated whatever caller it names: it is the protocol running, not
/// a transaction anybody sent.
#[test]
fn test_a_system_call_with_another_caller_is_still_system_originated() {
    use alloy_evm::Evm;

    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);

    let mut evm = MegaEvm::new(salt_context(db(code), envs.clone()));
    let result =
        evm.transact_system_call(CALLER, CONTRACT, Bytes::new()).expect("the system call is valid");
    assert!(result.result.is_success(), "{:?}", result.result);

    assert_eq!(result.result.gas().state_gas_spent_final(), entry(GasId::sstore_set_state_gas()));
    assert_eq!(envs.total_bucket_queries(), 0);
}

/// A transaction the system address sends prices at the minimum bucket too: that address has no
/// key, so only the protocol's own pre-block helpers ever set it as caller.
#[test]
fn test_a_transaction_from_the_system_address_prices_at_the_minimum_bucket() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);
    let tx = OpTx(op_transaction(TxEnv {
        caller: SYSTEM_ADDRESS,
        kind: TxKind::Call(CONTRACT),
        gas_limit: GAS_LIMIT,
        ..Default::default()
    }));

    let outcome = run(db(code), envs.clone(), tx);
    assert_eq!(outcome.gas.state, entry(GasId::sstore_set_state_gas()));
    assert_eq!(envs.total_bucket_queries(), 0);
}

/// The control arm of the rule: the very same write, sent by a user, pays the crowded price.
/// Without this the exemption above could be passing for the wrong reason.
#[test]
fn test_a_user_transaction_into_the_same_bucket_pays_the_crowded_price() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);

    let mut evm = MegaEvm::new(salt_context(db(code), envs.clone()));
    let outcome = evm.execute_transaction(call_contract()).expect("the probe is valid");

    assert_eq!(outcome.gas.state, entry(GasId::sstore_set_state_gas()) * 8);
    assert_eq!(envs.total_bucket_queries(), 1);
    assert!(!evm.ctx().is_system_originated());
}

/// The exemption belongs to one transaction: a user transaction run on the same EVM right after
/// a system call is priced by its own bucket again.
#[test]
fn test_the_system_exemption_does_not_leak_into_the_next_transaction() {
    use alloy_evm::Evm;

    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);
    let mut evm = MegaEvm::new(salt_context(db(code), envs));

    let system = evm
        .transact_system_call(SYSTEM_ADDRESS, CONTRACT, Bytes::new())
        .expect("the system call is valid");
    assert_eq!(system.result.gas().state_gas_spent_final(), entry(GasId::sstore_set_state_gas()));
    assert!(evm.ctx().is_system_originated());

    let user = evm.execute_transaction(call_contract()).expect("the probe is valid");
    assert!(!evm.ctx().is_system_originated(), "the next transaction is nobody's system call");
    assert_eq!(user.gas.state, entry(GasId::sstore_set_state_gas()) * 8);
}

/// With every bucket at the minimum the engine charges exactly what it charges without a SALT
/// environment at all, so turning SALT pricing on moves nothing until a bucket grows.
#[test]
fn test_the_minimum_bucket_matches_the_engine_without_a_salt_environment() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .append_many(value_call(EMPTY).build_vec())
        .stop()
        .build();

    let with_salt = run(db(code.clone()), minimal_envs(), call_contract()).gas;
    let without_salt = MegaEvm::new(
        MegaContext::new(db(code), MegaSpecId::SATIN)
            .with_block(block())
            .with_chain(zero_fee_l1_block_info()),
    )
    .execute_transaction(call_contract())
    .expect("the probe is valid")
    .gas;

    assert_eq!(with_salt, without_salt);
}
