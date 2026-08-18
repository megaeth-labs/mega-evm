//! Shared helpers for the REX7 test suite.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, MegaTransactionNew as _, MegaTransactionOutcome, TestExternalEnvs,
};
use revm::{
    context::{result::ExecutionResult, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
    state::EvmState,
};
use std::collections::BTreeMap;

/// Transaction sender.
pub(crate) const CALLER: Address = address!("0000000000000000000000000000000000300000");
/// Contract invoked by the transaction; its code exercises the opcodes under test.
pub(crate) const CONTRACT: Address = address!("0000000000000000000000000000000000300001");
/// A second contract, used as the target of internal CALL-family frames.
pub(crate) const CALLEE: Address = address!("0000000000000000000000000000000000300002");
/// A spare empty address used as a value-transfer / SELFDESTRUCT target.
pub(crate) const EMPTY_TARGET: Address = address!("0000000000000000000000000000000000300003");

/// One ether, in wei.
pub(crate) const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// The post-transaction readings compared across specs.
pub(crate) struct Outcome {
    pub(crate) result: ExecutionResult<MegaHaltReason>,
    /// Post-tx compute-gas tracker reading (`get_usage().compute_gas`).
    pub(crate) compute_gas: u64,
    /// Post-tx data-size tracker reading (`get_usage().data_size`).
    pub(crate) data_size: u64,
    /// Post-tx KV-update tracker reading (`get_usage().kv_updates`).
    pub(crate) kv_updates: u64,
    /// Post-tx state-growth tracker reading (`get_usage().state_growth`).
    pub(crate) state_growth: u64,
    /// Receipt `gas_used` (combined compute + storage EVM gas).
    pub(crate) gas_used: u64,
    /// The part of [`compute_gas`](Self::compute_gas) an exceptionally halted frame destroyed
    /// rather than performed (REX7+, else 0).
    pub(crate) destroyed: u64,
    /// Post-tx enforced compute gas — the part of [`compute_gas`](Self::compute_gas) every limit
    /// comparison and the block's admission counter run against.
    pub(crate) enforced_lane: u64,
    /// Receipt envelope before the EIP-3529 refund and the EIP-7623 floor: exactly the number
    /// settlement derives the destroyed total from.
    pub(crate) total_gas_spent: u64,
    /// Post-tx detained compute gas limit — equal to the configured TX limit unless volatile
    /// access lowered it.
    pub(crate) detained_compute_gas_limit: u64,
    /// Post-tx non-compute EVM gas — the `MegaETH` storage gas and sandbox residue the destroyed
    /// derivation subtracts. Signed: the sandbox boundary contributes a difference, not a charge.
    pub(crate) non_compute_gas: i128,
    /// Post-tx `CALL_STIPEND` total minted into child frames by value-transferring calls.
    pub(crate) minted_call_stipend: u64,
    /// Post-tx sum of the per-site destroyed bookings — the second opinion the derived
    /// [`destroyed`](Self::destroyed) is cross-checked against, never the reported number.
    pub(crate) booked_destroyed: u64,
    /// The state the transaction produced.
    pub(crate) state: EvmState,
}

impl Outcome {
    pub(crate) fn is_success(&self) -> bool {
        self.result.is_success()
    }

    /// The halt reason, or a panic with `label` when the transaction did not halt.
    pub(crate) fn halt_reason(&self, label: &str) -> &MegaHaltReason {
        match &self.result {
            ExecutionResult::Halt { reason, .. } => reason,
            other => panic!("{label}: expected a halt, got {other:?}"),
        }
    }

    /// The part of the reported compute total that a resource limit is evaluated against.
    ///
    /// Read from the tracker's own lane rather than subtracted from the reported total; the two
    /// agree because [`assert_terminal_identity`] checks that they do on every transaction the
    /// helpers in this module run.
    pub(crate) fn enforced(&self) -> u64 {
        self.enforced_lane
    }

    /// Reads a storage slot out of the produced state, defaulting to zero when the transaction
    /// never touched it.
    pub(crate) fn storage_value(&self, address: Address, slot: U256) -> U256 {
        self.state
            .get(&address)
            .and_then(|account| account.storage.get(&slot))
            .map(|value| value.present_value())
            .unwrap_or_default()
    }
}

/// The transaction gas limit [`transact`] runs with — high enough that EVM gas is never the
/// binding constraint.
pub(crate) const DEFAULT_TX_GAS_LIMIT: u64 = 100_000_000;

/// Runs a single transaction that calls [`CONTRACT`] under `spec` with the given DB and runtime
/// limits, returning the execution result plus the post-tx tracker readings and `gas_used`.
pub(crate) fn transact(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> Outcome {
    transact_with_gas_limit(spec, db, limits, DEFAULT_TX_GAS_LIMIT)
}

/// [`transact`] with an explicit transaction gas limit, for cases that need EVM gas itself to run
/// out.
pub(crate) fn transact_with_gas_limit(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
) -> Outcome {
    let mut context = MegaContext::new(&mut db, spec).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(gas_limit).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let outcome = evm.execute_transaction(tx).expect("tx should not surface EVMError");
    let (detained_compute_gas_limit, non_compute_gas, minted_call_stipend, booked_destroyed) = {
        let additional_limit = evm.ctx_ref().additional_limit.borrow();
        let (non_compute_gas, minted_call_stipend, booked_destroyed) =
            additional_limit.conservation_terms_for_test();
        (
            additional_limit.detained_compute_gas_limit(),
            non_compute_gas,
            minted_call_stipend,
            booked_destroyed,
        )
    };
    finish(
        spec,
        outcome,
        detained_compute_gas_limit,
        non_compute_gas,
        minted_call_stipend,
        booked_destroyed,
    )
}

/// Assembles an [`Outcome`] from what a transaction reported and checks the terminal identity
/// before handing it back.
///
/// Every helper in this module funnels through here, so every REX7 transaction the suite runs —
/// not just the ones written to look at gas — is a check that the tracker lanes reconcile with the
/// receipt the transaction produced.
pub(crate) fn finish(
    spec: MegaSpecId,
    outcome: MegaTransactionOutcome,
    detained_compute_gas_limit: u64,
    non_compute_gas: i128,
    minted_call_stipend: u64,
    booked_destroyed: u64,
) -> Outcome {
    let gas_used = outcome.result_and_state.result.tx_gas_used();
    let total_gas_spent = outcome.result_and_state.result.gas().total_gas_spent();
    let outcome = Outcome {
        result: outcome.result_and_state.result,
        compute_gas: outcome.compute_gas_used,
        data_size: outcome.data_size,
        kv_updates: outcome.kv_updates,
        state_growth: outcome.state_growth_used,
        gas_used,
        destroyed: outcome.compute_gas_destroyed,
        enforced_lane: outcome.compute_gas_enforced,
        total_gas_spent,
        detained_compute_gas_limit,
        non_compute_gas,
        minted_call_stipend,
        booked_destroyed,
        state: outcome.result_and_state.state,
    };
    assert_terminal_identity(spec, &outcome);
    outcome
}

/// The identity every REX7 transaction that produces a receipt must satisfy, connecting what the
/// trackers hold to the number the receipt reports.
///
/// # The identity
///
/// For one transaction, write
///
/// ```text
/// C = compute_gas         reported compute total
/// E = enforced_lane       the part limits and block admission compare against
/// D = destroyed           the part that is reported and accounted but never enforced
/// N = non_compute_gas     MegaETH storage gas plus the sandbox boundary residue (signed)
/// M = minted_call_stipend CALL_STIPEND minted into child frames and never debited from a caller
/// S = total_gas_spent     the receipt envelope, before the refund and the floor
/// R = the receipt's raw refund
/// F = the receipt's EIP-7623 floor gas
/// ```
///
/// then
///
/// ```text
/// (1)  C = E + D
/// (2)  C + N − M = S
/// (3)  receipt gas_used = max(S − R, F)
/// ```
///
/// (1) is the split of the reported total. (2) is the conservation law rearranged: settlement
/// defines `D = S + M − N − E`, so `S = D + E + N − M`, and substituting (1) gives `S = C + N − M`.
/// (3) is how a receipt's gas number is built from its envelope.
///
/// # Why (2) needs no refund or floor correction
///
/// The EIP-3529 refund and the EIP-7623 floor both move the number the receipt reports without
/// anyone having burnt the difference. Both are applied strictly after the envelope is final, and
/// both are carried on the result as their own fields rather than folded into the envelope, so
/// anchoring on `S` — the same value settlement reads — keeps them out of the identity entirely.
/// Substituting (2) into (3) gives the receipt-level form, which is what a reader normally wants:
///
/// ```text
/// receipt gas_used = max(C + N − M − R, F)
/// ```
///
/// # What it catches
///
/// (2) fails whenever a transaction's envelope moves without a `MegaETH` site accounting for it —
/// a settlement that never ran, a result rewritten after settlement, an upstream subsidy nobody
/// records. (1) fails when the reported split disagrees with the per-site bookings, which is what
/// the block's admission counter reads. Pre-REX7 specs have neither a destroyed lane nor a
/// non-compute lane, so the identity is REX7-only by construction.
fn assert_terminal_identity(spec: MegaSpecId, outcome: &Outcome) {
    if !spec.is_enabled(MegaSpecId::REX7) {
        return;
    }
    assert_eq!(
        outcome.compute_gas,
        outcome.enforced_lane + outcome.destroyed,
        "reported compute gas must split into enforced + destroyed; \
         compute={} enforced={} destroyed={} result={:?}",
        outcome.compute_gas,
        outcome.enforced_lane,
        outcome.destroyed,
        outcome.result,
    );
    let accounted = i128::from(outcome.compute_gas) + outcome.non_compute_gas -
        i128::from(outcome.minted_call_stipend);
    assert_eq!(
        accounted,
        i128::from(outcome.total_gas_spent),
        "the tracker lanes must account for the whole receipt envelope; \
         compute={} non_compute={} minted_stipend={} accounted={accounted} envelope={} \
         (receipt gas_used={}) result={:?}",
        outcome.compute_gas,
        outcome.non_compute_gas,
        outcome.minted_call_stipend,
        outcome.total_gas_spent,
        outcome.gas_used,
        outcome.result,
    );
}

/// Runs [`transact`] with the spec's default runtime limits.
pub(crate) fn transact_default(spec: MegaSpecId, db: MemoryDatabase) -> Outcome {
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec))
}

