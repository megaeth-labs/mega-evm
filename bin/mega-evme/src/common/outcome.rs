//! Execution outcome and output formatting for mega-evme commands

use std::{
    path::Path,
    time::{Duration, Instant},
};

use super::{EnvArgs, EvmeError, EvmeState, StateDumpArgs, TraceArgs};

use alloy_consensus::{Eip658Value, Receipt};
use alloy_primitives::{hex, Address, BlockHash, Bytes, TxHash, TxKind};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_sol_types::{Panic, Revert, SolError};
use clap::Parser;
use mega_evm::{
    op_revm::OpHaltReason,
    revm::{context::result::ExecutionResult, state::EvmState, DatabaseRef},
    MegaHaltReason, MegaTransaction, MegaTxType,
};
use op_alloy_consensus::{OpDepositReceipt, OpReceiptEnvelope};
use serde::Serialize;

/// OP-stack transaction receipt type alias
pub type OpTxReceipt = TransactionReceipt<OpReceiptEnvelope<alloy_rpc_types_eth::Log>>;

/// Common execution outcome for all evme commands
#[derive(Debug)]
pub struct EvmeOutcome {
    /// The nonce of the sender before execution
    pub pre_execution_nonce: u64,
    /// The EVM execution result
    pub exec_result: ExecutionResult<mega_evm::MegaHaltReason>,
    /// The post-execution EVM state
    pub state: EvmState,
    /// Time taken to execute
    pub exec_time: Duration,
    /// Optional trace data (if tracing was enabled)
    pub trace_data: Option<String>,
}

impl EvmeOutcome {
    /// Execute `tx` on `state` under the command-line environment, timing the
    /// execution.
    ///
    /// This is the tail `run` and `tx` share once each has prepared its
    /// transaction and read the sender's nonce: each command keeps its own
    /// input-preparation order, so a failure, an RPC request or a cache read
    /// happens where it always did.
    pub fn execute<N, P>(
        state: &mut EvmeState<N, P>,
        env_args: &EnvArgs,
        trace_args: &TraceArgs,
        tx: MegaTransaction,
        pre_execution_nonce: u64,
    ) -> Result<Self, EvmeError>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        let evm_context = env_args.create_evm_context(state)?;
        let start = Instant::now();
        let (exec_result, state, trace_data) = trace_args.execute_transaction(evm_context, tx)?;
        let exec_time = start.elapsed();
        Ok(Self { pre_execution_nonce, exec_result, state, exec_time, trace_data })
    }

    /// Convert the execution outcome to an OP receipt envelope.
    ///
    /// For deposit transactions (type 126), provide `deposit_nonce` and optionally
    /// `deposit_receipt_version` (introduced in Canyon hardfork).
    pub fn to_op_receipt(&self, tx_type: MegaTxType, state_nonce: u64) -> OpReceiptEnvelope {
        // Build base receipt
        let receipt = Receipt {
            status: Eip658Value::Eip658(self.exec_result.is_success()),
            cumulative_gas_used: self.exec_result.tx_gas_used(),
            logs: self.exec_result.logs().to_vec(),
        };

        // Wrap in OpReceiptEnvelope based on transaction type
        match tx_type {
            MegaTxType::Legacy => OpReceiptEnvelope::Legacy(receipt.with_bloom()),
            MegaTxType::Eip2930 => OpReceiptEnvelope::Eip2930(receipt.with_bloom()),
            MegaTxType::Eip1559 => OpReceiptEnvelope::Eip1559(receipt.with_bloom()),
            MegaTxType::Eip7702 => OpReceiptEnvelope::Eip7702(receipt.with_bloom()),
            MegaTxType::PostExec => OpReceiptEnvelope::PostExec(receipt.with_bloom()),
            MegaTxType::Deposit => {
                let deposit_receipt = OpDepositReceipt {
                    inner: receipt,
                    deposit_nonce: Some(state_nonce),
                    deposit_receipt_version: Some(1),
                };
                OpReceiptEnvelope::Deposit(deposit_receipt.with_bloom())
            }
        }
    }
}

