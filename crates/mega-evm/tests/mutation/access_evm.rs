//! Value / behavioral tests that close surviving mutants in `crates/mega-evm/src/access/` and
//! the `crates/mega-evm/src/evm/mod.rs` accessor methods.
//!
//! The `access/*` modules carry inline `#[cfg(test)] mod tests`, but those unit tests do not kill
//! the relevant mutants: the mutation run builds the integration test targets, and (for
//! `VolatileDataAccess::as_u8`) the inline assertions are tautological — they compare
//! `converted.as_u8()` against `expected_flag.as_u8()`, so a `-> 0` / `-> 1` mutant changes both
//! sides identically and the equality still holds. These integration tests instead pin the exact
//! discriminant / boolean / option value through the crate's public API.
//!
//! Covered survivors:
//! * `access/volatile.rs:89` — `VolatileDataAccess::as_u8 -> 0` and `-> 1`. Killed by asserting the
//!   exact bit-position discriminant of several distinct variants (so neither constant matches all
//!   of them).
//! * `access/tracker.rs:131` — `has_accessed_beneficiary_balance -> true`. Killed by asserting it
//!   is `false` on a fresh tracker.
//! * `access/tracker.rs:104` — `get_volatile_data_info -> Some(Default::default())`. Killed by
//!   asserting it is `None` on a fresh (no-access) tracker; the mutant returns `Some(empty)`.
//! * `access/tracker.rs:180` — `disable_access` depth guard (`>=` vs `<`, and the match-guard
//!   replaced with `true` / `false`). Killed by re-disabling at a deeper / shallower depth and
//!   observing whether the shallower depth is retained.
//! * `evm/mod.rs:107` — `Debug for MegaEvm::fmt -> Ok(())`. Killed by asserting the formatted
//!   output contains the struct name and field; the mutant writes nothing.
//! * `evm/mod.rs:251` — `block_env_mut -> Box::leak(Box::new(Default::default()))`. Killed by
//!   mutating a field through `block_env_mut` and reading it back via `block_env_ref`; the mutant
//!   writes to a throwaway leaked env so the read shows the original value.
//! * `evm/mod.rs:379` — `get_accessed_bucket_ids -> vec![]` and `vec![Default::default()]`. Killed
//!   by executing an `SSTORE`-set transaction under `TestExternalEnvs` (non-zero bucket IDs) and
//!   asserting the returned IDs are non-empty and equal the predicted (non-zero) bucket ID.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, SaltEnv, TestExternalEnvs,
    VolatileDataAccess, VolatileDataAccessTracker, MIN_BUCKET_SIZE,
};
use revm::context::{BlockEnv, TxEnv};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
/// Block beneficiary (coinbase) used by the `disable_beneficiary` tests.
const BENEFICIARY: Address = address!("3000000000000000000000000000000000000003");

// ============================================================================
// access/volatile.rs — VolatileDataAccess::as_u8 discriminant encoder
// ============================================================================

/// `as_u8()` returns the single-set-bit position. Pinning several distinct variants kills both the
/// `-> 0` mutant (the `BENEFICIARY_BALANCE`/`ORACLE` cases are non-zero) and the `-> 1` mutant
/// (the `BLOCK_NUMBER` case is zero), since no single constant can satisfy all of them.
#[test]
fn as_u8_returns_exact_bit_position_for_each_variant() {
    assert_eq!(VolatileDataAccess::BLOCK_NUMBER.as_u8(), 0, "BLOCK_NUMBER is bit 0");
    assert_eq!(VolatileDataAccess::TIMESTAMP.as_u8(), 1, "TIMESTAMP is bit 1");
    assert_eq!(VolatileDataAccess::COINBASE.as_u8(), 2, "COINBASE is bit 2");
    assert_eq!(VolatileDataAccess::BLOB_HASH.as_u8(), 9, "BLOB_HASH is bit 9");
    assert_eq!(
        VolatileDataAccess::BENEFICIARY_BALANCE.as_u8(),
        10,
        "BENEFICIARY_BALANCE is bit 10"
    );
    assert_eq!(VolatileDataAccess::ORACLE.as_u8(), 11, "ORACLE is bit 11");
}

// ============================================================================
// access/tracker.rs — VolatileDataAccessTracker accessors
// ============================================================================