/// The external environment [`transact_tx`] runs with when a test does not need SALT buckets or
/// oracle storage of its own. Equivalent to the empty environment the other helpers use: every
/// bucket reports the minimum capacity and the oracle has no data.
pub(crate) fn default_envs() -> TestExternalEnvs {
    TestExternalEnvs::new()
}

/// The general entry point: an explicit transaction and an explicit external environment.
///
/// The other helpers in this module fix the transaction to a plain call into [`CONTRACT`]; the
/// shapes that need a different one — EIP-7702 authorizations, system-originated callers, direct
/// calls into a system contract — build their own `TxEnv` and come through here. `envs` is borrowed
/// so a test can read back what execution recorded into it (oracle hints, for instance).
pub(crate) fn transact_tx(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: TxEnv,
    envs: &TestExternalEnvs,
) -> Outcome {
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    transact_mega_tx(spec, db, limits, tx, envs)
}

/// [`transact_tx`] for the shapes that need the `MegaETH` transaction itself, not just its
/// `TxEnv` — a deposit's `source_hash` and `mint` live on the outer type.
pub(crate) fn transact_mega_tx(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: MegaTransaction,
    envs: &TestExternalEnvs,
) -> Outcome {
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let outcome = evm.execute_transaction(tx).expect("tx should not surface EVMError");
    let (detained_compute_gas_limit, non_compute_gas, minted_call_stipend, booked_destroyed) = {
        let additional_limit = evm.ctx_ref().additional_limit.borrow();
        let (non_compute_gas, minted_call_stipend, booked_destroyed) =
            additional_limit.conservation_terms_for_test();
        (
            additional_limit.detained_compute_gas_limit(),
            non_compute_gas,
            minted_call_stipend,
            booked_destroyed,
        )
    };
    finish(
        spec,
        outcome,
        detained_compute_gas_limit,
        non_compute_gas,
        minted_call_stipend,
        booked_destroyed,
    )
}

