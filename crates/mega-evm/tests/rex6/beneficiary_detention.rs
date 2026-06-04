//! REX6 beneficiary detention / volatile-access coverage tests.
//!
//! Covers three scenarios whose root cause is "REX4+ beneficiary detention +
//! `disableVolatileDataAccess` only saw part of the surface":
//!
//! - **Source-side SELFDESTRUCT** — `volatile_data_ext::selfdestruct_with_beneficiary_guard` peeks
//!   the stack target on every spec, and REX6 additionally compares the source (executing contract
//!   whose balance is read and zeroed) against the beneficiary. Pre-REX6 source-side behavior is
//!   frozen.
//!
//! - **EIP-7702-delegated CALL** — REX5 `wrap_call_volatile_check!` compared the raw stack operand;
//!   a CALL to delegator `A` whose EIP-7702 code points at `B == beneficiary` slipped past both the
//!   `disableVolatileDataAccess` revert and the detention mark. That wrapper now resolves the
//!   EIP-7702 delegate one hop before the comparison under REX6 (raw operand <= REX5);
//!   `MegaContext::load_account_delegated` also marks the resolved delegate.
//!
//! - **Existing-target SELFDESTRUCT** — REX5 `storage_gas_ext::selfdestruct` only charged
//!   DataSize/KV/StateGrowth for SELFDESTRUCT to a *new* beneficiary. When the target already
//!   exists, the balance update went through `host.selfdestruct` without flowing through any
//!   frame-init or `target_updated` path, so DataSize/KV stayed at zero. Under REX6 that handler
//!   dispatches to `selfdestruct_rex6`, which records `DataSize` +40 / KV +1 (no `StateGrowth` —
//!   the target already exists) for the existing-target balance credit.
//!
//! Each scenario is paired with a REX5 baseline that freeze-guards the pre-REX6 behavior.
//! Pre-REX6 dispatch tables are unchanged.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaAccessControl, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, MegaTransactionError, VolatileDataAccessType,
    ACCESS_CONTROL_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        BlockEnv, TxEnv,
    },
    database::AccountState,
    handler::EvmTr,
    state::Bytecode,
};

/// 4-byte selector for `disableVolatileDataAccess()`.
const DISABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

/// 4-byte selector for the `VolatileDataAccessDisabled(uint8)` error.
const VOLATILE_DATA_ACCESS_DISABLED_SELECTOR: [u8; 4] =
    IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR;

// ============================================================================
// TEST ADDRESSES
// ============================================================================

/// Externally-owned tx sender. Never the beneficiary.
const CALLER: Address = address!("0000000000000000000000000000000000600000");
/// Non-beneficiary intermediary used to keep the top-level tx target distinct
/// from the beneficiary (avoids `on_new_tx`'s eager beneficiary-recipient mark
/// muddying the detention assertions).
const MIDDLE: Address = address!("0000000000000000000000000000000000600001");
/// Block beneficiary used throughout. Set via the `BlockEnv` in
/// `transact_with_beneficiary`.
const BENEFICIARY: Address = address!("0000000000000000000000000000000000600099");
/// SELFDESTRUCT destination for the source-side cases — empty and *not* the beneficiary.
const EMPTY_NON_BENEFICIARY: Address = address!("0000000000000000000000000000000000600002");
/// SELFDESTRUCT destination for the existing-target case — pre-existing and *not*
/// the beneficiary.
const EXISTING_NON_BENEFICIARY: Address = address!("0000000000000000000000000000000000600003");
/// EIP-7702 delegator used in the delegated-CALL tests — its bytecode is `0xef0100 ||
/// BENEFICIARY`, so a CALL to it should ultimately observe beneficiary state.
const DELEGATOR_TO_BENEFICIARY: Address = address!("0000000000000000000000000000000000600004");

/// Synthetic finite block-env-access compute-gas cap. Any value strictly less
/// than `u64::MAX` makes `detained_compute_gas_limit()` drop below `u64::MAX`
/// once `mark_beneficiary_balance_accessed()` fires; the exact value doesn't
/// matter as long as it doesn't saturate.
const DETENTION_CAP: u64 = 20_000_000;

