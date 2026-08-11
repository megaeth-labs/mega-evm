//! Build a self-validating EEST state-test fixture from a replayed transaction.
//!
//! `mega-evme replay --dump-fixture <FILE>` produces a single [`TestUnit`] JSON
//! file that captures everything needed to deterministically re-execute the
//! target transaction in isolation: the pre-state read closure, block
//! environment, transaction, and `MegaETH` external environment (SALT bucket
//! capacities, oracle storage).
//!
//! The recorded `post` roots are computed by `state-test`'s own
//! [`execute_unit_collect`] — the exact code path that validates the fixture —
//! so a dumped fixture is self-consistent by construction: re-running it through
//! the state-test runner reproduces the same roots. As a safety net, the
//! dump cross-checks the gas, status, and output computed from the isolated
//! fixture against the values observed during the full replay, and refuses to
//! write a fixture that does not reproduce them (e.g. an incomplete pre-state
//! closure or an unsupported transaction shape).

use std::{collections::BTreeMap, fmt::Display};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_eips::Typed2718 as _;
use alloy_primitives::KECCAK256_EMPTY;
use alloy_rpc_types_eth::Block;
use mega_evm::{
    revm::{
        context::result::ExecutionResult,
        primitives::{Address, Bytes, B256, U256},
        state::EvmState,
        DatabaseRef,
    },
    MegaHaltReason, MegaSpecId,
};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::Transaction;
use state_test::{
    runner::{execute_unit_collect, execution_status, halt_reason},
    types::{AccountInfo, Env, MegaEnv, SpecName, Test, TestSuite, TestUnit, TransactionParts},
};

use super::{ReplayError, Result};

/// Why [`build_draft`] refused to produce a fixture.
///
/// The two variants carry different consequences, so the distinction is typed
/// rather than recovered from the message: a whole-block sweep always meets some
/// transactions the fixture format cannot express, and those must not fail the
/// run, whereas a failure to construct a draft the caller asked for must.
pub(crate) enum FixtureBuildError {
    /// The transaction, spec, or replay is outside what a fixture can express
    /// (deposit and set-code transactions, specs with no fixture mapping, a
    /// replay that does not reproduce the chain). Reported as a skip.
    Unsupported(String),
    /// The draft could not be built (pre-state or code read failed). The
    /// requested artifact was not produced; reported as an error.
    Construction(ReplayError),
}

impl Display for FixtureBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => f.write_str(reason),
            Self::Construction(err) => write!(f, "{err}"),
        }
    }
}

impl From<FixtureBuildError> for ReplayError {
    fn from(err: FixtureBuildError) -> Self {
        match err {
            FixtureBuildError::Unsupported(reason) => Self::Other(reason),
            FixtureBuildError::Construction(err) => err,
        }
    }
}

/// The on-chain receipt values a dumped fixture is anchored to: a replay that
/// does not reproduce all of these did not reproduce the on-chain transaction.
pub(crate) struct OnchainAnchor {
    /// Gas the transaction used on-chain.
    pub gas_used: u64,
    /// Whether the transaction succeeded on-chain.
    pub success: bool,
    /// RLP-hash of the on-chain receipt's logs (the same `log_rlp_hash` the
    /// state-test runner records as `logsRoot`), so the replay's logs can be
    /// checked against the chain, not just its gas and status.
    pub logs_root: B256,
}

/// The fixture-specific inputs gathered during a replay: the `MegaETH` external
/// environment snapshot, the target transaction's execution result, and the
/// on-chain receipt it is anchored to.
///
/// Bundling these keeps the fixture's gas, status, and output derived from a
/// single `ExecutionResult` — there is no second place that recomputes the
/// status string, so the dumped and validated values cannot drift.
pub(crate) struct FixtureInputs<'a> {
    /// Effective `MegaETH` external environment (SALT buckets, oracle storage).
    pub mega_env: MegaEnv,
    /// The target transaction's execution result from the full replay.
    pub result: &'a ExecutionResult<MegaHaltReason>,
    /// The on-chain receipt this replay is checked against — the fidelity anchor.
    pub anchor: OnchainAnchor,
}