/// The part of an account a transaction's state actually asserts.
///
/// Raw [`EvmState`] cannot be compared directly: `Account::transaction_id` and each storage slot's
/// `is_cold` are journal bookkeeping with no consensus meaning, and two runs that produce identical
/// state can still differ there. This keeps the account info, the deployed code, the status flags
/// that decide how the account is applied, and every slot's original/present pair.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AccountView {
    balance: U256,
    nonce: u64,
    code_hash: B256,
    code: Bytes,
    touched: bool,
    created: bool,
    selfdestructed: bool,
    loaded_as_not_existing: bool,
    storage: BTreeMap<U256, (U256, U256)>,
}

/// Normalises an [`EvmState`] into a stable, order-independent view.
pub(crate) fn state_view(state: &EvmState) -> BTreeMap<Address, AccountView> {
    state
        .iter()
        .map(|(address, account)| {
            let view = AccountView {
                balance: account.info.balance,
                nonce: account.info.nonce,
                code_hash: account.info.code_hash,
                code: account
                    .info
                    .code
                    .as_ref()
                    .map(|code| code.original_bytes())
                    .unwrap_or_default(),
                touched: account.is_touched(),
                created: account.is_created(),
                selfdestructed: account.is_selfdestructed(),
                loaded_as_not_existing: account.is_loaded_as_not_existing(),
                storage: account
                    .storage
                    .iter()
                    .map(|(slot, value)| (*slot, (value.original_value, value.present_value)))
                    .collect(),
            };
            (*address, view)
        })
        .collect()
}

