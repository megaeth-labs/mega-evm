//! A block on the legacy engine, `mega-evm` 1.7.1.
//!
//! Everything of the legacy line is reached through `mega_evm_legacy`'s own re-exports, so no
//! legacy crate is named at a version of its own here. The block is configured as the 1.7.1
//! `replay` command configures the block of the transaction it replays: the chain's schedule,
//! the spec it gives at the block's timestamp unless one is forced, the block limits of the
//! hardfork active at the block, and a transaction runtime limit of the forced spec when one is.

use alloy_primitives::{Address, B256, U256};
use mega_evm_legacy::{
    alloy_consensus::transaction::Recovered,
    alloy_eips::{Decodable2718, Encodable2718},
    alloy_evm::{block::BlockExecutor, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    op_alloy_consensus::OpTxEnvelope,
    op_revm::OpHaltReason,
    revm::{
        context::{result::ExecutionResult, BlockEnv, CfgEnv},
        database::{DBErrorMarker, State},
        primitives::eip4844,
        state::{AccountInfo, Bytecode},
        Database,
    },
    AHashBucketHasher, BlockLimits, EvmTxRuntimeLimits, MegaBlockExecutionCtx,
    MegaBlockExecutorFactory, MegaEvmFactory, MegaHaltReason, MegaHardforks, MegaSpecId,
    TestExternalEnvs,
};

use super::{
    exec::{ExecStatus, ExecutedBlock, ReceiptData, TxResult},
    inputs::{HeaderFields, TxInput},
    state::{BlockState, StateReadError},
};
use crate::common::{decode_revert_reason, EvmeError, Result};

impl DBErrorMarker for StateReadError {}

/// The legacy engine's view of a [`BlockState`].
#[derive(Debug)]
struct LegacyDb<'a>(&'a mut BlockState);