/// Deposit transaction type byte (EIP-2718 `0x7e`). Deposit transactions carry
/// MegaETH/Optimism-specific fields (mint, source hash, system flag) that the
/// EEST schema cannot represent, so they cannot be dumped.
const DEPOSIT_TX_TYPE: u8 = 0x7e;

/// EIP-7702 set-code transaction type byte (`0x04`). The fixture builder does not
/// yet serialize the authorization list, so dropping it would silently change
/// execution (the delegations would not apply). Reject these explicitly rather
/// than emit a fixture whose isolated run diverges from the chain.
const EIP7702_TX_TYPE: u8 = 0x04;

/// Check that a local replay reproduces the on-chain receipt's gas, success
/// status, and logs root.
///
/// A mismatch means the replay executed under the wrong spec / hardfork config
/// for this chain and block; self-validation cannot catch this, because the
/// fixture is validated under the same spec it was dumped with.
///
/// Logs are checked, not just inferred from gas: LOG gas depends on topic count
/// and data length, never content, so two executions can burn identical gas yet
/// emit different log payloads (e.g. a preceding-tx divergence that changes a
/// value the target re-emits).
///
/// Returns the explanatory reason on failure so batch dump can record a skip
/// without treating it as an infrastructure error.
pub(crate) fn check_fidelity(
    result: &ExecutionResult<MegaHaltReason>,
    anchor: &OnchainAnchor,
    chain_id: u64,
) -> std::result::Result<(), String> {
    let actual_gas = result.tx_gas_used();
    if actual_gas != anchor.gas_used {
        return Err(format!(
            "replay gas {actual_gas} != on-chain receipt gas {}: the local replay does \
             not reproduce on-chain execution (likely a wrong spec or hardfork config \
             for chain {chain_id} at this block)",
            anchor.gas_used
        ));
    }
    if result.is_success() != anchor.success {
        return Err(format!(
            "replay status (success={}) != on-chain receipt status (success={}): the \
             local replay does not reproduce on-chain execution for chain {chain_id}",
            result.is_success(),
            anchor.success
        ));
    }
    let actual_logs_root = state_test::utils::log_rlp_hash(result.logs());
    if actual_logs_root != anchor.logs_root {
        return Err(format!(
            "replay logs root {actual_logs_root} != on-chain receipt logs root {}: the \
             local replay emits different logs than the chain for chain {chain_id} \
             (same gas/status, different log contents)",
            anchor.logs_root
        ));
    }
    Ok(())
}

/// Build an [`OnchainAnchor`] from the consensus facts of an on-chain receipt.
pub(crate) fn anchor_from_receipt_facts(facts: &super::verify::ReceiptFacts) -> OnchainAnchor {
    OnchainAnchor {
        gas_used: facts.gas_used,
        success: facts.status,
        logs_root: state_test::utils::log_rlp_hash(&facts.logs),
    }
}

/// A fixture built from a replay, awaiting its `post` expectation.
///
/// The `post` map is filled by [`finalize_and_write`] after re-executing the
/// isolated unit through `state-test`.
pub(crate) struct FixtureDraft {
    /// The assembled unit (pre / env / transaction / megaEnv); `post` is empty.
    unit: TestUnit,
    /// Spec the transaction executed under — becomes the `post` map key.
    spec: SpecName,
    /// Gas observed during the full replay, cross-checked before writing.
    actual_gas: u64,
    /// Status observed during the full replay, cross-checked before writing.
    actual_status: String,
    /// Halt reason observed during the full replay (`None` unless the status is
    /// `"halt"`), cross-checked before writing: two different halts typically
    /// burn the same full gas with no output and no logs, so the coarse status
    /// comparison alone cannot tell them apart.
    actual_halt_reason: Option<String>,
    /// Output observed during the full replay, cross-checked before writing.
    actual_output: Option<Bytes>,
    /// Logs root observed during the full replay (already equal to the on-chain
    /// receipt's logs root), cross-checked against the isolated run before writing.
    actual_logs_root: B256,
    /// Suite key (transaction hash) under which the unit is stored.
    name: String,
}