// ============================================================================
// HELPERS
// ============================================================================

/// `(execution result, recorded limit usage, detained block-env-access compute-gas
/// limit, whether the beneficiary balance was marked accessed)`.
type BeneficiaryTransactResult = Result<
    (ResultAndState<MegaHaltReason>, LimitUsage, u64, bool),
    EVMError<Infallible, MegaTransactionError>,
>;

/// Executes `tx` under `spec` with the block beneficiary set to `BENEFICIARY`
/// and a finite block-env-access compute-gas limit so detention is observable
/// via `detained_compute_gas_limit()`.
fn transact_with_beneficiary(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> BeneficiaryTransactResult {
    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(db, spec).with_block(block).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(DETENTION_CAP),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx)?;
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    let detained = evm.ctx_ref().additional_limit.borrow().detained_compute_gas_limit();
    let beneficiary_marked =
        evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance();
    Ok((result, usage, detained, beneficiary_marked))
}

/// Installs `0xef0100 || delegate_to` at `address`, mirroring what revm's
/// `apply_eip7702_auth_list` does for Type 4 transactions.
fn set_eip7702_delegation(db: &mut MemoryDatabase, address: Address, delegate_to: Address) {
    let bytecode = Bytecode::new_eip7702(delegate_to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).unwrap();
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

/// Builds bytecode that calls `disableVolatileDataAccess()` on the
/// `MegaAccessControl` system contract.
fn call_disable_volatile_data_access(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP)
}

/// Decodes `VolatileDataAccessDisabled(uint8 accessType)` from revert data.
fn decode_volatile_data_access_disabled(
    data: &[u8],
) -> IMegaAccessControl::VolatileDataAccessDisabled {
    <IMegaAccessControl::VolatileDataAccessDisabled as SolError>::abi_decode(data)
        .expect("valid VolatileDataAccessDisabled revert data")
}

// ============================================================================
// SELFDESTRUCT source-side coverage
// ============================================================================