fn new_tracker() -> VolatileDataAccessTracker {
    VolatileDataAccessTracker::new(20_000_000, 20_000_000)
}

/// A fresh tracker has not accessed the beneficiary balance. The `-> true` mutant flips this.
#[test]
fn has_accessed_beneficiary_balance_is_false_until_marked() {
    let mut tracker = new_tracker();
    assert!(
        !tracker.has_accessed_beneficiary_balance(),
        "fresh tracker must report no beneficiary-balance access (kills the `-> true` mutant)"
    );

    tracker.mark_beneficiary_balance_accessed();
    assert!(
        tracker.has_accessed_beneficiary_balance(),
        "after marking, beneficiary-balance access must be reported"
    );
}

/// A fresh tracker reports `None` for volatile-data info. The `-> Some(Default::default())` mutant
/// returns `Some(empty)`, which is `!= None`.
#[test]
fn get_volatile_data_info_is_none_until_accessed() {
    let tracker = new_tracker();
    assert_eq!(
        tracker.get_volatile_data_info(),
        None,
        "fresh tracker must report no volatile-data info (kills the `Some(empty)` mutant)"
    );

    let mut tracker = new_tracker();
    tracker.mark_beneficiary_balance_accessed();
    assert_eq!(
        tracker.get_volatile_data_info(),
        Some(VolatileDataAccess::BENEFICIARY_BALANCE),
        "after a beneficiary access, the info must report exactly that access"
    );
}

/// `disable_access` keeps the *shallower* (smaller) depth when already active and is a no-op when
/// the new depth is deeper.
///
/// Killing the three `:180` mutants:
/// * `>=` → `<`: after `disable_access(5)` then `disable_access(8)`, the real code keeps depth 5
///   (`8 >= 5` is a no-op). The mutant takes the else branch and overwrites with 8, so
///   `volatile_access_disabled(5)` would become false.
/// * guard → `false`: the guard is never taken, so `disable_access(8)` overwrites with 8 — same
///   observable failure as above.
/// * guard → `true`: the guard is always taken, so `disable_access(2)` becomes a no-op and the
///   shallower depth 2 is never adopted, leaving `volatile_access_disabled(2)` false.
#[test]
fn disable_access_keeps_shallower_depth() {
    let mut tracker = new_tracker();

    tracker.disable_access(5);
    assert!(tracker.volatile_access_disabled(5), "depth 5 is disabled after disable_access(5)");
    assert!(!tracker.volatile_access_disabled(4), "depth 4 (shallower) is not disabled");

    // Disabling at a deeper depth must NOT relax the restriction: depth 5 stays disabled.
    // Real: `8 >= 5` → no-op, keeps 5. Mutants `<` / guard-`false`: overwrite with 8, so
    // `volatile_access_disabled(5)` would become false (`5 >= 8` is false).
    tracker.disable_access(8);
    assert!(
        tracker.volatile_access_disabled(5),
        "re-disabling at a deeper depth must keep the shallower depth (kills `<` and guard-false)"
    );

    // Disabling at a shallower depth must tighten the restriction down to depth 2.
    // Real: `2 >= 5` is false → else branch sets 2. Mutant guard-`true`: no-op, keeps 5, so
    // `volatile_access_disabled(2)` would be false (`2 >= 5` is false).
    tracker.disable_access(2);
    assert!(
        tracker.volatile_access_disabled(2),
        "re-disabling at a shallower depth must adopt it (kills the guard-true mutant)"
    );
}

// ============================================================================
// evm/mod.rs — MegaEvm accessor methods
// ============================================================================

/// Build a bare `MegaEvm` over an empty in-memory database (no transaction executed).
fn empty_evm(
    db: &mut MemoryDatabase,
) -> MegaEvm<&mut MemoryDatabase, revm::inspector::NoOpInspector, mega_evm::EmptyExternalEnv> {
    let context = MegaContext::new(db, MegaSpecId::REX4);
    MegaEvm::new(context)
}