/// Build a fixture draft from the replayed target transaction.
///
/// `db` must be read at the point *after* preceding transactions have committed
/// but *before* the target transaction commits, so that the pre-state closure
/// reflects exactly what the target transaction observed.
pub(crate) fn build_draft<DB>(
    db: &DB,
    evm_state: &EvmState,
    chain_id: u64,
    spec: MegaSpecId,
    block: &Block<Transaction>,
    target_tx: &Transaction,
    inputs: FixtureInputs<'_>,
) -> std::result::Result<FixtureDraft, FixtureBuildError>
where
    DB: DatabaseRef,
    DB::Error: Display,
{
    let envelope: &OpTxEnvelope = &target_tx.inner.inner;
    if envelope.ty() == DEPOSIT_TX_TYPE {
        return Err(FixtureBuildError::Unsupported(
            "--dump-fixture does not support deposit transactions".to_string(),
        ));
    }
    if envelope.ty() == EIP7702_TX_TYPE {
        return Err(FixtureBuildError::Unsupported(
            "--dump-fixture does not support EIP-7702 (set-code) transactions: the \
             fixture builder does not serialize the authorization list"
                .to_string(),
        ));
    }

    let actual_gas = inputs.result.tx_gas_used();
    let actual_status = execution_status(inputs.result).to_string();
    let actual_halt_reason = halt_reason(inputs.result);
    let actual_output = inputs.result.output().cloned();
    let actual_logs_root = state_test::utils::log_rlp_hash(inputs.result.logs());

    // Fidelity gate: refuse to dump a fixture that does not match the chain.
    // See [`check_fidelity`] for the rationale and the dimensions checked.
    // Every rejection it can return is an unsupported replay, not a construction
    // failure, so the classification does not depend on which one fired.
    check_fidelity(inputs.result, &inputs.anchor, chain_id)
        .map_err(FixtureBuildError::Unsupported)?;

    let pre = build_pre_state(db, evm_state).map_err(FixtureBuildError::Construction)?;
    let env = build_env(chain_id, block);
    let transaction = build_transaction(target_tx)?;
    let spec_name = SpecName::from_mega_spec(spec);
    if spec_name == SpecName::Unknown {
        return Err(FixtureBuildError::Unsupported(format!(
            "--dump-fixture: spec {spec:?} has no fixture mapping"
        )));
    }

    let unit = TestUnit {
        info: None,
        env,
        pre,
        post: BTreeMap::new(),
        transaction,
        out: None,
        mega_env: Some(inputs.mega_env),
        extra: BTreeMap::new(),
    };

    let name = format!("replay_{:#x}", target_tx.inner.inner.tx_hash());

    Ok(FixtureDraft {
        unit,
        spec: spec_name,
        actual_gas,
        actual_status,
        actual_halt_reason,
        actual_output,
        actual_logs_root,
        name,
    })
}