/// REX6: When the executing contract IS the block beneficiary and disables
/// volatile access mid-frame, then SELFDESTRUCT to a non-beneficiary target,
/// the outer wrapper's source-side check must revert with
/// `VolatileDataAccessDisabled(Beneficiary)`. REX5 only inspected the stack
/// target — non-beneficiary destination, no revert.
///
/// Setup: deploy `SELFDESTRUCTing` code at `BENEFICIARY`. Top-level tx is from
/// `CALLER` → `BENEFICIARY` (recipient == beneficiary). The disable-volatile
/// call happens INSIDE `BENEFICIARY`'s frame (so the outer call to `ACCESS_CONTROL`
/// is not itself blocked by the existing REX5 CALL wrapper).
#[test]
fn test_rex6_selfdestruct_source_eq_beneficiary_disabled_reverts() {
    let beneficiary_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_address(EMPTY_NON_BENEFICIARY)
        .append(SELFDESTRUCT)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(BENEFICIARY, beneficiary_code)
        .account_balance(BENEFICIARY, U256::from(1_000_000u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(100_000_000)
        .build_fill();

    let (result, usage, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    // The SELFDESTRUCT's source-side guard reverts the beneficiary frame
    // directly — the top-level tx surfaces as Revert.
    let output = result.result.output().expect("revert with VolatileDataAccessDisabled payload");
    assert_eq!(
        &output[..4],
        &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
        "REX6 must revert with VolatileDataAccessDisabled (source-side check); got {output:?}",
    );
    assert_eq!(
        decode_volatile_data_access_disabled(output).accessType,
        VolatileDataAccessType::Beneficiary,
    );
    // Source-side guard short-circuits before any storage-layer side effect.
    assert_eq!(usage.state_growth, 0, "no on_selfdestruct_new_account: {}", usage.state_growth);
}

/// REX5 baseline for the source-side case above. Same code and setup; REX5's outer wrapper
/// checks only the stack target, so a source-side SELFDESTRUCT to a non-beneficiary target
/// succeeds. Freeze-guards the pre-REX6 behavior.
#[test]
fn test_rex5_selfdestruct_source_eq_beneficiary_disabled_does_not_revert() {
    let beneficiary_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_address(EMPTY_NON_BENEFICIARY)
        .append(SELFDESTRUCT)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(BENEFICIARY, beneficiary_code)
        .account_balance(BENEFICIARY, U256::from(1_000_000u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(100_000_000)
        .build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX5, &mut db, tx).unwrap();
    assert!(
        result.result.is_success(),
        "REX5 must NOT revert the SELFDESTRUCT — the source-side check is REX6-only; got: {result:?}",
    );
}

/// REX6: SELFDESTRUCT with the stack target == beneficiary, volatile disabled.
/// Same target-side case that REX5 already covers: the REX6 wrapper must still
/// revert here. The source-side check is additive, and the target-side check is
/// retained.
#[test]
fn test_rex6_selfdestruct_target_eq_beneficiary_disabled_still_reverts() {
    let beneficiary_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_address(BENEFICIARY)
        .append(SELFDESTRUCT)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(BENEFICIARY, beneficiary_code)
        .account_balance(BENEFICIARY, U256::from(1_000_000u64));

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(100_000_000)
        .build_fill();

    let (result, usage, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    let output = result.result.output().expect("revert with VolatileDataAccessDisabled");
    assert_eq!(&output[..4], &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR);
    assert_eq!(
        decode_volatile_data_access_disabled(output).accessType,
        VolatileDataAccessType::Beneficiary,
    );
    assert_eq!(usage.state_growth, 0, "guard short-circuits");
}

// ============================================================================
// CALL family EIP-7702 delegate resolution
// ============================================================================

/// REX6: A CALL to an EIP-7702 delegator whose code points at the beneficiary,
/// inside a disabled-volatile frame, must revert with
/// `VolatileDataAccessDisabled(Beneficiary)`. REX5 only compared the raw stack
/// operand and let the CALL through.
///
/// Setup: top-level tx → `MIDDLE`. `MIDDLE`'s code disables volatile access
/// then CALLs the delegator. In REX6 the wrapper resolves DELEGATOR →
/// BENEFICIARY and reverts the `MIDDLE` frame; in REX5 the CALL completes.
#[test]
fn test_rex6_call_to_eip7702_delegator_to_beneficiary_disabled_reverts() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(DELEGATOR_TO_BENEFICIARY)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code)
        .account_code(BENEFICIARY, beneficiary_code);
    set_eip7702_delegation(&mut db, DELEGATOR_TO_BENEFICIARY, BENEFICIARY);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    let output = result.result.output().expect("revert with VolatileDataAccessDisabled");
    assert_eq!(
        &output[..4],
        &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
        "REX6 wrapper must resolve DELEGATOR → BENEFICIARY and revert: {output:?}",
    );
    assert_eq!(
        decode_volatile_data_access_disabled(output).accessType,
        VolatileDataAccessType::Beneficiary,
    );
}

/// REX5 baseline: same setup, but REX5's wrapper compares the raw stack
/// operand (DELEGATOR != BENEFICIARY), so no revert. The CALL succeeds and the
/// delegate's STOP runs.
#[test]
fn test_rex5_call_to_eip7702_delegator_to_beneficiary_disabled_does_not_revert() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(DELEGATOR_TO_BENEFICIARY)
        .push_number(100_000_u64)
        .append(CALL)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code)
        .account_code(BENEFICIARY, beneficiary_code);
    set_eip7702_delegation(&mut db, DELEGATOR_TO_BENEFICIARY, BENEFICIARY);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX5, &mut db, tx).unwrap();
    assert!(
        result.result.is_success(),
        "REX5 wrapper must NOT revert — compares raw stack operand only: {result:?}",
    );
}

