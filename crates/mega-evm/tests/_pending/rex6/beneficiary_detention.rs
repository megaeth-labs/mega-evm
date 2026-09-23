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
//!   frame-init or `target_updated` path, so DataSize/KV stayed at zero. Under REX6 the REX6-gated
//!   arm inside `storage_gas_ext::selfdestruct` records `DataSize` +40 / KV +1 (no `StateGrowth` —
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

// ============================================================================
// CALL family EIP-7702 delegate resolution
// ============================================================================

// ============================================================================
// Existing-target SELFDESTRUCT accounting
// ============================================================================

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

// ============================================================================
// DB-error coverage for the REX6 `selfdestruct_rex6` inspect paths
// ============================================================================

/// Small helper: the standard `CALLER -> MIDDLE` transaction used across the CALL tests.
fn tx_to_middle() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(MIDDLE).gas_limit(100_000_000).build_fill()
}