/// Re-execute the isolated unit through `state-test`, cross-check it against the
/// observed replay outcome, fill the `post` expectation, and write the fixture.
///
/// `overwrite` controls the final publish step: when false, the write refuses to
/// replace an existing file (`persist_noclobber`); when true, it replaces via
/// `persist`. Existence checks earlier in the dump pipeline are a fast-path only
/// — correctness against a concurrent creator comes from the noclobber publish.
pub(crate) fn finalize_and_write(
    draft: FixtureDraft,
    path: &std::path::Path,
    overwrite: bool,
) -> Result<()> {
    let executed = execute_unit_collect(&draft.unit, &draft.spec)
        .map_err(|e| ReplayError::Other(format!("fixture self-execution failed: {e}")))?;

    // Cross-check the isolated execution against the full replay. These values
    // are independent of the L1 data fee (which state-test zeroes), so they must
    // match exactly; a mismatch means the fixture would not reproduce the replay.
    if executed.gas_used != draft.actual_gas {
        return Err(ReplayError::Other(format!(
            "fixture not reproducible: gas {} != replay gas {} (incomplete pre-state?)",
            executed.gas_used, draft.actual_gas
        )));
    }
    if executed.status != draft.actual_status {
        return Err(ReplayError::Other(format!(
            "fixture not reproducible: status {:?} != replay status {:?}",
            executed.status, draft.actual_status
        )));
    }
    // A halted target needs more than the coarse status: two different halt
    // reasons typically burn the same full gas with no output and no logs, so
    // without this check the fixture would bake in the wrong failure mode.
    if executed.halt_reason != draft.actual_halt_reason {
        return Err(ReplayError::Other(format!(
            "fixture not reproducible: halt reason {:?} != replay halt reason {:?}",
            executed.halt_reason, draft.actual_halt_reason
        )));
    }
    if executed.output != draft.actual_output {
        return Err(ReplayError::Other(
            "fixture not reproducible: output differs from replay".to_string(),
        ));
    }
    // Unlike gas/status/output, the logs root is sensitive to the L1 data fee: the
    // full replay charges it (reducing the sender's balance), the isolated run
    // zeroes it, so a log that reflects the sender's balance can diverge here even
    // when everything else matches. `actual_logs_root` already equals the on-chain
    // receipt's logs root, so this also pins the baked `post` logs to the chain.
    if executed.logs_root != draft.actual_logs_root {
        return Err(ReplayError::Other(format!(
            "fixture not reproducible: logs root {} != replay logs root {} \
             (isolated run diverges from the full replay, e.g. via the L1 data fee)",
            executed.logs_root, draft.actual_logs_root
        )));
    }

    let mut unit = draft.unit;
    unit.out = executed.output.clone();
    let test =
        Test::for_dump(executed.state_root, executed.logs_root, executed.gas_used, executed.status);
    unit.post = BTreeMap::from([(draft.spec, vec![test])]);

    let suite = TestSuite(BTreeMap::from([(draft.name, unit)]));
    let json = serde_json::to_string_pretty(&suite)
        .map_err(|e| ReplayError::Other(format!("failed to serialize fixture: {e}")))?;

    // Unique temp file in the target directory, then persist (or noclobber-persist)
    // into `path`. A fixed sibling name would race two concurrent dumps; a unique
    // name plus noclobber makes `--overwrite=false` safe at materialization time.
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        ReplayError::Other(format!("failed to create temp fixture file in {}: {e}", dir.display()))
    })?;
    use std::io::Write;
    tmp.write_all(json.as_bytes())
        .map_err(|e| ReplayError::Other(format!("failed to write fixture temp file: {e}")))?;
    tmp.flush()
        .map_err(|e| ReplayError::Other(format!("failed to flush fixture temp file: {e}")))?;
    // flush() only clears the userspace buffer; the rename below is atomic but
    // the contents are not. A benchmark corpus that a crash left holding a
    // truncated fixture would fail in a way that looks like a replay bug.
    tmp.as_file()
        .sync_all()
        .map_err(|e| ReplayError::Other(format!("failed to sync fixture temp file: {e}")))?;
    if overwrite {
        tmp.persist(path).map_err(|e| {
            ReplayError::Other(format!(
                "failed to persist fixture to {}: {}",
                path.display(),
                e.error
            ))
        })?;
    } else {
        tmp.persist_noclobber(path).map_err(|e| {
            // Target already present (or appeared between prep and publish): same
            // refused-overwrite path the prep-time existence check uses.
            if path.exists() {
                ReplayError::Other(format!(
                    "fixture already exists at {} (pass --overwrite to replace)",
                    path.display()
                ))
            } else {
                ReplayError::Other(format!(
                    "failed to persist fixture to {}: {}",
                    path.display(),
                    e.error
                ))
            }
        })?;
    }
    Ok(())
}

