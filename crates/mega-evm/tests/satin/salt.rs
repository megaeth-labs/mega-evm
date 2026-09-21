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
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    satin_gas_params,
    system::{
        IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
        SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    BucketId, ExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionError, MegaTransactionOutcome, SaltEnv, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use mega_system_contracts::sequencer_registry::storage_slots::CURRENT_SYSTEM_ADDRESS;
use revm::{
    bytecode::opcode::{CALL, CREATE, CREATE2, PUSH0, RETURN, SELFDESTRUCT},
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
pub(crate) fn salt_context<DB: revm::Database>(
    db: DB,
    envs: SaltEnvs,
) -> MegaContext<DB, SaltEnvs> {
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

/// A transaction from `CALLER`, at [`GAS_LIMIT`].
pub(crate) fn tx(kind: TxKind, data: Bytes, value: U256) -> MegaTransaction {
    tx_with_gas(kind, data, value, GAS_LIMIT)
}

/// A transaction from `CALLER` at a gas limit of its own, for a probe whose charge does not fit
/// in [`GAS_LIMIT`].
pub(crate) fn tx_with_gas(
    kind: TxKind,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind,
        data,
        value,
        gas_limit,
        ..Default::default()
    }))
}

/// A deposit transaction from `CALLER` to `to`, at [`GAS_LIMIT`].
///
/// A non-zero source hash is what makes op-revm classify a transaction as a deposit, and
/// `is_system_transaction` is left off: this is the deposit shape a user can produce, not the
/// sequencer's own system transaction.
pub(crate) fn deposit_tx(to: Address) -> MegaTransaction {
    let mut tx = op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        gas_limit: GAS_LIMIT,
        ..Default::default()
    });
    tx.deposit.source_hash = B256::from([0x42; 32]);
    tx.deposit.is_system_transaction = false;
    OpTx(tx)
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

/// `SELFDESTRUCT(beneficiary)`: the running contract sends its balance to `beneficiary` and ends
/// its frame.
pub(crate) fn selfdestruct_to(beneficiary: Address) -> BytecodeBuilder {
    BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT)
}

/// `CREATE(value = 0, offset = 0, size)` of `init_code`, written to memory first.
///
/// `mstore` right-pads to a whole word, so the init code sits at offset 0 and the `CREATE`
/// reads it from there.
pub(crate) fn create_with(init_code: &[u8]) -> BytecodeBuilder {
    BytecodeBuilder::default()
        .mstore(0, init_code)
        .push_number(init_code.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
}

/// A `CREATE` whose init code `PUSH0 PUSH0 RETURN` deploys zero bytes of runtime code.
pub(crate) fn create_empty_contract() -> BytecodeBuilder {
    create_with(&[PUSH0, PUSH0, RETURN])
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

/* The seven sites a state gas charge is made at. */

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

/// A `SELFDESTRUCT` moving a balance to an account that does not exist adds that account's leaf,
/// and the beneficiary's own bucket prices it — the same entry and the same site a value `CALL`
/// pays, reached from the opcode that empties an account rather than the one that funds it.
#[test]
fn test_the_selfdestruct_beneficiary_charge_scales_with_the_beneficiary_s_bucket() {
    assert_scales(
        "SELFDESTRUCT to a new account",
        entry(GasId::new_account_state_gas()),
        |envs, m| crowded_account(envs, EMPTY, m),
        || (db(selfdestruct_to(EMPTY).build()), call_contract()),
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

/* The sequencer's own system transaction. */

/// The Oracle slot the system transactions write. `setSlot` writes the raw slot number, so the
/// bucket a test crowds is the bucket the write lands in.
const ORACLE_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// The capacity the system-transaction arms crowd the Oracle's slot to.
const SYSTEM_TX_MULTIPLIER: u64 = 8;

/// A database holding the Oracle, the registry naming [`MEGA_SYSTEM_ADDRESS`] the current system
/// address — which is what lets an Oracle write from it through — and a user contract to send
/// the control arm's transaction to.
fn system_tx_db() -> MemoryDatabase {
    let user_code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(CONTRACT, user_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
        .account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            U256::from_be_slice(MEGA_SYSTEM_ADDRESS.as_slice()),
        )
}

/// The sequencer's own system transaction: a legacy call from the system address to the
/// whitelisted Oracle, writing `ORACLE_SLOT`, which the engine promotes to a deposit.
fn oracle_system_tx() -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        data: IOracle::setSlotCall { slot: ORACLE_SLOT, value: B256::with_last_byte(0xAB) }
            .abi_encode()
            .into(),
        chain_id: Some(1),
        gas_limit: GAS_LIMIT,
        ..Default::default()
    }))
}