/// REX6: A CALL to an EIP-7702 delegator pointing at the beneficiary, with
/// volatile access enabled, must engage beneficiary detention via
/// `MegaContext::load_account_delegated`'s resolved-delegate mark.
///
/// REX5 only marks the raw input (`DELEGATOR_TO_BENEFICIARY`), which is not the
/// block beneficiary, so detention never engages.
///
/// We compare both volatile-access marking and the detained compute-gas limit
/// to make the assertion self-explanatory.
#[test]
fn test_rex6_call_to_eip7702_delegator_to_beneficiary_enabled_marks_beneficiary() {
    let beneficiary_code = BytecodeBuilder::default().stop().build();
    let middle_code = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(DELEGATOR_TO_BENEFICIARY)
        .push_number(100_000_u64)
        .append(CALL)
        .append(STOP)
        .build();

    let build_db = || {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_code(MIDDLE, middle_code.clone())
            .account_code(BENEFICIARY, beneficiary_code.clone());
        set_eip7702_delegation(&mut db, DELEGATOR_TO_BENEFICIARY, BENEFICIARY);
        db
    };
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();

    let (result_rex5, _, detained_rex5, marked_rex5) =
        transact_with_beneficiary(MegaSpecId::REX5, &mut build_db(), tx.clone()).unwrap();
    assert!(result_rex5.result.is_success(), "REX5 tx should succeed: {result_rex5:?}");
    assert!(!marked_rex5, "REX5 must not mark beneficiary (raw target = DELEGATOR != BENEFICIARY)");
    assert_eq!(detained_rex5, u64::MAX, "REX5 must not engage detention: {detained_rex5}");

    let (result_rex6, _, detained_rex6, marked_rex6) =
        transact_with_beneficiary(MegaSpecId::REX6, &mut build_db(), tx).unwrap();
    assert!(result_rex6.result.is_success(), "REX6 tx should succeed: {result_rex6:?}");
    assert!(marked_rex6, "REX6 must mark beneficiary via resolved delegate");
    assert!(detained_rex6 < u64::MAX, "REX6 must engage detention: {detained_rex6}");
}

// ============================================================================
// Existing-target SELFDESTRUCT accounting
// ============================================================================

/// REX6: SELFDESTRUCT to an *existing* non-beneficiary target with value must
/// record `DataSize` +40 and KV +1 for the balance update, but no `StateGrowth`
/// (the target already exists). REX5 left all three at zero.
///
/// Driver contract is not the beneficiary, target is not the beneficiary;
/// keeps the detention and beneficiary marks out of the way of the accounting
/// assertion.
#[test]
fn test_rex6_selfdestruct_to_existing_target_records_data_size_and_kv() {
    let code = BytecodeBuilder::default()
        .push_address(EXISTING_NON_BENEFICIARY)
        .append(SELFDESTRUCT)
        .build();

    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_code(MIDDLE, code.clone())
            .account_balance(MIDDLE, U256::from(1_000_000u64))
            // Pre-fund EXISTING_NON_BENEFICIARY so it is *not* empty.
            .account_balance(EXISTING_NON_BENEFICIARY, U256::from(1u64))
    };
    let tx = TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(1_000_000).build_fill();

    // REX5 records nothing for an existing-target balance update.
    let (result_rex5, usage_rex5, _, _) =
        transact_with_beneficiary(MegaSpecId::REX5, &mut build_db(), tx.clone()).unwrap();
    assert!(result_rex5.result.is_success(), "REX5 tx should succeed: {result_rex5:?}");
    assert_eq!(usage_rex5.state_growth, 0, "REX5 must not record state growth (target exists)");
    let baseline_data_size = usage_rex5.data_size;
    let baseline_kv = usage_rex5.kv_updates;

    // REX6: existing-target arm records DataSize/KV but not StateGrowth.
    let (result_rex6, usage_rex6, _, _) =
        transact_with_beneficiary(MegaSpecId::REX6, &mut build_db(), tx).unwrap();
    assert!(result_rex6.result.is_success(), "REX6 tx should succeed: {result_rex6:?}");
    assert_eq!(
        usage_rex6.state_growth, 0,
        "REX6 existing-target SELFDESTRUCT must NOT record state growth: {}",
        usage_rex6.state_growth,
    );
    assert!(
        usage_rex6.data_size > baseline_data_size,
        "REX6 must record DataSize delta vs REX5 baseline ({} > {})",
        usage_rex6.data_size,
        baseline_data_size,
    );
    assert!(
        usage_rex6.kv_updates > baseline_kv,
        "REX6 must record KV update delta vs REX5 baseline ({} > {})",
        usage_rex6.kv_updates,
        baseline_kv,
    );
}