/// Read the pre-execution values of every account in the target transaction's
/// read/write closure.
///
/// Accounts that did not exist before the transaction (created during execution)
/// are omitted from `pre`. Storage is recorded as each touched slot's
/// `original_value`, which is the value the transaction first read.
fn build_pre_state<DB>(db: &DB, evm_state: &EvmState) -> Result<BTreeMap<Address, AccountInfo>>
where
    DB: DatabaseRef,
    DB::Error: Display,
{
    // Test-only injection: the offline State cache reuses account basics already
    // loaded during execution, so doctoring the capture cannot force a draft-only
    // pre-state failure. Integration tests set this env var to exercise the
    // construction-error path after a successful execution. Compiled out of
    // production builds: only the test profile and the `test-utils` feature
    // (enabled for the binary via the self dev-dependency) carry the hook.
    #[cfg(any(test, feature = "test-utils"))]
    if std::env::var_os("MEGA_EVME_INJECT_FIXTURE_PRE_STATE_ERROR").is_some() {
        return Err(ReplayError::Other(
            "pre-state read for 0x0000000000000000000000000000000000000001: \
             injected draft-time database failure"
                .to_string(),
        ));
    }

    let mut pre = BTreeMap::new();
    for (address, account) in evm_state {
        let Some(info) = db
            .basic_ref(*address)
            .map_err(|e| ReplayError::Other(format!("pre-state read for {address}: {e}")))?
        else {
            // The database reports no account. On a forked replay the RPC
            // backend normalizes an all-zero (balance, nonce, code) answer to
            // `None` (see `normalize_rpc_account` in `common/state.rs`), so
            // this branch fires for every pre-transaction nonexistent account
            // — including accounts the target transaction itself creates.
            // Omitting them is correct state-test semantics: absence in `pre`
            // means the account did not exist.
            continue;
        };

        let code = resolve_code(db, info.code_hash, info.code.as_ref())?;

        // BTreeMap (not HashMap) so storage slots serialize in a deterministic
        // order, keeping the dumped fixture byte-reproducible.
        let storage: BTreeMap<U256, U256> = account
            .storage
            .iter()
            .filter(|(_, slot)| !slot.original_value.is_zero())
            .map(|(key, slot)| (*key, slot.original_value))
            .collect();

        pre.insert(
            *address,
            AccountInfo { balance: info.balance, code, nonce: info.nonce, storage },
        );
    }
    Ok(pre)
}

/// Resolve an account's bytecode bytes, fetching by hash if not materialized.
fn resolve_code<DB>(
    db: &DB,
    code_hash: B256,
    code: Option<&mega_evm::revm::state::Bytecode>,
) -> Result<Bytes>
where
    DB: DatabaseRef,
    DB::Error: Display,
{
    if let Some(bytecode) = code {
        return Ok(bytecode.original_byte_slice().to_vec().into());
    }
    if code_hash == KECCAK256_EMPTY {
        return Ok(Bytes::new());
    }
    let bytecode = db
        .code_by_hash_ref(code_hash)
        .map_err(|e| ReplayError::Other(format!("code fetch for {code_hash}: {e}")))?;
    Ok(bytecode.original_byte_slice().to_vec().into())
}

/// Build the EEST `env` from the replayed block header.
fn build_env(chain_id: u64, block: &Block<Transaction>) -> Env {
    let header = &block.header;
    Env {
        current_chain_id: Some(U256::from(chain_id)),
        current_coinbase: header.beneficiary(),
        current_difficulty: header.difficulty(),
        current_gas_limit: U256::from(header.gas_limit()),
        current_number: U256::from(header.number()),
        current_timestamp: U256::from(header.timestamp()),
        current_base_fee: header.base_fee_per_gas().map(U256::from),
        current_random: header.mix_hash().map(|h| U256::from_be_bytes(h.0)),
        current_excess_blob_gas: header.excess_blob_gas().map(U256::from),
        // Remaining fields are not needed to reproduce a single transaction.
        previous_hash: None,
        parent_timestamp: None,
        parent_gas_used: None,
        parent_gas_limit: None,
        parent_base_fee: None,
        parent_hash: None,
        parent_uncle_hash: None,
        parent_beacon_block_root: None,
        parent_difficulty: None,
        block_hashes: None,
        ommers: None,
        withdrawals: None,
        current_beacon_root: None,
        current_withdrawals_root: None,
        parent_blob_gas_used: None,
        parent_excess_blob_gas: None,
        current_blob_gas_used: None,
    }
}