/// `envs` with the Oracle's slot at multiplier `m`.
fn crowded_oracle_slot(m: u64) -> SaltEnvs {
    crowded_slot(minimal_envs(), ORACLE_CONTRACT_ADDRESS, ORACLE_SLOT, m)
}

/// The sequencer's system transaction prices at the minimum bucket: it pays the same state gas
/// whatever the capacity of the bucket it writes into, and never reads the SALT environment.
///
/// This is the arm the system-origin predicate gained when the system transaction's definition
/// landed: the engine promotes it to a deposit, and a promoted system transaction is the
/// protocol running, so the growth of the Oracle's region cannot price the protocol out of
/// maintaining it.
#[test]
fn test_the_promoted_system_transaction_prices_at_the_minimum_bucket() {
    let set = entry(GasId::sstore_set_state_gas());
    let mut spends = Vec::new();
    let mut receipts = Vec::new();
    for m in [1, SYSTEM_TX_MULTIPLIER] {
        let envs = crowded_oracle_slot(m);
        let mut evm = MegaEvm::new(salt_context(system_tx_db(), envs.clone()));
        let outcome = evm.execute_transaction(oracle_system_tx()).expect("the probe is valid");
        assert!(outcome.result.is_success(), "at m = {m}: {:?}", outcome.result);

        assert!(evm.ctx().is_system_originated(), "at m = {m}");
        assert_eq!(envs.total_bucket_queries(), 0, "at m = {m}: it reads no capacity at all");
        assert_eq!(
            written_oracle_slot(&outcome),
            U256::from_be_bytes(B256::with_last_byte(0xAB).0),
            "at m = {m}: the Oracle slot holds what the transaction wrote",
        );
        spends.push(outcome.gas.state);
        receipts.push(outcome.gas.gas_used);
    }

    assert_eq!(spends[0], spends[1], "the two capacities cost the same");
    assert_eq!(
        receipts[0], receipts[1],
        "and so does the whole receipt, not only its state ledger",
    );
    assert_eq!(
        spends[0],
        set + entry(GasId::new_account_state_gas()),
        "and both charges are the schedule's own entries: the Oracle's write, and the account \
         the promoted deposit creates for the system address",
    );
}

/// What [`ORACLE_SLOT`] holds in the state a probe produced.
fn written_oracle_slot(outcome: &MegaTransactionOutcome) -> U256 {
    outcome
        .state
        .get(&ORACLE_CONTRACT_ADDRESS)
        .expect("the Oracle is in the transaction's state")
        .storage
        .get(&ORACLE_SLOT)
        .expect("the write reached the slot")
        .present_value
}