impl Database for LegacyDb<'_> {
    type Error = StateReadError;

    fn basic(&mut self, address: Address) -> std::result::Result<Option<AccountInfo>, Self::Error> {
        Ok(self.0.account(address)?.map(|account| {
            AccountInfo::new(
                account.balance,
                account.nonce,
                account.code_hash,
                Bytecode::new_raw(account.code),
            )
        }))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> std::result::Result<Bytecode, Self::Error> {
        Ok(Bytecode::new_raw(self.0.code(code_hash)?))
    }

    fn storage(&mut self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        self.0.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> std::result::Result<B256, Self::Error> {
        self.0.block_hash(number)
    }
}

/// The legacy spec a block of `chain_id` at `timestamp` runs, or `spec_override`.
pub(super) fn resolve_spec(
    chain_id: u64,
    timestamp: u64,
    spec_override: Option<&str>,
) -> Result<String> {
    if let Some(name) = spec_override {
        let spec: MegaSpecId = name
            .parse()
            .map_err(|e| EvmeError::InvalidInput(format!("Invalid legacy spec {name:?}: {e:?}")))?;
        return Ok(spec.to_string());
    }
    Ok(mega_evm_legacy::hardfork_schedule(chain_id).spec_id(timestamp).to_string())
}

/// Executes the block `header` describes, with `transactions`, on the legacy engine over `state`.
pub(super) fn execute(
    chain_id: u64,
    header: &HeaderFields,
    transactions: &[TxInput],
    bucket_capacities: &[(u32, u64)],
    spec_override: Option<&str>,
    state: &mut BlockState,
) -> Result<ExecutedBlock> {
    let hardforks = mega_evm_legacy::hardfork_schedule(chain_id);
    let mut cfg = CfgEnv::default();
    cfg.chain_id = chain_id;
    cfg.spec = hardforks.spec_id(header.timestamp);
    let hardfork = hardforks.hardfork(header.timestamp).ok_or_else(|| {
        EvmeError::Other(format!("no MegaHardfork active at timestamp {}", header.timestamp))
    })?;
    let mut block_limits =
        BlockLimits::from_hardfork_and_block_gas_limit(hardfork, header.gas_limit);
    if let Some(name) = spec_override {
        let spec: MegaSpecId = name
            .parse()
            .map_err(|e| EvmeError::InvalidInput(format!("Invalid legacy spec {name:?}: {e:?}")))?;
        cfg.spec = spec;
        block_limits = block_limits.with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    }

    let mut block_env = BlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        gas_limit: header.gas_limit,
        basefee: header.base_fee,
        difficulty: header.difficulty,
        prevrandao: Some(header.mix_hash),
        blob_excess_gas_and_price: None,
    };
    block_env.set_blob_excess_gas_and_price(
        header.excess_blob_gas,
        eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
    );

    let mut envs = TestExternalEnvs::<std::convert::Infallible, AHashBucketHasher>::new();
    for &(bucket, capacity) in bucket_capacities {
        envs = envs.with_bucket_capacity(bucket, capacity);
    }
    let factory = MegaBlockExecutorFactory::new(
        &hardforks,
        MegaEvmFactory::new().with_external_env_factory(envs),
        OpAlloyReceiptBuilder::default(),
    );
    let block_ctx = MegaBlockExecutionCtx::new(
        header.parent_hash,
        header.parent_beacon_block_root,
        header.extra_data.clone(),
        block_limits,
    );

    let result = {
        let mut db = State::builder().with_database(LegacyDb(state)).with_bundle_update().build();
        let mut executor = factory.create_executor(&mut db, block_ctx, EvmEnv::new(cfg, block_env));
        let mut run = || -> Result<ExecutedBlock> {
            executor
                .apply_pre_execution_changes()
                .map_err(|e| EvmeError::Other(format!("pre-block changes: {e}")))?;
            let mut results = Vec::with_capacity(transactions.len());
            for tx in transactions {
                let envelope = OpTxEnvelope::decode_2718(&mut tx.envelope.as_ref())
                    .map_err(|e| EvmeError::Other(format!("decoding {}: {e}", tx.hash)))?;
                let recovered = Recovered::new_unchecked(envelope, tx.sender);
                let outcome = match executor.run_transaction(recovered.as_recovered_ref()) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        results.push(TxResult::Refused { reason: e.to_string() });
                        continue;
                    }
                };
                let (status, reason) = match &outcome.inner.result {
                    ExecutionResult::Success { .. } => (ExecStatus::Success, None),
                    ExecutionResult::Revert { output, .. } => {
                        (ExecStatus::Revert, Some(decode_revert_reason(output)))
                    }
                    ExecutionResult::Halt { reason, .. } => {
                        (ExecStatus::Halt, Some(halt_reason(reason)))
                    }
                };
                if let Err(e) = executor.commit_transaction_outcome(outcome) {
                    results.push(TxResult::Refused { reason: e.to_string() });
                    continue;
                }
                let receipt =
                    executor.receipts.last().expect("a committed transaction has a receipt");
                results.push(TxResult::Included {
                    status,
                    reason,
                    receipt: ReceiptData {
                        encoded: receipt.encoded_2718().into(),
                        success: receipt.status(),
                        cumulative_gas_used: receipt.cumulative_gas_used(),
                        logs: receipt.logs().to_vec(),
                    },
                    satin: None,
                });
            }
            Ok(ExecutedBlock { transactions: results })
        };
        run()
    };
    if let Some(miss) = &state.miss {
        return Err(EvmeError::Other(format!("block {}: {miss}", header.number)));
    }
    result
}

/// A legacy halt reason, printed as the 1.7.1 tool prints it.
fn halt_reason(reason: &MegaHaltReason) -> String {
    match reason {
        MegaHaltReason::Base(OpHaltReason::Base(eth)) => format!("{eth:?}"),
        MegaHaltReason::Base(op) => format!("{op:?}"),
        other => format!("{other:?}"),
    }
}