/// REX6 regression guard: SELFDESTRUCT to an *empty* non-beneficiary target
/// must still go through the new-target arm (state growth + `DataSize` + KV +
/// new-account storage gas), matching REX5's behavior for the same case.
#[test]
fn test_rex6_selfdestruct_to_empty_target_still_records_state_growth() {
    let code =
        BytecodeBuilder::default().push_address(EMPTY_NON_BENEFICIARY).append(SELFDESTRUCT).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, code)
        .account_balance(MIDDLE, U256::from(1_000_000u64));
    let tx = TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(1_000_000).build_fill();

    let (result, usage, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    assert!(result.result.is_success(), "REX6 tx should succeed: {result:?}");
    assert!(
        usage.state_growth > 0,
        "REX6 new-target SELFDESTRUCT must still record state growth: {}",
        usage.state_growth,
    );
}

// ============================================================================
// SELFDESTRUCT target-side freeze, enabled-path detention, self-target, DB-error
// ============================================================================

/// REX5 must still revert SELFDESTRUCT when the stack target is the beneficiary under disabled
/// volatile access: the target-side guard is spec-independent, only the source-side guard is
/// REX6-only. Mirrors `test_rex6_selfdestruct_target_eq_beneficiary_disabled_still_reverts`.
#[test]
fn test_rex5_selfdestruct_target_eq_beneficiary_disabled_still_reverts() {
    let beneficiary_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_address(BENEFICIARY)
        .append(SELFDESTRUCT)
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(BENEFICIARY, beneficiary_code)
        .account_balance(BENEFICIARY, U256::from(1_000_000u64));
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(100_000_000)
        .build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX5, &mut db, tx).unwrap();
    let output = result.result.output().expect("REX5 must still revert on target==beneficiary");
    assert_eq!(
        &output[..4],
        &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
        "REX5 target-side revert must be preserved by the hand-written wrapper: {output:?}",
    );
}

/// With volatile access enabled, a SELFDESTRUCT whose source is the beneficiary engages detention
/// purely via the entry mark: reaching beneficiary-as-executor requires entering its frame (here
/// the tx recipient is the beneficiary, so `on_new_tx` eagerly marks the beneficiary balance), so
/// detention already engages under REX5 and REX6 identically — no SELFDESTRUCT-specific source hook
/// is needed on the enabled path.
#[test]
fn test_selfdestruct_source_eq_beneficiary_enabled_detention_via_entry_mark_rex5_eq_rex6() {
    let beneficiary_code =
        BytecodeBuilder::default().push_address(EMPTY_NON_BENEFICIARY).append(SELFDESTRUCT).build();
    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_code(BENEFICIARY, beneficiary_code.clone())
            .account_balance(BENEFICIARY, U256::from(1_000_000u64))
    };
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(100_000_000)
        .build_fill();

    let (r5, _, d5, m5) =
        transact_with_beneficiary(MegaSpecId::REX5, &mut build_db(), tx.clone()).unwrap();
    let (r6, _, d6, m6) = transact_with_beneficiary(MegaSpecId::REX6, &mut build_db(), tx).unwrap();
    assert!(r5.result.is_success() && r6.result.is_success());
    // The entry mark (recipient == beneficiary) engages detention on BOTH specs, so the enabled
    // path needs no SELFDESTRUCT-specific source hook.
    assert!(m5 && m6, "entry-mark engages detention on both specs (m5={m5} m6={m6})");
    assert!(d5 < u64::MAX && d6 < u64::MAX, "both detained (d5={d5} d6={d6})");
    assert_eq!(
        d5, d6,
        "enabled-path detention is identical on REX5/REX6 (entry-mark): {d5} == {d6}"
    );
}