/// The nonce `signer` holds in `db`, or zero for an account that does not exist.
///
/// Read before a transaction executes, it is the nonce its created-contract
/// address derives from ([`create_address`]).
pub fn pre_execution_nonce<DB: DatabaseRef>(db: &DB, signer: Address) -> Result<u64, DB::Error> {
    Ok(db.basic_ref(signer)?.map(|account| account.nonce).unwrap_or(0))
}

/// Announce how a transaction ended, at the level its outcome deserves.
///
/// A macro rather than a function so every line keeps the tracing target its
/// caller names: each command has always logged these under its own module
/// path, which `RUST_LOG` filters match and `-vvvv` output shows.
macro_rules! log_execution_result {
    (target: $target:literal, $result:expr) => {{
        let result: &::mega_evm::revm::context::result::ExecutionResult<
            ::mega_evm::MegaHaltReason,
        > = $result;
        match result {
            ::mega_evm::revm::context::result::ExecutionResult::Success { .. } => {
                ::tracing::info!(
                    target: $target,
                    gas_used = result.tx_gas_used(),
                    "Execution succeeded"
                )
            }
            ::mega_evm::revm::context::result::ExecutionResult::Revert { .. } => {
                ::tracing::warn!(
                    target: $target,
                    gas_used = result.tx_gas_used(),
                    "Execution reverted"
                )
            }
            ::mega_evm::revm::context::result::ExecutionResult::Halt { reason, .. } => {
                ::tracing::warn!(
                    target: $target,
                    ?reason,
                    gas_used = result.tx_gas_used(),
                    "Execution halted"
                )
            }
        }
    }};
}
pub(crate) use log_execution_result;

/// The address a transaction's receipt reports as `contractAddress`: `sender.create(nonce)` for
/// every contract creation, whether or not it deployed anything, and `None` for a call.
///
/// The field follows the transaction kind alone, never the outcome: an execution client stamps
/// it on a reverted or halted creation's receipt too, naming the address the creation targeted
/// and left empty. For a deposit, the OP deposit-receipt rules derive it from the deposit nonce,
/// which is the sender's nonce before execution
/// (<https://specs.optimism.io/protocol/deposits.html#deposit-receipt>). `pre_execution_nonce`
/// is that nonce for every transaction type: for a non-deposit it equals the transaction's own
/// nonce, which execution validated.
///
/// Every command builds its receipt through this function; the execution summary narrows the
/// result with [`deployed_contract`].
pub fn create_address(sender: Address, kind: TxKind, pre_execution_nonce: u64) -> Option<Address> {
    kind.is_create().then(|| sender.create(pre_execution_nonce))
}

/// The contract a transaction deployed: its [`create_address`], but only when execution
/// succeeded. A reverted or halted creation targets an address it left empty, which its receipt
/// still reports and the execution summary does not.
pub fn deployed_contract(
    exec_result: &ExecutionResult<MegaHaltReason>,
    create_address: Option<Address>,
) -> Option<Address> {
    create_address.filter(|_| exec_result.is_success())
}

/// Convert an [`OpReceiptEnvelope`] to an OP transaction receipt.
///
/// `first_log_index` is the block-global log index of this receipt's first log
/// (the cumulative log count of all preceding receipts in the block). Each
/// inner log is stamped with the same block/tx identity as the outer receipt so
/// the JSON is self-consistent.
#[allow(clippy::too_many_arguments)]
pub fn op_receipt_to_tx_receipt(
    receipt: &OpReceiptEnvelope,
    block_number: u64,
    block_timestamp: u64,
    from: Address,
    to: Option<Address>,
    contract_address: Option<Address>,
    effective_gas_price: u128,
    gas_used: u64,
    transaction_hash: Option<TxHash>, // only used for replay command where tx hash is known
    block_hash: Option<BlockHash>,    // only used for replay command where block hash is known
    transaction_index: u64,
    first_log_index: u64,
) -> OpTxReceipt {
    // Resolve the effective tx hash once so the outer receipt and every inner
    // log agree: a missing hash becomes `B256::ZERO` on both sides (the outer
    // field is non-optional on `TransactionReceipt`).
    let effective_tx_hash = transaction_hash.unwrap_or_default();
    let stamped_tx_hash = Some(effective_tx_hash);

    // Map logs to include block/tx metadata matching the outer receipt.
    let mut log_index = first_log_index;
    let inner = receipt.clone().map_logs(|log| {
        let log = alloy_rpc_types_eth::Log {
            inner: log,
            block_hash,
            block_number: Some(block_number),
            block_timestamp: Some(block_timestamp),
            transaction_hash: stamped_tx_hash,
            transaction_index: Some(transaction_index),
            log_index: Some(log_index),
            removed: false,
        };
        log_index += 1;
        log
    });

    TransactionReceipt {
        inner,
        transaction_hash: effective_tx_hash,
        transaction_index: Some(transaction_index),
        block_hash,
        block_number: Some(block_number),
        gas_used,
        effective_gas_price,
        blob_gas_used: None,
        blob_gas_price: None,
        from,
        to,
        contract_address,
    }
}