/// Asserts that two outcomes are indistinguishable: same execution result, same four-dimension
/// usage, same receipt `gas_used`, the same detained compute-gas limit, and the same state.
///
/// This is the precision invariant in assertion form — what a transaction that stays inside every
/// per-tx limit must produce under both accounting models.
pub(crate) fn assert_outcomes_identical(label: &str, r6: &Outcome, r7: &Outcome) {
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "{label}: execution result must be identical; REX6={:?} REX7={:?}",
        r6.result,
        r7.result
    );
    assert_eq!(
        r6.compute_gas, r7.compute_gas,
        "{label}: compute gas must be identical; REX6={} REX7={}",
        r6.compute_gas, r7.compute_gas
    );
    assert_eq!(
        r6.gas_used, r7.gas_used,
        "{label}: receipt gas_used must be identical; REX6={} REX7={}",
        r6.gas_used, r7.gas_used
    );
    assert_eq!(
        (r6.data_size, r6.kv_updates, r6.state_growth),
        (r7.data_size, r7.kv_updates, r7.state_growth),
        "{label}: the non-compute dimensions must be identical",
    );
    assert_eq!(
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit,
        "{label}: the detained compute-gas limit must be identical; REX6={} REX7={}",
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit
    );
    let (s6, s7) = (state_view(&r6.state), state_view(&r7.state));
    if s6 != s7 {
        // Report the first address the two disagree on; dumping both whole states buries it.
        let mut addresses: Vec<&Address> = s6.keys().chain(s7.keys()).collect();
        addresses.sort_unstable();
        addresses.dedup();
        let culprit = addresses
            .into_iter()
            .find(|address| s6.get(*address) != s7.get(*address))
            .expect("the maps differ, so some address must");
        panic!(
            "{label}: the produced state must be identical; {culprit} is\n  REX6: {:?}\n  REX7: {:?}",
            s6.get(culprit),
            s7.get(culprit),
        );
    }
}

/// [`transact`] with every SALT bucket reporting `bucket_capacity`.
///
/// The SALT-scaled storage-gas charges (`SSTORE` set, new account, contract creation) are
/// `base × (capacity / MIN_BUCKET_SIZE − 1)`, so only a capacity above
/// [`mega_evm::MIN_BUCKET_SIZE`] makes them non-zero and exercises the paths that have to
/// exclude them from the compute-gas window.
pub(crate) fn transact_with_bucket_capacity(
    spec: MegaSpecId,
    mut db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    bucket_capacity: u64,
) -> Outcome {
    let envs = TestExternalEnvs::default().with_default_bucket_capacity(bucket_capacity);
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(100_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let outcome = evm.execute_transaction(tx).expect("tx should not surface EVMError");
    let (detained_compute_gas_limit, non_compute_gas, minted_call_stipend, booked_destroyed) = {
        let additional_limit = evm.ctx_ref().additional_limit.borrow();
        let (non_compute_gas, minted_call_stipend, booked_destroyed) =
            additional_limit.conservation_terms_for_test();
        (
            additional_limit.detained_compute_gas_limit(),
            non_compute_gas,
            minted_call_stipend,
            booked_destroyed,
        )
    };
    finish(
        spec,
        outcome,
        detained_compute_gas_limit,
        non_compute_gas,
        minted_call_stipend,
        booked_destroyed,
    )
}
