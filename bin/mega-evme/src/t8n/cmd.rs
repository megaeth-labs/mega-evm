use std::path::PathBuf;

use clap::Parser;
use mega_evm::{MegaContext, MegaEvm, MegaSpecId, MegaTransaction};
use revm::{
    context::{block::BlockEnv, cfg::CfgEnv, either::Either, tx::TxEnv},
    database::{CacheState, EmptyDB, State},
    primitives::{hardfork::SpecId, Bytes, TxKind, B256},
    state::Bytecode,
    ExecuteCommitEvm,
};
use state_test::types::Env;

use crate::t8n::{
    calculate_logs_bloom, calculate_logs_root, calculate_state_root,
    extract_post_state_alloc_from_state, load_alloc, load_env, load_from_stdin, load_transactions,
    recover_address_from_secret_key, write_alloc_to_file, write_body_output, write_result_to_file,
    RejectedTx, Result, StateAlloc, T8nError, T8nOutput, Transaction, TransactionLog,
    TransactionReceipt, TransitionInputs, TransitionResults,
};

/// Executes a full state transition
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Configures the use of the JSON opcode tracer. This tracer emits traces to files
    /// as trace-<txIndex>-<txHash>.jsonl
    #[arg(long)]
    pub trace: bool,

    /// The configurations for the custom tracer specified by --trace.tracer. If
    /// provided, must be in JSON format
    #[arg(long = "trace.jsonconfig")]
    pub trace_jsonconfig: Option<String>,

    /// Enable full memory dump in traces
    #[arg(long = "trace.memory")]
    pub trace_memory: bool,

    /// Disable stack output in traces
    #[arg(long = "trace.nostack")]
    pub trace_nostack: bool,

    /// Enable return data output in traces
    #[arg(long = "trace.returndata")]
    pub trace_returndata: bool,

    /// Enable call frames output in traces
    #[arg(long = "trace.callframes")]
    pub trace_callframes: bool,

    /// Specifies where output files are placed. Will be created if it does not exist.
    #[arg(long = "output.basedir")]
    pub output_basedir: Option<PathBuf>,

    /// Determines where to put the `alloc` of the post-state.
    /// `stdout` - into the stdout output
    /// `stderr` - into the stderr output
    /// <file> - into the file <file>
    #[arg(long = "output.alloc", default_value = "alloc.json")]
    pub output_alloc: String,

    /// Determines where to put the `result` (stateroot, txroot etc) of the post-state.
    /// `stdout` - into the stdout output
    /// `stderr` - into the stderr output
    /// <file> - into the file <file>
    #[arg(long = "output.result", default_value = "result.json")]
    pub output_result: String,

    /// If set, the RLP of the transactions (block body) will be written to this file.
    #[arg(long = "output.body")]
    pub output_body: Option<String>,

    /// File name of where to find the prestate alloc to use.
    #[arg(long = "input.alloc", default_value = "stdin")]
    pub input_alloc: String,

    /// File name of where to find the prestate env to use.
    #[arg(long = "input.env", default_value = "stdin")]
    pub input_env: String,

    /// File name of where to find the transactions to apply. If the file
    /// extension is '.rlp', then the data is interpreted as an RLP list of signed
    /// transactions. The '.rlp' format is identical to the output.body format.
    #[arg(long = "input.txs", default_value = "stdin")]
    pub input_txs: String,

    /// Name of ruleset to use.
    #[arg(long = "state.fork", value_parser, default_value_t = MegaSpecId::MINI_REX)]
    pub fork: MegaSpecId,

    /// `ChainID` to use
    #[arg(long = "state.chainid", default_value = "6342")]
    pub chain_id: u64,

    /// Mining reward. Set to -1 to disable
    #[arg(long = "state.reward", default_value = "0")]
    pub reward: i64,
}

impl Cmd {
    /// Execute the state transition in three main steps:
    /// 1. Load inputs (alloc, env, txs)
    /// 2. Run EVM state transition
    /// 3. Output results
    pub fn run(&self) -> Result<()> {
        // Step 1: Load inputs
        let inputs = self.load_inputs()?;

        // Step 2: Run EVM state transition
        let results = self.run_evm_transition(inputs)?;

        // Step 3: Output results
        self.output_results(results)?;

        Ok(())
    }

    /// Step 1: Load input files (alloc.json, env.json, txs.json) or from stdin
    fn load_inputs(&self) -> Result<TransitionInputs> {
        // Check if we should read from stdin (when any input is "stdin")
        if self.input_alloc == "stdin" || self.input_env == "stdin" || self.input_txs == "stdin" {
            return load_from_stdin();
        }

        // Load prestate allocation
        let alloc = load_alloc(&self.input_alloc)?;

        // Load block environment
        let env = load_env(&self.input_env)?;

        // Load transactions
        let txs = load_transactions(&self.input_txs)?;

        Ok(TransitionInputs { alloc, env, txs })
    }