/// Print a human-readable execution summary.
///
/// `create_address` is the transaction's [`create_address`]; the summary names it as a contract
/// address only when the creation deployed ([`deployed_contract`]).
pub fn print_execution_summary(
    exec_result: &ExecutionResult<MegaHaltReason>,
    create_address: Option<Address>,
    exec_time: Duration,
) {
    let contract_address = deployed_contract(exec_result, create_address);
    println!();
    println!("=== Transaction Summary ===");

    match exec_result {
        ExecutionResult::Success { logs, output, .. } => {
            println!("Status:           Success");
            println!("Gas Used:         {}", exec_result.tx_gas_used());
            println!("Execution Time:   {:?}", exec_time);
            if let Some(addr) = contract_address {
                println!("Contract Address: {}", addr);
            }
            if !logs.is_empty() {
                println!("Events:           {} log(s) emitted", logs.len());
            }
            let output_data = output.data();
            if !output_data.is_empty() {
                println!("Output:           0x{}", hex::encode(output_data));
            }
        }
        ExecutionResult::Revert { output, .. } => {
            println!("Status:           Reverted");
            println!("Gas Used:         {}", exec_result.tx_gas_used());
            println!("Execution Time:   {:?}", exec_time);
            println!("Revert Reason:    {}", decode_revert_reason(output));
        }
        ExecutionResult::Halt { reason, .. } => {
            println!("Status:           Halted");
            println!("Gas Used:         {}", exec_result.tx_gas_used());
            println!("Execution Time:   {:?}", exec_time);
            println!("Halt Reason:      {}", format_halt_reason(reason));
        }
    }
}

/// Decode revert reason from output bytes using alloy's built-in decoders.
///
/// Supports:
/// - `Error(string)` via `alloy_sol_types::Revert`
/// - `Panic(uint256)` via `alloy_sol_types::Panic`
/// - Raw hex fallback
fn decode_revert_reason(output: &Bytes) -> String {
    if output.is_empty() {
        return "(empty)".to_string();
    }

    // Try to decode as Revert (Error(string))
    if let Ok(revert) = Revert::abi_decode(output) {
        return format!("Error(\"{}\")", revert.reason());
    }

    // Try to decode as Panic (Panic(uint256))
    if let Ok(panic) = Panic::abi_decode(output) {
        return if let Some(kind) = panic.kind() {
            format!("Panic: {}", kind)
        } else {
            format!("Panic(0x{:x})", panic.code)
        };
    }

    // Fallback: raw hex
    format!("0x{}", hex::encode(output))
}

/// Format halt reason for display.
fn format_halt_reason(reason: &MegaHaltReason) -> String {
    match reason {
        MegaHaltReason::Base(op_reason) => format_op_halt_reason(op_reason),
        _ => format!("{:?}", reason),
    }
}

/// Format OP halt reason for display.
fn format_op_halt_reason(reason: &OpHaltReason) -> String {
    match reason {
        OpHaltReason::Base(eth_reason) => format!("{:?}", eth_reason),
        _ => format!("{:?}", reason),
    }
}

/// Print one transaction's human-readable report: the execution summary, the
/// receipt when the command reports one, and then each note line, each after a
/// blank line.
///
/// Notes carry what a command adds about the transaction — a verification
/// verdict, a fixture outcome — and print in the order given, before any trace
/// or state dump.
pub fn print_transaction_report(
    exec_result: &ExecutionResult<MegaHaltReason>,
    create_address: Option<Address>,
    exec_time: Duration,
    receipt: Option<&OpTxReceipt>,
    notes: &[String],
) {
    print_execution_summary(exec_result, create_address, exec_time);
    if let Some(receipt) = receipt {
        print_receipt(receipt);
    }
    for note in notes {
        println!();
        println!("{note}");
    }
}