/// The system-call entry point likewise: the same call priced across the same two capacities
/// costs the same, so a protocol-mandated write cannot be priced out either. This is the entry
/// point a block issues its pre-block calls through; driving a real block through it is a
/// block-execution test.
#[test]
fn test_the_system_call_entry_point_prices_the_same_at_any_capacity() {
    use alloy_evm::Evm;

    let mut spends = Vec::new();
    for m in [1, SYSTEM_TX_MULTIPLIER] {
        let envs = crowded_oracle_slot(m);
        let data = IOracle::setSlotCall { slot: ORACLE_SLOT, value: B256::with_last_byte(0xCD) }
            .abi_encode()
            .into();
        let result = MegaEvm::new(salt_context(system_tx_db(), envs.clone()))
            .transact_system_call(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, data)
            .expect("the system call is valid");
        assert!(result.result.is_success(), "at m = {m}: {:?}", result.result);

        assert_eq!(envs.total_bucket_queries(), 0, "at m = {m}");
        spends.push(result.result.gas().state_gas_spent_final());
    }

    assert_eq!(spends[0], spends[1], "the two capacities cost the same");
    assert_eq!(spends[0], entry(GasId::sstore_set_state_gas()));
}

/// The control arm: a user transaction in the same block, into a bucket crowded the same way,
/// still pays the crowded price. The exemption belongs to the protocol's own transactions, not
/// to the block they run in.
#[test]
fn test_a_user_transaction_in_the_same_block_still_pays_the_crowded_price() {
    let set = entry(GasId::sstore_set_state_gas());
    let envs = crowded_slot(
        crowded_oracle_slot(SYSTEM_TX_MULTIPLIER),
        CONTRACT,
        U256::ZERO,
        SYSTEM_TX_MULTIPLIER,
    );
    let mut evm = MegaEvm::new(salt_context(system_tx_db(), envs.clone()));

    let system = evm.execute_transaction(oracle_system_tx()).expect("the system probe is valid");
    assert!(system.result.is_success(), "{:?}", system.result);
    assert_eq!(
        system.gas.state,
        set + entry(GasId::new_account_state_gas()),
        "the system transaction is exempt",
    );
    assert!(evm.ctx().is_system_originated());

    let user = evm.execute_transaction(call_contract()).expect("the user probe is valid");
    assert!(user.result.is_success(), "{:?}", user.result);
    assert_eq!(user.gas.state, set * SYSTEM_TX_MULTIPLIER, "the user transaction pays the crowd");
    assert!(!evm.ctx().is_system_originated());
    assert_eq!(envs.total_bucket_queries(), 1, "only the user transaction read a capacity");
}

/// A deposit transaction is deliberately not system-originated, and pays the crowded price like
/// any other transaction a user sends. A deposit is a shape a user can produce, so matching it
/// here would be a way around the scaling.
#[test]
fn test_a_deposit_transaction_pays_the_crowded_price() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);

    let mut evm = MegaEvm::new(salt_context(db(code), envs.clone()));
    let tx = deposit_tx(CONTRACT);
    assert_ne!(tx.0.deposit.source_hash, B256::ZERO, "the probe must be a deposit");
    assert!(!tx.0.deposit.is_system_transaction, "and not a system deposit");

    let outcome = evm.execute_transaction(tx).expect("the probe is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    assert_eq!(outcome.gas.state, entry(GasId::sstore_set_state_gas()) * 8);
    assert_eq!(envs.total_bucket_queries(), 1, "the deposit read the bucket it writes into");
    assert!(!evm.ctx().is_system_originated());
}