    /// Step 2: Execute state transition using the EVM
    fn run_evm_transition(&self, inputs: TransitionInputs) -> Result<TransitionResults> {
        // Setup configuration
        let mut cfg = CfgEnv::default();
        cfg.chain_id = self.chain_id;
        cfg.spec = self.fork;

        // Setup block environment
        let block = self.create_block_env(&inputs.env);

        // Setup state from prestate allocation
        let mut state = self.create_initial_state(&inputs.alloc)?;

        // Execute transactions sequentially
        let mut total_gas_used = 0u64;
        let mut all_logs = Vec::new();
        let mut receipts = Vec::new();
        let mut rejected = Vec::new();

        for (tx_index, tx_data) in inputs.txs.iter().enumerate() {
            // Convert transaction to TxEnv
            let tx_env = match self.convert_transaction_to_env(tx_data) {
                Ok(env) => env,
                Err(e) => {
                    rejected.push(RejectedTx {
                        index: tx_index as u64,
                        error: format!("Failed to convert transaction: {:?}", e),
                    });

                    // Create failed receipt
                    let receipt = TransactionReceipt {
                        status: Some(0),
                        cumulative_gas_used: total_gas_used,
                        logs: Vec::new(),
                        transaction_hash: None, // TODO: Add transaction hash
                        gas_used: Some(0),
                        root: None,
                        logs_bloom: None,
                        contract_address: None,
                        effective_gas_price: None,
                        block_hash: None,
                        transaction_index: None,
                        blob_gas_used: None,
                        blob_gas_price: None,
                        delegations: None,
                    };
                    receipts.push(receipt);
                    continue;
                }
            };

            // Create EVM context and transaction
            let evm_context = MegaContext::default()
                .with_db(&mut state)
                .with_cfg(cfg.clone())
                .with_block(block.clone());

            let mut tx = MegaTransaction::new(tx_env.clone());
            tx.enveloped_tx = Some(Bytes::default());

            // Execute transaction
            let mut evm = MegaEvm::new(evm_context);
            let exec_result = evm.transact_commit(tx);

            match &exec_result {
                Ok(result) => {
                    let tx_gas_used = result.gas_used();
                    total_gas_used += tx_gas_used;

                    // Determine if execution was successful based on execution result type
                    let is_success =
                        matches!(result, revm::context::result::ExecutionResult::Success { .. });

                    // Only add logs if execution was successful
                    if is_success {
                        all_logs.extend_from_slice(result.logs());
                    }

                    // Create receipt with status based on execution result
                    let receipt = TransactionReceipt {
                        status: Some(if is_success { 1 } else { 0 }),
                        cumulative_gas_used: total_gas_used,
                        logs: if is_success {
                            result
                                .logs()
                                .to_vec()
                                .into_iter()
                                .enumerate()
                                .map(|(log_index, log)| {
                                    let (topics, data) = log.data.split();
                                    TransactionLog {
                                        address: log.address,
                                        topics,
                                        data,
                                        block_number: 0,
                                        transaction_hash: B256::default(), // TODO: Add transaction hash
                                        transaction_index: tx_index as u64,
                                        block_hash: B256::default(),
                                        log_index: log_index as u64,
                                        removed: false,
                                    }
                                })
                                .collect()
                        } else {
                            Vec::new()
                        },
                        transaction_hash: None,
                        gas_used: Some(tx_gas_used),
                        root: None,
                        logs_bloom: None,
                        contract_address: None,
                        effective_gas_price: None,
                        block_hash: None,
                        transaction_index: None,
                        blob_gas_used: None,
                        blob_gas_price: None,
                        delegations: None,
                    };
                    receipts.push(receipt);
                }
                Err(e) => {
                    // For failed transactions, we still update the state but mark as failed
                    let receipt = TransactionReceipt {
                        status: Some(0),
                        cumulative_gas_used: total_gas_used, // Don't add gas for failed tx
                        logs: Vec::new(),
                        transaction_hash: None,
                        gas_used: Some(0),
                        root: None,
                        logs_bloom: None,
                        contract_address: None,
                        effective_gas_price: None,
                        block_hash: None,
                        transaction_index: None,
                        blob_gas_used: None,
                        blob_gas_price: None,
                        delegations: None,
                    };
                    receipts.push(receipt);

                    rejected.push(RejectedTx { index: tx_index as u64, error: format!("{:?}", e) });
                }
            }
        }

        // Calculate bloom filter from all logs
        let logs_bloom = calculate_logs_bloom(&all_logs);

        // Calculate roots
        let state_root = calculate_state_root(&state);
        let tx_root = B256::default(); // TODO: Calculate transaction trie root
        let receipts_root = B256::default(); // TODO: Calculate receipts root
        let logs_hash = calculate_logs_root(&all_logs);

        // Extract post-state allocation
        let post_state_alloc = extract_post_state_alloc_from_state(&state);

        Ok(TransitionResults {
            state_root,
            tx_root,
            receipts_root,
            logs_hash,
            logs_bloom,
            receipts,
            rejected,
            difficulty: inputs.env.current_difficulty,
            gas_used: total_gas_used,
            base_fee: inputs.env.current_base_fee.unwrap_or_default(),
            post_state_alloc,
        })
    }