/// Print the artifacts a human-readable single-transaction run ends with: the
/// trace (or the file it was written to) and the state dump.
pub fn print_run_artifacts(
    outcome: &EvmeOutcome,
    trace_args: &TraceArgs,
    dump_args: &StateDumpArgs,
) -> Result<(), EvmeError> {
    print_execution_trace(outcome.trace_data.as_deref(), trace_args.trace_output_file.as_deref())?;
    if dump_args.dump {
        dump_args.dump_evm_state(&outcome.state)?;
    }
    Ok(())
}

/// Print a receipt as pretty-printed JSON.
pub fn print_receipt<T: serde::Serialize>(receipt: &T) {
    println!();
    println!("=== Receipt ===");
    match serde_json::to_string_pretty(receipt) {
        Ok(json) => println!("{}", json),
        Err(e) => println!("Failed to serialize receipt: {}", e),
    }
}

/// Print execution trace to console or write to file.
///
/// If `output_file` is provided, writes the trace to the file and prints the path.
/// Otherwise, prints the trace to the console.
pub fn print_execution_trace(
    trace: Option<&str>,
    output_file: Option<&Path>,
) -> Result<(), EvmeError> {
    let Some(trace) = trace else {
        return Ok(());
    };

    println!();
    println!("=== Execution Trace ===");

    if let Some(path) = output_file {
        std::fs::write(path, trace)
            .map_err(|e| EvmeError::Other(format!("Failed to write trace to file: {}", e)))?;
        println!("Trace written to: {}", path.display());
    } else {
        println!("{}", trace);
    }

    Ok(())
}

/// Output format configuration
#[derive(Parser, Debug, Clone, Default)]
#[command(next_help_heading = "Output Options")]
pub struct OutputArgs {
    /// Output results as JSON instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

/// Serializable execution summary for JSON output
#[derive(Debug, Default, Serialize)]
pub struct ExecutionSummary {
    /// Whether the execution succeeded
    pub success: bool,
    /// Gas consumed by the execution
    pub gas_used: u64,
    /// Hex-encoded return data (present only on success with non-empty output)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Deployed contract address (present only for successful CREATE transactions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<Address>,
    /// Number of log entries emitted
    pub logs_count: usize,
    /// Decoded revert reason (present only on revert)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_reason: Option<String>,
    /// Halt reason (present only on halt)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub halt_reason: Option<String>,
    /// Execution trace (present only when --trace is enabled without --trace.output)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<serde_json::Value>,
    /// Post-execution state dump (present only when --dump is enabled without --dump.output)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
    /// Transaction receipt (present only for `tx` command)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<serde_json::Value>,
    /// On-chain receipt verification verdict (present only for `replay
    /// --verify-receipt`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<serde_json::Value>,
}

impl ExecutionSummary {
    /// The summary of one transaction: its result, plus its receipt when the
    /// command reports one.
    ///
    /// `create_address` is the transaction's [`create_address`], reported as
    /// `contract_address` only when the creation deployed.
    pub fn of_transaction(
        exec_result: &ExecutionResult<MegaHaltReason>,
        create_address: Option<Address>,
        receipt: Option<&OpTxReceipt>,
    ) -> Self {
        let mut summary = Self::from_result(exec_result, create_address);
        summary.receipt = receipt
            .map(|receipt| serde_json::to_value(receipt).expect("failed to serialize receipt"));
        summary
    }

    /// Print the summary as the one pretty-printed JSON object a
    /// single-transaction run writes to stdout.
    pub fn print_pretty(&self) {
        println!("{}", serde_json::to_string_pretty(self).expect("failed to serialize output"));
    }