/// And a deposit whose bucket cannot be read fails, exactly as a plain transaction does: not
/// being system-originated cuts both ways.
#[test]
fn test_an_unpriceable_deposit_transaction_fails() {
    const SLOT: u64 = 7;
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = minimal_envs()
        .with_failing_bucket(slot_bucket(CONTRACT, U256::from(SLOT)), "salt backend down".into());

    match try_run(db(code), envs, deposit_tx(CONTRACT)) {
        Err(EVMError::Custom(message)) => {
            assert!(message.contains("salt backend down"), "got {message:?}");
        }
        other => panic!("expected the recorded cause, got {other:?}"),
    }
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

/* What the multiplier has nothing to scale: the writes that add no state. */

/// Runs `probe` at the minimum bucket and at a crowded one and requires both to charge nothing
/// on the state ledger — a write that adds no state has nothing for `m` to multiply.
fn assert_charges_no_state_gas(
    site: &str,
    crowd: impl Fn(SaltEnvs, u64) -> SaltEnvs,
    probe: impl Fn() -> (MemoryDatabase, MegaTransaction) + Copy,
) {
    for m in [1, 8] {
        let (db, tx) = probe();
        let outcome = run(db, crowd(minimal_envs(), m), tx);
        assert_eq!(outcome.gas.state, 0, "{site} at m = {m}");
    }
}

/// Writing over a slot that already held a value adds no state, so it pays no state gas
/// however crowded its bucket is.
#[test]
fn test_writing_over_a_slot_charges_no_state_gas() {
    assert_charges_no_state_gas(
        "a reset",
        |envs, m| crowded_slot(envs, CONTRACT, U256::ZERO, m),
        || {
            let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(2)).stop().build();
            (db(code).account_storage(CONTRACT, U256::ZERO, U256::from(1)), call_contract())
        },
    );
}

/// Clearing a slot removes state rather than adding it, so it pays no state gas either.
#[test]
fn test_clearing_a_slot_charges_no_state_gas() {
    assert_charges_no_state_gas(
        "a clear",
        |envs, m| crowded_slot(envs, CONTRACT, U256::ZERO, m),
        || {
            let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).stop().build();
            (db(code).account_storage(CONTRACT, U256::ZERO, U256::from(1)), call_contract())
        },
    );
}

/// A slot is one leaf however many times the transaction writes it: the first write off zero
/// pays, the rest pay nothing.
#[test]
fn test_a_slot_written_twice_pays_for_one_leaf() {
    let set = entry(GasId::sstore_set_state_gas());
    assert_scales(
        "a slot written twice",
        set,
        |envs, m| crowded_slot(envs, CONTRACT, U256::ZERO, m),
        || {
            let code = BytecodeBuilder::default()
                .sstore(U256::ZERO, U256::from(1))
                .sstore(U256::ZERO, U256::from(2))
                .stop()
                .build();
            (db(code), call_contract())
        },
    );
}

/// Value reaching an account that already exists adds no leaf, at the top level and from a
/// frame alike.
#[test]
fn test_a_transfer_to_an_account_that_exists_charges_no_state_gas() {
    const FUNDED: Address = address!("0000000000000000000000000000000000c00006");

    assert_charges_no_state_gas(
        "a top-level transfer to an existing account",
        |envs, m| crowded_account(envs, FUNDED, m),
        || {
            let db = db(Bytes::new()).account_balance(FUNDED, U256::from(1));
            (db, tx(TxKind::Call(FUNDED), Bytes::new(), U256::from(1)))
        },
    );

    assert_charges_no_state_gas(
        "a transfer from a frame to an existing account",
        |envs, m| crowded_account(envs, FUNDED, m),
        || {
            let db = db(value_call(FUNDED).stop().build()).account_balance(FUNDED, U256::from(1));
            (db, call_contract())
        },
    );
}

/// A `SELFDESTRUCT` pays for a beneficiary's leaf only when it has a balance to move and the
/// beneficiary does not exist yet. Neither half alone adds state, so neither is charged, however
/// crowded the beneficiary's bucket is.
#[test]
fn test_a_selfdestruct_that_adds_no_account_charges_no_state_gas() {
    const FUNDED: Address = address!("0000000000000000000000000000000000c00007");

    assert_charges_no_state_gas(
        "a SELFDESTRUCT with nothing to move",
        |envs, m| crowded_account(envs, EMPTY, m),
        || {
            // The contract holds no balance, so the beneficiary receives nothing and is not
            // created.
            let db = MemoryDatabase::default()
                .account_balance(CALLER, U256::from(10u64.pow(18)))
                .account_code(CONTRACT, selfdestruct_to(EMPTY).build());
            (db, call_contract())
        },
    );

    assert_charges_no_state_gas(
        "a SELFDESTRUCT to an account that exists",
        |envs, m| crowded_account(envs, FUNDED, m),
        || {
            let db = db(selfdestruct_to(FUNDED).build()).account_balance(FUNDED, U256::from(1));
            (db, call_contract())
        },
    );
}