/// The `Debug` impl writes the struct name and the `inspect` field. The
/// `fmt -> Ok(Default::default())` mutant skips all writes, producing an empty body.
#[test]
fn debug_impl_writes_struct_name_and_field() {
    let mut db = MemoryDatabase::default();
    let evm = empty_evm(&mut db);

    let rendered = format!("{evm:?}");
    assert!(
        rendered.contains("MegaethEvm"),
        "Debug output must contain the struct name, got: {rendered:?}"
    );
    assert!(
        rendered.contains("inspect"),
        "Debug output must contain the `inspect` field, got: {rendered:?}"
    );
}

/// `block_env_mut` must return a reference to the EVM's own block env, so a write through it is
/// visible via `block_env_ref`. The mutant returns a leaked default env, so the write is lost.
#[test]
fn block_env_mut_returns_live_block_env() {
    let mut db = MemoryDatabase::default();
    let mut evm = empty_evm(&mut db);

    let sentinel = U256::from(0xABCD_u64);
    evm.block_env_mut().number = sentinel;

    assert_eq!(
        evm.block_env_ref().number,
        sentinel,
        "a write through block_env_mut must be observable via block_env_ref \
         (kills the leaked-default mutant)"
    );
}

/// `get_accessed_bucket_ids` must report the SALT buckets touched during execution. An
/// `SSTORE`-set (zero → non-zero) records the slot's bucket. Under `TestExternalEnvs` the bucket
/// ID is non-zero, so the result distinguishes both the `vec![]` mutant (empty) and the
/// `vec![Default::default()]` mutant (`[0]`).
#[test]
fn get_accessed_bucket_ids_reports_touched_non_zero_bucket() {
    let storage_slot = U256::from(1_u64);

    // Contract that sets storage slot 1 to a non-zero value, then stops.
    let code = BytecodeBuilder::default().sstore(storage_slot, U256::from(1_u64)).stop().build();

    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_u64));
    db.set_account_code(CONTRACT, code);

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit: 10_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "SSTORE transaction should succeed: {:?}", result.result);

    // The SSTORE-set records the slot's bucket. Predict it from the same hasher the EVM uses.
    let expected_bucket =
        <TestExternalEnvs<Infallible> as SaltEnv>::bucket_id_for_slot(CONTRACT, storage_slot);
    assert_ne!(expected_bucket, 0, "TestExternalEnvs must produce a non-zero bucket ID");

    let buckets = evm.get_accessed_bucket_ids();
    assert!(
        !buckets.is_empty(),
        "an SSTORE-set transaction must record at least one accessed bucket (kills `vec![]`)"
    );
    assert!(
        buckets.contains(&expected_bucket),
        "accessed buckets {buckets:?} must contain the SSTORE slot's bucket {expected_bucket} \
         (kills the `vec![Default::default()]` = [0] mutant)"
    );
}

// ============================================================================
// evm/context.rs:521 — MegaContext::disable_beneficiary
// ============================================================================

/// Runs a simple value-transfer transaction whose entire gas price is a priority fee (block
/// `basefee` is 0), and returns the block beneficiary's (coinbase's) post-transaction balance.
///
/// When `disable` is true, `MegaContext::disable_beneficiary()` is called before execution; the
/// consumer at `evm/execution.rs:689` then skips `reward_beneficiary`, so the coinbase is NOT
/// credited the priority fee. Operator fees are zeroed so the only thing crediting the beneficiary
/// is the priority-fee reward.
fn beneficiary_balance_after_transfer(disable: bool) -> U256 {
    const GAS_PRICE: u128 = 7;

    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_u64));

    let block = BlockEnv { beneficiary: BENEFICIARY, basefee: 0, ..Default::default() };
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    if disable {
        context.disable_beneficiary();
    }

    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller: CALLER,
        // Plain value transfer to a fresh account: no code runs, so the only beneficiary credit is
        // the priority-fee reward.
        kind: TxKind::Call(CONTRACT),
        gas_limit: 1_000_000,
        gas_price: GAS_PRICE,
        value: U256::from(1_u64),
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success(), "transfer should succeed: {:?}", result.result);

    result.state.get(&BENEFICIARY).map(|acct| acct.info.balance).unwrap_or(U256::ZERO)
}