    /// Fill trace and state dump fields from the execution outcome.
    ///
    /// When an output file is specified, data is written to that file.
    /// Otherwise, data is inlined into the corresponding JSON field.
    pub fn fill_trace_and_dump(
        &mut self,
        outcome: &EvmeOutcome,
        trace_args: &TraceArgs,
        dump_args: &StateDumpArgs,
    ) -> Result<(), EvmeError> {
        // Trace: inline or write to file
        if let Some(trace) = outcome.trace_data.as_deref() {
            if let Some(ref path) = trace_args.trace_output_file {
                std::fs::write(path, trace).map_err(|e| {
                    EvmeError::Other(format!("Failed to write trace to file: {}", e))
                })?;
            } else {
                self.trace = Some(serde_json::from_str(trace).unwrap_or_else(|_| trace.into()));
            }
        }

        // Dump: inline or write to file
        if dump_args.dump {
            let state_json = dump_args.serialize_evm_state(&outcome.state)?;
            if let Some(ref path) = dump_args.dump_output_file {
                std::fs::write(path, &state_json).map_err(|e| {
                    EvmeError::Other(format!("Failed to write state dump to file: {}", e))
                })?;
            } else {
                self.state =
                    Some(serde_json::from_str(&state_json).unwrap_or_else(|_| state_json.into()));
            }
        }

        Ok(())
    }

    /// Create from an `ExecutionResult` and the transaction's [`create_address`], which the
    /// summary reports as `contract_address` only when the creation deployed
    /// ([`deployed_contract`]).
    pub fn from_result(
        exec_result: &ExecutionResult<MegaHaltReason>,
        create_address: Option<Address>,
    ) -> Self {
        let contract_address = deployed_contract(exec_result, create_address);
        match exec_result {
            ExecutionResult::Success { logs, output, .. } => {
                let output_data = output.data();
                Self {
                    success: true,
                    gas_used: exec_result.tx_gas_used(),
                    output: if output_data.is_empty() {
                        None
                    } else {
                        Some(format!("0x{}", hex::encode(output_data)))
                    },
                    contract_address,
                    logs_count: logs.len(),
                    ..Default::default()
                }
            }
            ExecutionResult::Revert { output, .. } => Self {
                gas_used: exec_result.tx_gas_used(),
                revert_reason: Some(decode_revert_reason(output)),
                ..Default::default()
            },
            ExecutionResult::Halt { reason, .. } => Self {
                gas_used: exec_result.tx_gas_used(),
                halt_reason: Some(format!("{:?}", reason)),
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, Bytes, Log as PrimitiveLog, LogData, B256};
    use alloy_sol_types::SolError;
    use mega_evm::revm::context::result::{Output, SuccessReason};

    const CREATOR: Address = address!("0x00000000000000000000000000000000000000c1");

    fn success() -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: Default::default(),
            logs: Vec::new(),
            output: Output::Create(Bytes::new(), None),
        }
    }

    fn revert() -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Revert { gas: Default::default(), logs: Vec::new(), output: Bytes::new() }
    }

