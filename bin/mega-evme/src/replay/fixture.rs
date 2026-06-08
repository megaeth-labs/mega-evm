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

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    time::Duration,
};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_eips::Typed2718 as _;
use alloy_primitives::KECCAK256_EMPTY;
use alloy_rpc_types_eth::Block;
use mega_evm::{
    revm::{
        primitives::{Address, Bytes, HashMap as RevmHashMap, B256, U256},
        state::EvmState,
        DatabaseRef,
    },
    MegaSpecId,
};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::Transaction;
use state_test::{
    runner::{execute_unit_collect, time_unit_execution},
    types::{AccountInfo, Env, MegaEnv, SpecName, Test, TestSuite, TestUnit, TransactionParts},
};

use super::{ReplayError, Result};

/// Deposit transaction type byte (EIP-2718 `0x7e`). Deposit transactions carry
/// MegaETH/Optimism-specific fields (mint, source hash, system flag) that the
/// EEST schema cannot represent, so they cannot be dumped.
const DEPOSIT_TX_TYPE: u8 = 0x7e;

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
    /// Output observed during the full replay, cross-checked before writing.
    actual_output: Option<Bytes>,
    /// Suite key (transaction hash) under which the unit is stored.
    name: String,
}

/// Build a fixture draft from the replayed target transaction.
///
/// `db` must be read at the point *after* preceding transactions have committed
/// but *before* the target transaction commits, so that the pre-state closure
/// reflects exactly what the target transaction observed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_draft<DB>(
    db: &DB,
    evm_state: &EvmState,
    chain_id: u64,
    spec: MegaSpecId,
    block: &Block<Transaction>,
    target_tx: &Transaction,
    mega_env: MegaEnv,
    actual_gas: u64,
    actual_status: String,
    actual_output: Option<Bytes>,
    onchain_gas: u64,
) -> Result<FixtureDraft>
where
    DB: DatabaseRef,
    DB::Error: Display,
{
    let envelope: &OpTxEnvelope = &target_tx.inner.inner;
    if envelope.ty() == DEPOSIT_TX_TYPE {
        return Err(ReplayError::Other(
            "--dump-fixture does not support deposit transactions".to_string(),
        ));
    }

    // Fidelity gate: the local replay must reproduce the gas the transaction
    // actually used on-chain. A mismatch means the replay executed under the
    // wrong spec / hardfork config for this chain and block — self-validation
    // cannot catch this, because the fixture is validated under the same spec it
    // was dumped with. Refuse to build a fixture that does not match the chain.
    if actual_gas != onchain_gas {
        return Err(ReplayError::Other(format!(
            "replay gas {actual_gas} != on-chain receipt gas {onchain_gas}: the local \
             replay does not reproduce on-chain execution (likely a wrong spec or \
             hardfork config for chain {chain_id} at this block)"
        )));
    }

    let pre = build_pre_state(db, evm_state)?;
    let env = build_env(chain_id, block);
    let transaction = build_transaction(target_tx);
    let spec_name = SpecName::from_mega_spec(spec);
    if spec_name == SpecName::Unknown {
        return Err(ReplayError::Other(format!(
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
        mega_env: Some(mega_env),
    };

    let name = format!("replay_{:#x}", target_tx.inner.inner.tx_hash());

    Ok(FixtureDraft { unit, spec: spec_name, actual_gas, actual_status, actual_output, name })
}

/// Re-execute the isolated unit through `state-test`, cross-check it against the
/// observed replay outcome, fill the `post` expectation, and write the fixture.
pub(crate) fn finalize_and_write(draft: FixtureDraft, path: &std::path::Path) -> Result<()> {
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
    if executed.output != draft.actual_output {
        return Err(ReplayError::Other(
            "fixture not reproducible: output differs from replay".to_string(),
        ));
    }

    let mut unit = draft.unit;
    unit.out = executed.output.clone();
    let test =
        Test::for_dump(executed.state_root, executed.logs_root, executed.gas_used, executed.status);
    unit.post = BTreeMap::from([(draft.spec, vec![test])]);

    let suite = TestSuite(BTreeMap::from([(draft.name, unit)]));
    let json = serde_json::to_string_pretty(&suite)
        .map_err(|e| ReplayError::Other(format!("failed to serialize fixture: {e}")))?;
    std::fs::write(path, json)
        .map_err(|e| ReplayError::Other(format!("failed to write fixture {}: {e}", path.display())))
}

/// Throughput statistics from a `--bench-runs` measurement.
pub(crate) struct BenchStats {
    /// Number of timed iterations.
    pub runs: u32,
    /// Gas used by the (identical) target transaction.
    pub gas_used: u64,
    /// Fastest observed iteration.
    pub min: Duration,
    /// Median iteration time.
    pub median: Duration,
    /// Mean iteration time.
    pub mean: Duration,
}

impl BenchStats {
    /// Throughput in millions of gas per second, computed from the median time.
    pub(crate) fn mgas_per_sec(&self) -> f64 {
        let secs = self.median.as_secs_f64();
        if secs > 0.0 {
            self.gas_used as f64 / secs / 1.0e6
        } else {
            f64::INFINITY
        }
    }
}

impl FixtureDraft {
    /// Measure EVM throughput by re-executing the target transaction in isolation.
    ///
    /// First verifies that the isolated execution reproduces the replay's gas and
    /// status (so the measurement reflects the real transaction), then runs
    /// `warmup` discarded iterations followed by `runs` timed iterations.
    pub(crate) fn run_bench(&self, runs: u32, warmup: u32) -> Result<BenchStats> {
        let executed = execute_unit_collect(&self.unit, &self.spec)
            .map_err(|e| ReplayError::Other(format!("bench self-execution failed: {e}")))?;
        if executed.gas_used != self.actual_gas || executed.status != self.actual_status {
            return Err(ReplayError::Other(format!(
                "bench not reproducible: isolated ({}, {:?}) != replay ({}, {:?})",
                executed.gas_used, executed.status, self.actual_gas, self.actual_status
            )));
        }

        for _ in 0..warmup {
            time_unit_execution(&self.unit, &self.spec)
                .map_err(|e| ReplayError::Other(format!("bench warmup failed: {e}")))?;
        }

        let mut durations = Vec::with_capacity(runs.max(1) as usize);
        let mut gas_used = executed.gas_used;
        for _ in 0..runs {
            let (elapsed, gas, _status) = time_unit_execution(&self.unit, &self.spec)
                .map_err(|e| ReplayError::Other(format!("bench run failed: {e}")))?;
            durations.push(elapsed);
            gas_used = gas;
        }

        durations.sort_unstable();
        let min = durations[0];
        let median = durations[durations.len() / 2];
        let sum: Duration = durations.iter().sum();
        let mean = sum / durations.len() as u32;
        Ok(BenchStats { runs, gas_used, min, median, mean })
    }
}

/// Read the pre-execution values of every account in the target transaction's
/// read/write closure.
///
/// Accounts that did not exist before the transaction (created during execution)
/// are omitted from `pre`. Storage is recorded as each touched slot's
/// `original_value`, which is the value the transaction first read.
fn build_pre_state<DB>(db: &DB, evm_state: &EvmState) -> Result<HashMap<Address, AccountInfo>>
where
    DB: DatabaseRef,
    DB::Error: Display,
{
    let mut pre = HashMap::with_capacity(evm_state.len());
    for (address, account) in evm_state {
        let Some(info) = db
            .basic_ref(*address)
            .map_err(|e| ReplayError::Other(format!("pre-state read for {address}: {e}")))?
        else {
            // Account did not exist before the transaction — it is created during
            // execution and must not appear in `pre`.
            continue;
        };

        let code = resolve_code(db, info.code_hash, info.code.as_ref())?;

        let storage: RevmHashMap<U256, U256> = account
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
        parent_target_blobs_per_block: None,
        current_blob_gas_used: None,
    }
}

/// Build the EEST `transaction` (single-element index arrays) from the target tx.
fn build_transaction(target_tx: &Transaction) -> TransactionParts {
    let sender = target_tx.inner.inner.signer();
    let tx: &OpTxEnvelope = &target_tx.inner.inner;
    let tx_type = tx.ty();

    // Legacy / EIP-2930 carry a gas price; fee-market types carry max fees.
    let (gas_price, max_fee_per_gas) = match tx_type {
        0 | 1 => (Some(U256::from(tx.gas_price().unwrap_or(0))), None),
        _ => (None, Some(U256::from(tx.max_fee_per_gas()))),
    };

    TransactionParts {
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
    }
}