// ============================================================================
// DB-error coverage for the REX6 `selfdestruct_rex6` inspect paths
// ============================================================================

/// REX6: a DB failure while resolving the SELFDESTRUCT target must SURFACE (as an `EVMError` or a
/// non-success halt) — never a silent success. `MemoryDatabase` is infallible, so this is only
/// reachable by injecting a `basic()` failure on the target. The error surfaces as an `EVMError`
/// (the journal records the DB failure and returns it at the transaction boundary), the robust
/// outcome the SELFDESTRUCT accounting path must preserve.
#[test]
fn test_rex6_selfdestruct_db_error_on_target_surfaces() {
    let code = BytecodeBuilder::default()
        .push_address(EXISTING_NON_BENEFICIARY)
        .append(SELFDESTRUCT)
        .build();
    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, code)
        .account_balance(MIDDLE, U256::from(1_000_000u64))
        .account_balance(EXISTING_NON_BENEFICIARY, U256::from(1u64));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(EXISTING_NON_BENEFICIARY);

    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX6).with_block(block).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(DETENTION_CAP),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(1_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let surfaced = match alloy_evm::Evm::transact_raw(&mut evm, tx) {
        Err(_) => true,                            // propagated as EVMError
        Ok(result) => !result.result.is_success(), // or a non-success halt
    };
    assert!(surfaced, "a DB error on the SELFDESTRUCT target must surface, not be swallowed");
}

/// REX6: SELFDESTRUCT to SELF on a non-same-tx-created account is an EIP-6780 balance no-op
/// (self → self transfer, no account-info write, account not deleted), so the existing-target
/// accounting arm must record NOTHING — same as REX5. Guards against over-recording `DataSize`
/// +40 / KV +1 (and a spurious additional-limit revert) for a write that never happens.
#[test]
fn test_rex6_selfdestruct_to_self_records_no_existing_target_accounting() {
    let code = BytecodeBuilder::default().push_address(MIDDLE).append(SELFDESTRUCT).build();
    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_code(MIDDLE, code.clone())
            .account_balance(MIDDLE, U256::from(1_000_000u64))
    };
    let tx = TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(1_000_000).build_fill();

    let (r5, u5, _, _) =
        transact_with_beneficiary(MegaSpecId::REX5, &mut build_db(), tx.clone()).unwrap();
    let (r6, u6, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut build_db(), tx).unwrap();
    assert!(r5.result.is_success() && r6.result.is_success());
    assert_eq!(
        u6.data_size, u5.data_size,
        "SELFDESTRUCT to self is a no-op; REX6 must not over-record DataSize (rex6={} rex5={})",
        u6.data_size, u5.data_size,
    );
    assert_eq!(
        u6.kv_updates, u5.kv_updates,
        "SELFDESTRUCT to self is a no-op; REX6 must not over-record KV (rex6={} rex5={})",
        u6.kv_updates, u5.kv_updates,
    );
}

/// REX6: a malformed SELFDESTRUCT (stack underflow) executed by the beneficiary with volatile
/// access disabled must still halt with `StackUnderflow`. The source-side beneficiary guard must
/// not pre-empt the missing-operand check — it only applies once the SELFDESTRUCT has a target to
/// act on. Pre-REX6 already produced `StackUnderflow` here (the source guard is REX6-only).
#[test]
fn test_rex6_selfdestruct_stack_underflow_keeps_stack_underflow_under_source_guard() {
    // BENEFICIARY disables volatile access, then runs SELFDESTRUCT with an EMPTY stack (underflow).
    let beneficiary_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .append(SELFDESTRUCT) // no beneficiary operand pushed -> stack underflow
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(BENEFICIARY, beneficiary_code)
        .account_balance(BENEFICIARY, U256::from(1_000_000u64));
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(BENEFICIARY)
        .gas_limit(100_000_000)
        .build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    // The malformed SELFDESTRUCT must surface as a Halt (StackUnderflow), NOT a
    // VolatileDataAccessDisabled Revert pre-empting the operand check.
    assert!(
        matches!(&result.result, ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("StackUnderflow")),
        "stack-underflow SELFDESTRUCT must keep its StackUnderflow halt, not be pre-empted by the \
         beneficiary revert; got: {:?}",
        result.result,
    );
}