/// Build the EEST `transaction` (single-element index arrays) from the target tx.
fn build_transaction(
    target_tx: &Transaction,
) -> std::result::Result<TransactionParts, FixtureBuildError> {
    let sender = target_tx.inner.inner.signer();
    let tx: &OpTxEnvelope = &target_tx.inner.inner;
    let tx_type = tx.ty();

    // Legacy / EIP-2930 carry a gas price; fee-market types carry max fees.
    // A type-0/1 envelope always carries a gas price, but if that invariant
    // ever broke, falling back to 0 would bake a wrong price into a fixture
    // that still self-validates — so refuse instead of guessing.
    let (gas_price, max_fee_per_gas) = match tx_type {
        0 | 1 => {
            let gas_price = tx.gas_price().ok_or_else(|| {
                FixtureBuildError::Unsupported(format!(
                    "--dump-fixture: transaction type {tx_type} reports no gas price; \
                     refusing to record a guessed price in the fixture"
                ))
            })?;
            (Some(U256::from(gas_price)), None)
        }
        _ => (None, Some(U256::from(tx.max_fee_per_gas()))),
    };

    Ok(TransactionParts {
        tx_type: Some(tx_type),
        data: vec![tx.input().clone()],
        gas_limit: vec![U256::from(tx.gas_limit())],
        gas_price,
        nonce: U256::from(tx.nonce()),
        secret_key: B256::ZERO,
        sender: Some(sender),
        to: tx.to(),
        value: vec![tx.value()],
        max_fee_per_gas,
        max_priority_fee_per_gas: tx.max_priority_fee_per_gas().map(U256::from),
        initcodes: None,
        access_lists: vec![tx.access_list().cloned()],
        authorization_list: None,
        blob_versioned_hashes: tx.blob_versioned_hashes().map(|h| h.to_vec()).unwrap_or_default(),
        max_fee_per_blob_gas: tx.max_fee_per_blob_gas().map(U256::from),
    })
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::Sealed;
    use mega_evm::revm::{
        context::result::{Output, ResultGas, SuccessReason},
        primitives::{StorageKey, StorageValue},
        state::{AccountInfo as RevmAccountInfo, Bytecode},
    };
    use op_alloy_consensus::TxDeposit;

    use super::*;

    fn success_result(gas_used: u64) -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default().with_total_gas_spent(gas_used),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        }
    }

    /// A database that fails every read.
    ///
    /// Any `build_draft` rejection that fires *before* the pre-state closure is
    /// read must be reachable with this: if a rejection ever moved behind a
    /// database read, the test would surface a `Construction` error instead.
    struct UnreadableDb;

    impl DatabaseRef for UnreadableDb {
        type Error = crate::common::EvmeError;

        fn basic_ref(
            &self,
            _: Address,
        ) -> std::result::Result<Option<RevmAccountInfo>, Self::Error> {
            Err(unavailable())
        }

        fn code_by_hash_ref(&self, _: B256) -> std::result::Result<Bytecode, Self::Error> {
            Err(unavailable())
        }

        fn storage_ref(
            &self,
            _: Address,
            _: StorageKey,
        ) -> std::result::Result<StorageValue, Self::Error> {
            Err(unavailable())
        }

        fn block_hash_ref(&self, _: u64) -> std::result::Result<B256, Self::Error> {
            Err(unavailable())
        }
    }

    fn unavailable() -> crate::common::EvmeError {
        crate::common::EvmeError::InvalidInput("database unavailable".to_string())
    }

    /// A database whose `basic_ref` answers are supplied per address.
    ///
    /// Used to pin the `build_pre_state` shape for both existence outcomes:
    /// a touched address that returns `None` is omitted from `pre`, and a
    /// touched address that returns `Some` is recorded with its fields.
    struct MapDb {
        accounts: std::collections::HashMap<Address, Option<RevmAccountInfo>>,
    }

    impl DatabaseRef for MapDb {
        type Error = crate::common::EvmeError;

        fn basic_ref(
            &self,
            address: Address,
        ) -> std::result::Result<Option<RevmAccountInfo>, Self::Error> {
            Ok(self.accounts.get(&address).cloned().unwrap_or(None))
        }

        fn code_by_hash_ref(&self, _: B256) -> std::result::Result<Bytecode, Self::Error> {
            Err(unavailable())
        }

        fn storage_ref(
            &self,
            _: Address,
            _: StorageKey,
        ) -> std::result::Result<StorageValue, Self::Error> {
            Err(unavailable())
        }

        fn block_hash_ref(&self, _: u64) -> std::result::Result<B256, Self::Error> {
            Err(unavailable())
        }
    }

    /// Touched addresses with no pre-transaction account are omitted from `pre`;
    /// touched addresses that exist are recorded with their fields — including
    /// an explicitly present-but-empty account (`Some(AccountInfo::default())`).
    ///
    /// Absence-means-nonexistence is the state-test fixture shape: a forked
    /// backend returns `None` for all-zero RPC answers, so accounts created by
    /// the target transaction must not appear as explicit empty entries.
    /// Presence of an empty account is a different DB answer and must still be
    /// recorded; `build_pre_state` does not filter empties with `is_empty()`.
    #[test]
    fn test_build_pre_state_omits_nonexistent_and_records_existing() {
        let missing = Address::repeat_byte(0xaa);
        let present = Address::repeat_byte(0xbb);
        let empty_present = Address::repeat_byte(0xcc);
        let balance = U256::from(42u64);
        let nonce = 7u64;

        let mut accounts = std::collections::HashMap::new();
        accounts.insert(missing, None);
        accounts.insert(
            present,
            Some(RevmAccountInfo {
                balance,
                nonce,
                code_hash: KECCAK256_EMPTY,
                code: Some(Bytecode::default()),
                ..Default::default()
            }),
        );
        // Present-but-empty: the DB returns Some with zero fields. This must
        // stay in `pre` so a future `is_empty()` filter cannot creep in.
        accounts.insert(empty_present, Some(RevmAccountInfo::default()));
        let db = MapDb { accounts };

        let mut evm_state = EvmState::default();
        evm_state.insert(missing, Default::default());
        evm_state.insert(present, Default::default());
        evm_state.insert(empty_present, Default::default());

        let pre = build_pre_state(&db, &evm_state).expect("pre-state construction succeeds");

        assert!(
            !pre.contains_key(&missing),
            "touched + basic_ref=None must be absent from pre (nonexistence)"
        );
        let recorded = pre.get(&present).expect("touched + basic_ref=Some must appear in pre");
        assert_eq!(recorded.balance, balance);
        assert_eq!(recorded.nonce, nonce);
        assert!(recorded.code.is_empty());
        assert!(recorded.storage.is_empty());

        let empty_recorded = pre
            .get(&empty_present)
            .expect("touched + basic_ref=Some(default) must appear in pre (empty is not absent)");
        assert_eq!(empty_recorded.balance, U256::ZERO);
        assert_eq!(empty_recorded.nonce, 0);
        assert!(empty_recorded.code.is_empty());
        assert!(empty_recorded.storage.is_empty());
    }

    fn deposit_transaction() -> Transaction {
        let envelope = OpTxEnvelope::Deposit(Sealed::new(TxDeposit::default()));
        let inner = alloy_rpc_types_eth::Transaction {
            inner: Recovered::new_unchecked(envelope, Address::ZERO),
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_index: None,
            effective_gas_price: None,
        };
        Transaction { inner, deposit_nonce: None, deposit_receipt_version: None }
    }

    /// A deposit transaction is an unsupported shape, not a construction failure.
    ///
    /// Every OP-stack block opens with one, so misclassifying this would make
    /// `--block N --dump-fixture-dir` exit non-zero on every block instead of
    /// skipping the transaction the fixture format cannot express. The database
    /// here fails every read, which proves the rejection is reached without
    /// touching state — a `Construction` verdict would mean the check moved.
    #[test]
    fn test_build_draft_rejects_a_deposit_as_unsupported() {
        let result = success_result(21_000);
        let anchor = OnchainAnchor {
            gas_used: 21_000,
            success: true,
            logs_root: state_test::utils::log_rlp_hash(&[]),
        };
        let err = build_draft(
            &UnreadableDb,
            &EvmState::default(),
            4326,
            MegaSpecId::REX6,
            &Block::default(),
            &deposit_transaction(),
            FixtureInputs { mega_env: MegaEnv::default(), result: &result, anchor },
        )
        .err()
        .expect("a deposit cannot be dumped");
        match err {
            FixtureBuildError::Unsupported(reason) => {
                assert!(reason.contains("deposit"), "reason={reason}");
            }
            FixtureBuildError::Construction(err) => {
                panic!("a deposit is an unsupported shape, not a construction failure: {err}")
            }
        }
    }

    /// A failing pre-state read is a construction error, not a skip.
    ///
    /// The counterpart to the deposit case: this rejection means the artifact
    /// the caller asked for was not produced, so the run must fail rather than
    /// report a skip and exit 0.
    #[test]
    fn test_build_draft_reports_a_failed_pre_state_read_as_construction() {
        let result = success_result(21_000);
        let anchor = OnchainAnchor {
            gas_used: 21_000,
            success: true,
            logs_root: state_test::utils::log_rlp_hash(&[]),
        };
        // One touched account is enough to force a `basic_ref` during the
        // pre-state closure; the transaction itself is a plain legacy call.
        let mut evm_state = EvmState::default();
        evm_state.insert(Address::repeat_byte(0x11), Default::default());
        let envelope = OpTxEnvelope::Eip1559(alloy_consensus::Signed::new_unchecked(
            alloy_consensus::TxEip1559::default(),
            alloy_primitives::Signature::new(U256::ONE, U256::ONE, false),
            B256::ZERO,
        ));
        let inner = alloy_rpc_types_eth::Transaction {
            inner: Recovered::new_unchecked(envelope, Address::ZERO),
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_index: None,
            effective_gas_price: None,
        };
        let tx = Transaction { inner, deposit_nonce: None, deposit_receipt_version: None };
        let err = build_draft(
            &UnreadableDb,
            &evm_state,
            4326,
            MegaSpecId::REX6,
            &Block::default(),
            &tx,
            FixtureInputs { mega_env: MegaEnv::default(), result: &result, anchor },
        )
        .err()
        .expect("the pre-state read fails");
        match err {
            FixtureBuildError::Construction(err) => {
                let message = err.to_string();
                assert!(message.contains("pre-state read"), "message={message}");
            }
            FixtureBuildError::Unsupported(reason) => {
                panic!("a failed database read is not an unsupported shape: {reason}")
            }
        }
    }

    /// Each of the three fidelity dimensions rejects on its own.
    ///
    /// [`build_draft`] wraps every rejection at one `map_err` site, so all three
    /// are reported as [`FixtureBuildError::Unsupported`] — a whole-block sweep
    /// skips a diverging replay rather than failing the run, whichever dimension
    /// diverged. Only the gas and status messages share a phrase; a classifier
    /// keyed on message text would have had to enumerate the third separately.
    #[test]
    fn test_check_fidelity_rejects_each_dimension() {
        let logs_root = state_test::utils::log_rlp_hash(&[]);
        let matching = OnchainAnchor { gas_used: 21_000, success: true, logs_root };
        let result = success_result(21_000);
        check_fidelity(&result, &matching, 4326).expect("a faithful replay must pass the gate");

        let cases = [
            ("gas", OnchainAnchor { gas_used: 42_000, ..matching }),
            ("status", OnchainAnchor { success: false, ..matching }),
            ("logs root", OnchainAnchor { logs_root: B256::repeat_byte(0xab), ..matching }),
        ];
        let mut reasons = Vec::new();
        for (dimension, anchor) in cases {
            let Err(reason) = check_fidelity(&result, &anchor, 4326) else {
                panic!("a {dimension} divergence must be rejected");
            };
            assert!(!reason.is_empty(), "{dimension} rejection must explain itself");
            reasons.push(reason);
        }
        assert_eq!(
            reasons.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3,
            "each dimension explains its own divergence: {reasons:?}"
        );
    }
}