/// `disable_beneficiary()` must suppress the post-transaction coinbase reward.
///
/// Control run (not disabled): the beneficiary is credited the priority fee, so its balance is
/// non-zero. Disabled run: the beneficiary reward is skipped, so its balance is zero.
///
/// The `disable_beneficiary -> ()` no-op mutant never sets the flag, so the "disabled" run behaves
/// like the control and credits the beneficiary — making the `assert_eq!(disabled, ZERO)` below
/// fail, which kills the mutant.
#[test]
fn disable_beneficiary_suppresses_coinbase_reward() {
    let credited = beneficiary_balance_after_transfer(false);
    assert!(
        credited > U256::ZERO,
        "control run must credit the beneficiary the priority fee (sanity check)"
    );

    let suppressed = beneficiary_balance_after_transfer(true);
    assert_eq!(
        suppressed,
        U256::ZERO,
        "disable_beneficiary() must suppress the coinbase reward (kills the `-> ()` no-op mutant); \
         got {suppressed} vs control {credited}"
    );
}

// ============================================================================
// evm/execution.rs:167 — MegaHandler::before_execution intrinsic-gas boundary
// ============================================================================

/// Executes a plain call (no calldata, no access list, no value) to an existing code account that
/// immediately `STOP`s, with the given `gas_limit`, and returns the `transact_raw` outcome.
///
/// Under REX the intrinsic gas of such a call is exactly `21000 + TX_INTRINSIC_STORAGE_GAS`
/// (no calldata words, no new-account creation since the callee already has code and the value is
/// zero). `MegaHandler::before_execution` (`evm/execution.rs:167`) halts the tx out-of-gas when
/// `gas_limit < initial_gas`. Operator fees and the basefee are zeroed so the only thing that can
/// make the tx fail at the boundary is that intrinsic-gas guard.
fn intrinsic_boundary_result(
    gas_limit: u64,
) -> Result<
    revm::context::result::ResultAndState<MegaHaltReason>,
    revm::context::result::EVMError<Infallible, mega_evm::MegaTransactionError>,
> {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_u64));
    // Callee already exists (has code) and just STOPs, so the call adds no new-account storage gas
    // and burns no execution gas beyond the intrinsic cost.
    db.set_account_code(CONTRACT, BytecodeBuilder::default().stop().build());

    let block = BlockEnv { basefee: 0, ..Default::default() };
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit,
        gas_price: 0,
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// `before_execution` must allow a tx whose `gas_limit` equals its (fully Mega-adjusted) intrinsic
/// gas, and reject one below it. This pins the `<` boundary at `evm/execution.rs:167`.
///
/// Under REX the intrinsic gas of the bare call above is `21000 + TX_INTRINSIC_STORAGE_GAS`
/// (= 60000). With `gas_limit == initial_gas` the real `gas_limit < initial_gas` check is `false`,
/// so `before_execution` returns `Ok(None)` and the tx executes and succeeds. The `<` → `<=`
/// mutant makes it `60000 <= 60000` → `true`, wrongly halting the exact-gas tx out-of-gas — caught
/// by the `is_success()` assertion below. The `gas_limit == initial_gas - 1` case pins the lower
/// side of the boundary: validation (`initial_gas > gas_limit`) rejects it as a transaction error.
#[test]
fn before_execution_allows_exact_intrinsic_gas() {
    // Fully Mega-adjusted intrinsic gas for the bare REX call: base 21000 + the REX intrinsic
    // storage surcharge. The `gas_used == intrinsic` assertion below self-validates this value.
    let intrinsic: u64 = 21_000 + constants::rex::TX_INTRINSIC_STORAGE_GAS;

    // Exactly intrinsic gas: must execute and succeed (kills the `<` → `<=` mutant, which would
    // OOG-halt this exact-gas tx).
    let exact = intrinsic_boundary_result(intrinsic).expect("exact-gas tx must not be rejected");
    assert!(
        exact.result.is_success(),
        "a call with gas_limit == intrinsic gas ({intrinsic}) must succeed, not OOG-halt; \
         got {:?}",
        exact.result
    );
    assert_eq!(
        exact.result.gas_used(),
        intrinsic,
        "the exact-gas call must consume exactly the intrinsic gas"
    );

    // One below intrinsic gas: must NOT succeed (pins the lower side of the boundary). Under REX
    // this is surfaced as a `CallGasCostMoreThanGasLimit` validation error.
    let one_below = intrinsic_boundary_result(intrinsic - 1);
    assert!(
        one_below.is_err() || !one_below.unwrap().result.is_success(),
        "a call with gas_limit == intrinsic gas - 1 ({}) must fail the intrinsic-gas guard",
        intrinsic - 1
    );
}