    fn halt() -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Halt {
            reason: MegaHaltReason::DataLimitExceeded { limit: 1, actual: 2 },
            gas: Default::default(),
            logs: Vec::new(),
        }
    }

    /// A creation reports `sender.create(nonce)` and a call reports nothing; the outcome plays no
    /// part.
    #[test]
    fn test_create_address_follows_the_transaction_kind_only() {
        assert_eq!(create_address(CREATOR, TxKind::Create, 7), Some(CREATOR.create(7)));
        assert_eq!(create_address(CREATOR, TxKind::Create, 0), Some(CREATOR.create(0)));
        assert_eq!(create_address(CREATOR, TxKind::Call(CREATOR), 7), None);
    }

    /// Only a successful creation deployed the contract its receipt names.
    #[test]
    fn test_deployed_contract_requires_success() {
        let created = create_address(CREATOR, TxKind::Create, 0);
        assert_eq!(deployed_contract(&success(), created), created);
        assert_eq!(deployed_contract(&revert(), created), None);
        assert_eq!(deployed_contract(&halt(), created), None);
        assert_eq!(deployed_contract(&success(), None), None);
    }

    /// The summary takes the receipt's address and names it only for a deployment, so a failed
    /// creation's summary carries no contract.
    #[test]
    fn test_execution_summary_names_only_a_deployed_contract() {
        let created = create_address(CREATOR, TxKind::Create, 3);
        assert_eq!(ExecutionSummary::from_result(&success(), created).contract_address, created);
        assert_eq!(ExecutionSummary::from_result(&revert(), created).contract_address, None);
        assert_eq!(ExecutionSummary::from_result(&halt(), created).contract_address, None);
    }

    #[test]
    fn test_decode_revert_reason_empty() {
        assert_eq!(decode_revert_reason(&Bytes::new()), "(empty)");
    }

    #[test]
    fn test_decode_revert_reason_error_string() {
        let encoded = Revert::from("insufficient balance").abi_encode();
        assert_eq!(decode_revert_reason(&encoded.into()), "Error(\"insufficient balance\")");
    }

    #[test]
    fn test_decode_revert_reason_panic() {
        // Panic(0x01) = assert failure
        let encoded = Panic { code: alloy_primitives::U256::from(0x01) }.abi_encode();
        assert_eq!(decode_revert_reason(&encoded.into()), "Panic: assertion failed");
    }

    #[test]
    fn test_decode_revert_reason_raw_hex() {
        let raw = Bytes::from(vec![0xde, 0xad]);
        assert_eq!(decode_revert_reason(&raw), "0xdead");
    }

    /// Inner logs carry the same block/tx identity as the outer receipt, and
    /// `log_index` is the block-global index starting at `first_log_index`.
    #[test]
    fn test_op_receipt_to_tx_receipt_stamps_inner_log_metadata() {
        let addr = address!("0x00000000000000000000000000000000000000aa");
        let topic = b256!("0x000000000000000000000000000000000000000000000000000000000000000a");
        let log = PrimitiveLog {
            address: addr,
            data: LogData::new(vec![topic], Bytes::from(vec![0xde, 0xad])).expect("topics"),
        };
        let receipt = OpReceiptEnvelope::Legacy(
            alloy_consensus::Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![log.clone(), log],
            }
            .with_bloom(),
        );
        let tx_hash = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        let block_hash =
            b256!("0x2222222222222222222222222222222222222222222222222222222222222222");
        let from = address!("0x00000000000000000000000000000000000000bb");

        let tx_receipt = op_receipt_to_tx_receipt(
            &receipt,
            42,
            1_700_000_000,
            from,
            Some(addr),
            None,
            1,
            21_000,
            Some(tx_hash),
            Some(block_hash),
            3,
            7, // two preceding receipts already emitted 7 logs in this block
        );

        assert_eq!(tx_receipt.transaction_hash, tx_hash);
        assert_eq!(tx_receipt.block_hash, Some(block_hash));
        assert_eq!(tx_receipt.transaction_index, Some(3));
        let logs = tx_receipt.inner.logs();
        assert_eq!(logs.len(), 2);
        for (i, log) in logs.iter().enumerate() {
            assert_eq!(log.block_hash, Some(block_hash), "log {i} block_hash");
            assert_eq!(log.transaction_hash, Some(tx_hash), "log {i} transaction_hash");
            assert_eq!(log.transaction_index, Some(3), "log {i} transaction_index");
            assert_eq!(log.log_index, Some(7 + i as u64), "log {i} block-global log_index");
            assert_eq!(log.block_number, Some(42));
        }
    }

    /// A missing transaction hash becomes `B256::ZERO` on the outer receipt and
    /// the same value on every inner log (not `None` on logs / zero only outside).
    #[test]
    fn test_op_receipt_to_tx_receipt_missing_tx_hash_stamps_outer_and_inner_consistently() {
        let addr = address!("0x00000000000000000000000000000000000000aa");
        let topic = b256!("0x000000000000000000000000000000000000000000000000000000000000000a");
        let log = PrimitiveLog {
            address: addr,
            data: LogData::new(vec![topic], Bytes::from(vec![0xbe, 0xef])).expect("topics"),
        };
        let receipt = OpReceiptEnvelope::Legacy(
            alloy_consensus::Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![log],
            }
            .with_bloom(),
        );
        let from = address!("0x00000000000000000000000000000000000000bb");

        let tx_receipt = op_receipt_to_tx_receipt(
            &receipt,
            1,
            1,
            from,
            Some(addr),
            None,
            1,
            21_000,
            None,
            None,
            0,
            0,
        );

        assert_eq!(tx_receipt.transaction_hash, B256::ZERO);
        let logs = tx_receipt.inner.logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].transaction_hash,
            Some(B256::ZERO),
            "inner log must use the same effective hash as the outer receipt"
        );
    }
}