/// REX6: with volatile access disabled, a CALL whose effective target is NOT the beneficiary
/// (no EIP-7702 delegation to it) passes the beneficiary-volatile check and runs normally — the
/// guard reverts only when the resolved target is the beneficiary.
#[test]
fn test_rex6_call_to_non_beneficiary_under_disabled_volatile_proceeds() {
    let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(EMPTY_NON_BENEFICIARY) // target — not the beneficiary, no delegation
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(STOP)
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code);
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    assert!(
        result.result.is_success(),
        "a CALL to a non-beneficiary target under disabled volatile access must proceed: {:?}",
        result.result,
    );
}

/// REX6: a DB failure while `load_account_delegated` resolves a CALL target's EIP-7702 delegate
/// must SURFACE (as an `EVMError` or a non-success halt), never a silent success. The resolve error
/// is swallowed by the side-mark (best-effort), and the inner `load_account_delegated` then
/// surfaces the failure. `MemoryDatabase` is infallible, so this is only reachable by injecting a
/// `basic()` failure on the CALL target.
#[test]
fn test_rex6_call_db_error_on_target_surfaces() {
    let middle_code = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(EMPTY_NON_BENEFICIARY) // target whose account read will fail
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(STOP)
        .build();
    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code);
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(EMPTY_NON_BENEFICIARY);

    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX6).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let surfaced = match alloy_evm::Evm::transact_raw(&mut evm, tx) {
        Err(_) => true,
        Ok(result) => !result.result.is_success(),
    };
    assert!(surfaced, "a DB error on the CALL target must surface, not be swallowed");
}

/// REX6: a malformed CALL (stack underflow) under disabled volatile access must keep its
/// `StackUnderflow` halt — the volatile wrapper's operand peek finds nothing and falls through,
/// so the missing-operand check is not pre-empted by a beneficiary revert.
#[test]
fn test_rex6_call_stack_underflow_under_disabled_volatile_keeps_stack_underflow() {
    let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .append(CALL) // no operands pushed -> stack underflow
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code);
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();

    let (result, _, _, _) = transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx).unwrap();
    assert!(
        matches!(&result.result, ExecutionResult::Halt { reason, .. }
            if format!("{reason:?}").contains("StackUnderflow")),
        "stack-underflow CALL under disabled volatile must keep its StackUnderflow halt, not be \
         pre-empted by a beneficiary revert: {:?}",
        result.result,
    );
}

/// A partial-stack CALL — operands `[gas, target]` present but the remaining operands missing —
/// whose target's account read fails surfaces the DB error identically on REX5 and REX6 under
/// disabled volatile access, i.e. the REX6 beneficiary-delegate resolve does NOT uniquely mask
/// anything. `storage_gas_ext::call` must read the target's account (for new-account storage gas)
/// before the opcode validates its later operands, so a failing target surfaces there regardless of
/// the volatile wrapper; the wrapper's resolve reads the same account. Pins that subsumption so a
/// future change to the wrapper's resolve placement cannot silently diverge REX6 from the frozen
/// REX5 behavior. The target is not the beneficiary (the raw-target check misses).
#[test]
fn test_call_partial_stack_db_error_on_target_surfaces_rex5_eq_rex6() {
    let surfaces = |spec: MegaSpecId| {
        let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
            .push_address(EMPTY_NON_BENEFICIARY) // target (position 1) — not the beneficiary
            .push_number(100_000_u64) // gas (position 0); remaining CALL operands missing
            .append(CALL)
            .append(STOP)
            .build();
        let inner = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_code(MIDDLE, middle_code);
        let mut db = ErrorInjectingDatabase::new(inner);
        db.fail_on_account = Some(EMPTY_NON_BENEFICIARY);

        let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
        let mut context = MegaContext::new(&mut db, spec).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::from(0));
            chain.operator_fee_constant = Some(U256::from(0));
        });
        let mut evm = MegaEvm::new(context);
        let tx =
            TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());
        match alloy_evm::Evm::transact_raw(&mut evm, tx) {
            Err(_) => true,
            Ok(result) => !result.result.is_success(),
        }
    };
    assert!(surfaces(MegaSpecId::REX5), "REX5: failing target must surface");
    assert!(
        surfaces(MegaSpecId::REX6),
        "REX6 must surface identically to REX5 — the delegate resolve masks nothing the mandatory \
         storage-gas target read would not already surface",
    );
}