// ============================================================================
// evm/context.rs:563 — MegaContext::on_new_block refreshes the dynamic gas cache
// ============================================================================

/// `on_new_block()` (invoked by `with_block`) must refresh the dynamic storage gas-cost
/// calculator for the new block. Concretely it calls `DynamicGasCost::on_new_block`, which
/// `reset`s the calculator and **clears its cached per-bucket cost multipliers**. The
/// `MegaContext::on_new_block -> ()` no-op mutant skips this, leaving a stale cached multiplier.
///
/// Observable effect: under `MINI_REX` (but not REX), the SSTORE-set gas is
/// `SSTORE_SET_STORAGE_GAS * (capacity / MIN_BUCKET_SIZE)`. The calculator caches the multiplier
/// the first time a bucket is queried. If the underlying SALT bucket capacity changes between
/// blocks, the cached multiplier is stale until `on_new_block` clears it.
///
/// The test primes the cache at capacity `C1`, then mutates the shared `TestExternalEnvs` bucket
/// capacity to `C2` (the env is shared by `Rc<RefCell>`), then calls `with_block` to drive
/// `on_new_block`, and asserts the recomputed SSTORE gas reflects `C2`. The no-op mutant keeps the
/// stale `C1` multiplier, so the assertion fails — killing the mutant.
#[test]
fn on_new_block_clears_stale_dynamic_gas_cache() {
    let min_bucket = MIN_BUCKET_SIZE as u64;
    // Two capacities yielding distinct multipliers (capacity / MIN_BUCKET_SIZE).
    let cap_before = 2 * min_bucket; // multiplier 2
    let cap_after = 3 * min_bucket; // multiplier 3
    let base = constants::mini_rex::SSTORE_SET_STORAGE_GAS;
    let gas_before = base * 2;
    let gas_after = base * 3;

    let storage_slot = U256::from(7_u64);
    let bucket =
        <TestExternalEnvs<Infallible> as SaltEnv>::bucket_id_for_slot(CONTRACT, storage_slot);

    // Shared env handle. `with_bucket_capacity` mutates the shared `Rc<RefCell>` map and returns
    // `self`; cloning shares the same map, so later edits are visible to the EVM's clone.
    let env = TestExternalEnvs::<Infallible>::new().with_bucket_capacity(bucket, cap_before);

    let mut db = MemoryDatabase::default();
    // MINI_REX (not REX) so the gas formula is the simple `base * multiplier`.
    let context =
        MegaContext::new(&mut db, MegaSpecId::MINI_REX).with_external_envs(env.clone().into());

    // Prime the cache: the calculator now caches multiplier = cap_before / MIN_BUCKET_SIZE.
    let primed = context
        .dynamic_storage_gas_cost
        .borrow_mut()
        .sstore_set_gas(CONTRACT, storage_slot)
        .unwrap();
    assert_eq!(primed, gas_before, "primed SSTORE gas must reflect the initial capacity");

    // Change the shared bucket capacity. Without a cache clear, the calculator keeps `cap_before`.
    env.clear_bucket_capacity();
    let _ = env.clone().with_bucket_capacity(bucket, cap_after);
    assert_eq!(
        <TestExternalEnvs<Infallible> as SaltEnv>::get_bucket_capacity(&env, bucket).unwrap(),
        cap_after,
        "the shared env must now report the new capacity"
    );

    // Drive `on_new_block` via the public `with_block`. Real code clears the stale cache; the
    // no-op mutant leaves it.
    let context = context.with_block(BlockEnv { number: U256::from(2_u64), ..Default::default() });

    let recomputed = context
        .dynamic_storage_gas_cost
        .borrow_mut()
        .sstore_set_gas(CONTRACT, storage_slot)
        .unwrap();
    assert_eq!(
        recomputed, gas_after,
        "after on_new_block the dynamic gas cache must be refreshed to the new capacity \
         (kills the `on_new_block -> ()` no-op mutant; stale value would be {gas_before})"
    );
}
