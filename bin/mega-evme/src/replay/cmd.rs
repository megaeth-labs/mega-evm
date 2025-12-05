use std::time::Instant;

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::Block;
use clap::Parser;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{BlockEnv, ContextTr},
        context_interface::block::BlobExcessGasAndPrice,
        database::{states::bundle_state::BundleRetention, StateBuilder},
        DatabaseRef,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaSpecId,
};

use op_alloy_rpc_types::Transaction;

use crate::{
    common::{op_receipt_to_tx_receipt, EvmeOutcome, FixedHardfork, OpTxReceipt},
    run, EvmeState,
};

use super::{v1_0_1, ReplayError, Result};

/// Replay a transaction from RPC
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Transaction hash to replay
    #[arg(value_name = "TX_HASH")]
    pub tx_hash: B256,

    /// RPC URL to fetch transaction from
    #[arg(long = "rpc", default_value = "http://localhost:8545")]
    pub rpc: String,

    // Shared argument groups
    /// Chain configuration (hardfork and chain ID)
    #[command(flatten)]
    pub chain_args: run::ChainArgs,

    /// External environment configuration (bucket capacities)
    #[command(flatten)]
    pub ext_args: run::ExtEnvArgs,

    /// State dump configuration
    #[command(flatten)]
    pub dump_args: run::StateDumpArgs,

    /// Trace configuration
    #[command(flatten)]
    pub trace_args: run::TraceArgs,

    /// Use v1.0.1 of the mega-evm crate
    #[arg(long = "use-v1-0-1")]
    pub use_v1_0_1: bool,
}

/// Replay-specific execution outcome
#[allow(dead_code)]
pub(super) struct ReplayOutcome {
    /// Common execution outcome
    pub outcome: EvmeOutcome,
    /// The original transaction that was replayed
    pub original_tx: Transaction,
    /// The transaction receipt
    pub receipt: OpTxReceipt,
}