/* The remaining creation sites. */

/// `CREATE2` reaches its deployment address by hashing rather than by nonce, and is priced in
/// that address's bucket just as `CREATE` is.
#[test]
fn test_the_create2_charge_scales_with_the_created_address_s_bucket() {
    const SALT: U256 = U256::ZERO;
    let init: [u8; 3] = [PUSH0, PUSH0, RETURN];
    let created = CONTRACT.create2_from_code(SALT.to_be_bytes::<32>(), init);

    assert_scales(
        "CREATE2",
        entry(GasId::create_state_gas()),
        move |envs, m| crowded_account(envs, created, m),
        move || {
            let code = BytecodeBuilder::default()
                .mstore(0, init)
                .push_u256(SALT)
                .push_number(init.len() as u64)
                .push_number(0u64)
                .push_number(0u64)
                .append(CREATE2)
                .stop()
                .build();
            (db(code), call_contract())
        },
    );
}

/// An account leaf costs the same however it comes about: the entry a creation pays and the one
/// a value transfer pays are the same number, because they buy the same leaf.
#[test]
fn test_an_account_leaf_costs_the_same_however_it_is_added() {
    assert_eq!(entry(GasId::create_state_gas()), entry(GasId::new_account_state_gas()));

    let created = CONTRACT.create(0);
    let creation = run(
        db(create_empty_contract().stop().build()),
        crowded_account(minimal_envs(), created, 8),
        call_contract(),
    );
    let transfer = run(
        db(value_call(EMPTY).stop().build()),
        crowded_account(minimal_envs(), EMPTY, 8),
        call_contract(),
    );

    assert_eq!(creation.gas.state, transfer.gas.state);
}

/// A creation that also writes a slot pays for both leaves, each in the bucket it lands in: the
/// account in the created address's, the slot in its own.
#[test]
fn test_a_creation_that_writes_a_slot_pays_both_in_their_own_buckets() {
    const ACCOUNT_M: u64 = 2;
    const SLOT_M: u64 = 4;
    let created = CALLER.create(0);
    // Init code writing slot 0 of the contract being created, then deploying nothing.
    let init = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .append_many([PUSH0, PUSH0, RETURN])
        .build();

    let envs = crowded_account(minimal_envs(), created, ACCOUNT_M);
    let envs = crowded_slot(envs, created, U256::ZERO, SLOT_M);
    let outcome = run(db(Bytes::new()), envs, tx(TxKind::Create, init, U256::ZERO));

    assert_eq!(
        outcome.gas.state,
        entry(GasId::create_state_gas()) * ACCOUNT_M +
            entry(GasId::sstore_set_state_gas()) * SLOT_M,
    );
}

/// The multiplier is linear over the whole range a bucket can grow through, not just the small
/// ones the other probes use. The largest of these charges more than the execution cap allows
/// in regular gas, so the probe brings a budget that puts the excess in the state reservoir —
/// which is where a charge this size is meant to be paid from.
#[test]
fn test_the_multiplier_is_linear_over_a_wide_range() {
    const WIDE_GAS_LIMIT: u64 = 400_000_000;
    let set = entry(GasId::sstore_set_state_gas());
    let code = || BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();

    for m in [1, 2, 10, 1_000] {
        let envs = crowded_slot(minimal_envs(), CONTRACT, U256::ZERO, m);
        let probe = tx_with_gas(TxKind::Call(CONTRACT), Bytes::new(), U256::ZERO, WIDE_GAS_LIMIT);
        let outcome = run(db(code()), envs, probe);
        assert_eq!(outcome.gas.state, set * m, "at m = {m}");
    }
}