    /// Create block environment from the input environment
    fn create_block_env(&self, env: &Env) -> BlockEnv {
        let mut block = BlockEnv {
            number: env.current_number,
            beneficiary: env.current_coinbase,
            timestamp: env.current_timestamp,
            gas_limit: env.current_gas_limit.try_into().unwrap_or(u64::MAX),
            basefee: env.current_base_fee.unwrap_or_default().try_into().unwrap_or(u64::MAX),
            difficulty: env.current_difficulty,
            prevrandao: env.current_random.map(|i| i.into()),
            blob_excess_gas_and_price: None,
        };

        // Set blob excess gas from currentExcessBlobGas if available
        if let Some(current_excess_blob_gas) = env.current_excess_blob_gas {
            block.set_blob_excess_gas_and_price(
                current_excess_blob_gas.to(),
                revm::primitives::eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        }

        block
    }

    /// Create initial state from prestate allocation
    fn create_initial_state(&self, alloc: &StateAlloc) -> Result<State<EmptyDB>> {
        // Determine state clear flag based on EVM spec (Spurious Dragon and later)
        let has_state_clear = self.fork.into_eth_spec().is_enabled_in(SpecId::SPURIOUS_DRAGON);
        let mut cache_state = CacheState::new(has_state_clear);

        for (address, info) in alloc {
            let bytecode = Bytecode::new_raw_checked(info.code.clone())
                .unwrap_or_else(|_| Bytecode::new_legacy(info.code.clone()));
            let code_hash = bytecode.hash_slow();

            let acc_info = revm::state::AccountInfo {
                balance: info.balance,
                code_hash,
                code: Some(bytecode),
                nonce: info.nonce,
            };

            cache_state.insert_account_with_storage(*address, acc_info, info.storage.clone());
        }

        Ok(State::builder().with_cached_prestate(cache_state).with_bundle_update().build())
    }

    /// Convert Transaction to `TxEnv`
    fn convert_transaction_to_env(&self, tx: &Transaction) -> Result<TxEnv> {
        // Determine sender from secret_key if provided, otherwise use signature recovery
        let caller = if let Some(secret_key) = tx.secret_key {
            recover_address_from_secret_key(&secret_key)?
        } else {
            // TODO: Implement signature recovery from v, r, s
            return Err(T8nError::InvalidTransaction(
                "Missing secret key for transaction".to_string(),
            ));
        };

        Ok(TxEnv {
            caller,
            gas_price: tx.gas_price.or(tx.max_fee_per_gas).unwrap_or_default().into(),
            gas_priority_fee: tx.max_priority_fee_per_gas.map(|b| b.into()),
            blob_hashes: tx.blob_versioned_hashes.clone(),
            max_fee_per_blob_gas: tx
                .max_fee_per_blob_gas
                .map(|b| u128::try_from(b).expect("max fee less than u128::MAX"))
                .unwrap_or(u128::MAX),
            tx_type: tx.tx_type.unwrap_or(0),
            gas_limit: tx.gas,
            data: tx.data.clone(),
            nonce: tx.nonce,
            value: tx.value,
            access_list: tx.access_list.clone().unwrap_or_default(),
            authorization_list: tx
                .authorization_list
                .clone()
                .map(|auth_list| auth_list.into_iter().map(Either::Left).collect::<Vec<_>>())
                .unwrap_or_default(),
            kind: match tx.to {
                Some(addr) => TxKind::Call(addr),
                None => TxKind::Create,
            },
            chain_id: tx.chain_id.map(|c| c.to()),
        })
    }

    /// Step 3: Write output files (result.json, alloc.json)
    fn output_results(&self, results: TransitionResults) -> Result<()> {
        // Create T8N output format with alloc and result
        let t8n_output =
            T8nOutput { alloc: results.post_state_alloc.clone(), result: results.clone() };

        // Always print result to stdout as JSON (default behavior)
        let result_json = serde_json::to_string_pretty(&t8n_output)
            .map_err(|e| T8nError::JsonParse { file: "stdout".to_string(), source: e })?;
        println!("{}", result_json);

        // Additionally write to files if specified
        if self.output_result != "stdout" {
            write_result_to_file(&results, &self.output_result, self.output_basedir.as_ref())?;
        }

        if self.output_alloc != "stdout" {
            write_alloc_to_file(
                &results.post_state_alloc,
                &self.output_alloc,
                self.output_basedir.as_ref(),
            )?;
        }

        // Write body.rlp if requested
        if let Some(ref body_file) = self.output_body {
            write_body_output(body_file, self.output_basedir.as_ref())?;
        }

        Ok(())
    }
}