impl Cmd {
    /// Execute the replay command
    pub async fn run(&self) -> Result<()> {
        // Step 1: Fetch transaction from RPC
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_http(self.rpc.parse().map_err(|e| {
                ReplayError::RpcError(format!("Invalid RPC URL '{}': {}", self.rpc, e))
            })?);

        eprintln!("Fetching transaction {} from RPC {}", self.tx_hash, self.rpc);

        // Step 2: fetch transaction
        let target_tx = provider
            .get_transaction_by_hash(self.tx_hash)
            .await
            .map_err(|e| ReplayError::RpcError(format!("Failed to fetch transaction: {}", e)))?
            .ok_or_else(|| ReplayError::TransactionNotFound(self.tx_hash))?;

        eprintln!("Transaction found in block {:?}", target_tx.block_number);

        // Step 3: determine block number to execute the transaction
        let (state_base_block_number, block_number, is_pending) =
            if let Some(block_number) = target_tx.block_number {
                (block_number - 1, block_number, false)
            } else {
                let latest_block_number = provider
                    .get_block_number()
                    .await
                    .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?;
                (latest_block_number, latest_block_number, true)
            };

        // Step 4: Setup initial state by forking from the parent block
        let mut database = EvmeState::new_forked(
            provider.clone(),
            Some(state_base_block_number),
            Default::default(),
        )
        .await?;

        // Step 5: Setup BlockEnv and CfgEnv
        let parent_block = provider
            .get_block_by_number(state_base_block_number.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
            .ok_or(ReplayError::BlockNotFound(state_base_block_number))?;
        let block = provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
            .ok_or(ReplayError::BlockNotFound(block_number))?;
        let block_env = self.retrieve_block_env(&block).await?;
        let cfg_env = self.chain_args.create_cfg_env()?;
        let evm_env = EvmEnv::new(cfg_env, block_env);

        // Step 6: fetch preceding transactions
        let mut preceding_transactions = vec![];
        if !is_pending {
            for tx in block.transactions.hashes() {
                if tx == self.tx_hash {
                    break;
                }
                preceding_transactions.push(tx);
            }
        }

        // Step 7: Execute transactions with inspector
        let result = if self.use_v1_0_1 {
            eprintln!("execute tx v1.0.1");
            v1_0_1::execute_transactions_v1_0_1(
                &mut database,
                &parent_block,
                &block,
                evm_env,
                &provider,
                preceding_transactions,
                &target_tx,
                self.chain_args.spec_id()?,
                self.trace_args.tracer,
                self.trace_args.trace_disable_storage,
                self.trace_args.trace_disable_memory,
                self.trace_args.trace_disable_stack,
                self.trace_args.trace_enable_return_data,
            )
            .await?
        } else {
            self.execute_transactions(
                &mut database,
                &parent_block,
                &block,
                evm_env,
                &provider,
                preceding_transactions,
                &target_tx,
            )
            .await?
        };

        // Step 8: Output results
        self.output_results(&result)?;

        Ok(())
    }

    /// Execute transactions with block executor and optional tracing
    async fn execute_transactions<P>(
        &self,
        database: &mut run::EvmeState<op_alloy_network::Optimism, P>,
        parent_block: &Block<Transaction>,
        block: &Block<Transaction>,
        evm_env: mega_evm::alloy_evm::EvmEnv<MegaSpecId>,
        provider: &P,
        preceding_transactions: Vec<B256>,
        target_tx: &Transaction,
    ) -> Result<ReplayOutcome>
    where
        P: alloy_provider::Provider<op_alloy_network::Optimism> + std::fmt::Debug,
    {
        let chain_spec = FixedHardfork::new(self.chain_args.spec_id()?);
        let external_env_factory = self.ext_args.create_external_envs()?;
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_env_factory);
        let block_executor_factory = MegaBlockExecutorFactory::new(
            chain_spec,
            evm_factory,
            OpAlloyReceiptBuilder::default(),
        );
        let block_limits = match self.chain_args.spec_id()? {
            MegaSpecId::EQUIVALENCE => BlockLimits::no_limits().fit_equivalence(),
            MegaSpecId::MINI_REX => BlockLimits::no_limits().fit_mini_rex(),
            MegaSpecId::REX => BlockLimits::no_limits().fit_rex(),
            _ => panic!("Unsupported spec id: {:?}", self.chain_args.spec_id()),
        };
        let block_ctx = MegaBlockExecutionCtx::new(
            parent_block.hash(),
            block.header.parent_beacon_block_root(),
            block.header.extra_data().clone(),
            block_limits,
        );

        // Execute transactions with inspector (trace will be generated only if enabled)
        let start = Instant::now();

        // Setup tracing inspector
        let mut inspector = self.trace_args.create_inspector();

        // Create state and block executor with inspector
        let mut state = StateBuilder::new().with_database(database).with_bundle_update().build();
        let mut block_executor = block_executor_factory.create_executor_with_inspector(
            &mut state,
            block_ctx,
            evm_env,
            &mut inspector,
        );

        // Apply pre-execution changes
        block_executor
            .apply_pre_execution_changes()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;

        // Execute preceding transactions
        for tx_hash in preceding_transactions {
            let tx = provider
                .get_transaction_by_hash(tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {}", e)))?
                .ok_or(ReplayError::TransactionNotFound(tx_hash))?;
            let tx = tx.as_recovered();
            let outcome = block_executor
                .run_transaction(tx)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
            block_executor
                .commit_transaction_outcome(outcome)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        }

        // Execute target transaction
        let pre_execution_nonce = block_executor
            .evm()
            .db_ref()
            .basic_ref(target_tx.as_recovered().signer())?
            .map(|acc| acc.nonce)
            .unwrap_or(0);
        block_executor.inspector_mut().fuse();
        let outcome = block_executor
            .run_transaction(target_tx.as_recovered())
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        let exec_result = outcome.inner.result.clone();
        let evm_state = outcome.inner.state.clone();
        block_executor
            .commit_transaction_outcome(outcome)
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;

        let duration = start.elapsed();

        // Obtain receipt envelope
        let (evm, block_result) = block_executor
            .finish()
            .map_err(|e| ReplayError::Other(format!("Block execution error: {}", e)))?;
        let (db, _evm_env) = evm.finish();
        db.merge_transitions(BundleRetention::Reverts);
        let receipt_envelope = block_result.receipts.last().unwrap().clone();

        // Generate trace only if tracing is enabled
        let trace_data = if self.trace_args.is_tracing_enabled() {
            Some(self.trace_args.generate_trace(&inspector, &exec_result))
        } else {
            None
        };

        // Convert OpReceiptEnvelope to TransactionReceipt
        let from = target_tx.inner.inner.signer();
        let to = target_tx.inner.inner.to();
        let contract_address = if to.is_none() && receipt_envelope.is_success() {
            Some(from.create(pre_execution_nonce))
        } else {
            None
        };
        let receipt = op_receipt_to_tx_receipt(
            &receipt_envelope,
            block.number(),
            block.header.timestamp(),
            from,
            to,
            contract_address,
            target_tx.inner.effective_gas_price.unwrap_or(0),
        );

        Ok(ReplayOutcome {
            outcome: EvmeOutcome {
                pre_execution_nonce,
                exec_result,
                state: evm_state,
                exec_time: duration,
                trace_data,
            },
            original_tx: target_tx.clone(),
            receipt,
        })
    }

    async fn retrieve_block_env(&self, block: &Block<Transaction>) -> Result<BlockEnv> {
        Ok(BlockEnv {
            number: U256::from(block.number()),
            beneficiary: block.header.beneficiary(),
            timestamp: U256::from(block.header.timestamp()),
            gas_limit: block.header.gas_limit(),
            basefee: block.header.base_fee_per_gas().unwrap_or_default(),
            difficulty: block.header.difficulty(),
            prevrandao: block.header.mix_hash(),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
        })
    }

    /// Output execution results
    fn output_results(&self, result: &ReplayOutcome) -> Result<()> {
        // Serialize and print receipt as JSON
        let receipt_json = serde_json::to_string_pretty(&result.receipt)
            .map_err(|e| ReplayError::Other(format!("Failed to serialize receipt: {}", e)))?;
        println!("{}", receipt_json);

        // Print execution time to stderr
        eprintln!();
        eprintln!("execution time:  {:?}", result.outcome.exec_time);

        // Output trace data if available
        if let Some(ref trace) = result.outcome.trace_data {
            if let Some(ref output_file) = self.trace_args.trace_output_file {
                // Write trace to file
                std::fs::write(output_file, trace).map_err(|e| {
                    ReplayError::Other(format!("Failed to write trace to file: {}", e))
                })?;
                eprintln!();
                eprintln!("Trace written to: {}", output_file.display());
            } else {
                // Print trace to console
                eprintln!();
                eprintln!("=== Execution Trace ===");
                eprintln!("{}", trace);
            }
        }

        // Dump state if requested
        if self.dump_args.dump {
            self.dump_args.dump_evm_state(&result.outcome.state)?;
        }

        Ok(())
    }
}