/// REX6: a malformed (partial-stack) CALL whose target's *code* fails to load must keep its
/// `StackUnderflow` halt under disabled volatile access. The beneficiary-volatile precheck resolves
/// the target's EIP-7702 delegate on a best-effort basis, which loads the target's code
/// (`load_code = true`); that read must NOT stash a `ctx.error`. Here the target's `basic()`
/// succeeds — so the mandatory storage-gas inspect (`load_code = false`) does not touch the code
/// and the CALL reaches the operand check and underflows — but the target's code load fails, a
/// failure only the precheck touches. Without a non-stashing (best-effort) resolve the underflow is
/// masked by a spurious `EVMError`.
#[test]
fn test_rex6_call_partial_stack_code_load_failure_keeps_stack_underflow() {
    // `to` is a non-beneficiary target whose code is lazy and whose `code_by_hash` is injected
    // to fail (so only the `load_code = true` precheck touches the failure).
    let to = EXISTING_NON_BENEFICIARY;
    let to_code_hash: B256 = Bytecode::new_eip7702(BENEFICIARY).hash_slow();
    let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_address(to) // target (position 1)
        .push_number(100_000_u64) // gas (position 0); remaining CALL operands missing
        .append(CALL)
        .append(STOP)
        .build();
    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code)
        .account_lazy_code(to, to_code_hash);
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_code_by_hash = Some(to_code_hash);

    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX6).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    match alloy_evm::Evm::transact_raw(&mut evm, tx) {
        Ok(result) => assert!(
            matches!(&result.result, ExecutionResult::Halt { reason, .. }
                if format!("{reason:?}").contains("StackUnderflow")),
            "partial-stack CALL whose target code fails to load must keep its StackUnderflow halt: \
             {:?}",
            result.result,
        ),
        Err(e) => panic!(
            "partial-stack CALL must keep StackUnderflow; the precheck's code-load failure was \
             stashed and surfaced as EVMError instead: {e:?}"
        ),
    }
}

/// REX6: a CALL to a beneficiary that is *itself* an EIP-7702 delegator must still revert under
/// disabled volatile access. The raw target is the beneficiary, so loading it observes beneficiary
/// state regardless of where it delegates — the guard must match on the raw target before resolving
/// the delegate (resolving first would yield the delegate `!= beneficiary` and miss the revert).
#[test]
fn test_rex6_call_to_beneficiary_that_delegates_elsewhere_still_reverts() {
    let middle_code = call_disable_volatile_data_access(BytecodeBuilder::default())
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(BENEFICIARY) // raw target IS the beneficiary
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(STOP)
        .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(MIDDLE, middle_code);
    // The beneficiary itself delegates to a non-beneficiary address.
    set_eip7702_delegation(&mut db, BENEFICIARY, EMPTY_NON_BENEFICIARY);

    let (result, _, _, _) =
        transact_with_beneficiary(MegaSpecId::REX6, &mut db, tx_to_middle()).unwrap();
    let output = result.result.output().expect("a VolatileDataAccessDisabled revert payload");
    assert_eq!(
        &output[..4],
        &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
        "a CALL to a beneficiary that delegates elsewhere must still revert (raw target matches): {:?}",
        result.result,
    );
}

/// Small helper: the standard `CALLER -> MIDDLE` transaction used across the CALL tests.
fn tx_to_middle() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill()
}
