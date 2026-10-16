//! Execution outcome and output formatting for mega-evme commands

use std::{path::Path, time::Duration};

use super::{EvmeError, StateDumpArgs, TraceArgs};

use alloy_consensus::{Eip658Value, Receipt};
use alloy_primitives::{hex, Address, BlockHash, Bytes, TxHash, B256};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_sol_types::{Panic, Revert, SolError};
use clap::Parser;
use mega_evm::{
    op_revm::OpHaltReason,
    revm::{context::result::ExecutionResult, state::EvmState},
    LimitCheck, LimitKind, MegaHaltReason, MegaTransactionOutcome, MegaTxType, ProtocolLimits,
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
    /// What Satin counted beside the result
    pub satin: SatinReport,
}

/// What the Satin engine counts for a transaction beside revm's result: the gas by ledger, the
/// data size and write records it kept, and the limit that stopped it.
///
/// It is the one part of the output a legacy run has no counterpart for, so it is carried in a
/// field of its own and every other field keeps its legacy name and meaning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SatinReport {
    /// Regular (compute) gas spent, before the refund.
    pub regular_gas: u64,
    /// EIP-8037 state gas spent, net of refills.
    pub state_gas: u64,
    /// History gas spent, net of refills.
    pub history_gas: u64,
    /// History bytes the transaction appended, whoever paid for them.
    pub history_bytes: u64,
    /// The EIP-8037 reservoir left unspent, which the sender gets back.
    pub reservoir_remaining: u64,
    /// The EIP-7623 floor the receipt's gas used cannot fall below.
    pub floor_gas: u64,
    /// Data-size bytes the transaction kept.
    pub data_size: u64,
    /// Account and storage write records the transaction kept: its KV count.
    pub write_records: u64,
    /// The transaction-level limit that stopped the transaction, if one did.
    pub limit_exceeded: Option<LimitStop>,
    /// The protocol limits the run was held to, in the shape `--override.limits` takes them;
    /// present only when that flag replaced the schedule's, so the record of a counterfactual
    /// says what it ran under. Boxed: only such a run carries them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits_override: Option<Box<ProtocolLimits>>,
}

/// A transaction-level limit that stopped a Satin transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LimitStop {
    /// The dimension: `data_size`, `kv_update`, `compute_gas` or `state_growth`.
    pub kind: &'static str,
    /// The limit crossed, in the dimension's unit.
    pub limit: u64,
    /// The usage that crossed it, where it crossed it: for `state_growth`, the state gas held
    /// at that point, so a deposit whose created caller alone crosses the limit reports that
    /// account, even where its first frame would add another.
    pub used: u64,
}

impl SatinReport {
    /// The report of `outcome`.
    pub fn of(outcome: &MegaTransactionOutcome) -> Self {
        let gas = outcome.gas;
        Self {
            regular_gas: gas.regular,
            state_gas: gas.state,
            history_gas: gas.history,
            history_bytes: gas.history_bytes,
            reservoir_remaining: gas.reservoir_remaining,
            floor_gas: gas.floor,
            data_size: outcome.usage.data_size,
            write_records: outcome.usage.write_records,
            limit_exceeded: match outcome.limit_exceeded {
                Some(LimitCheck::ExceedsLimit { kind, limit, used, .. }) => {
                    Some(LimitStop { kind: limit_kind_name(kind), limit, used })
                }
                _ => None,
            },
            limits_override: None,
        }
    }
}

/// The name of a limit dimension in output.
pub const fn limit_kind_name(kind: LimitKind) -> &'static str {
    match kind {
        LimitKind::DataSize => "data_size",
        LimitKind::KVUpdate => "kv_update",
        LimitKind::ComputeGas => "compute_gas",
        LimitKind::StateGrowth => "state_growth",
    }
}

impl EvmeOutcome {
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

/// Convert an [`OpReceiptEnvelope`] to an OP transaction receipt.
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
) -> OpTxReceipt {
    // Map logs to include block/tx metadata
    let mut log_index = 0;
    let inner = receipt.clone().map_logs(|log| {
        let log = alloy_rpc_types_eth::Log {
            inner: log,
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp: Some(block_timestamp),
            transaction_hash: Some(B256::ZERO),
            transaction_index: Some(0),
            log_index: Some(log_index),
            removed: false,
        };
        log_index += 1;
        log
    });

    TransactionReceipt {
        inner,
        transaction_hash: transaction_hash.unwrap_or_default(),
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
pub fn print_execution_summary(
    exec_result: &ExecutionResult<MegaHaltReason>,
    contract_address: Option<Address>,
    exec_time: Duration,
) {
    println!();
    println!("=== Transaction Summary ===");

    let gas_used = exec_result.tx_gas_used();
    match exec_result {
        ExecutionResult::Success { logs, output, .. } => {
            println!("Status:           Success");
            println!("Gas Used:         {}", gas_used);
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
            println!("Gas Used:         {}", gas_used);
            println!("Execution Time:   {:?}", exec_time);
            println!("Revert Reason:    {}", decode_revert_reason(output));
        }
        ExecutionResult::Halt { reason, .. } => {
            println!("Status:           Halted");
            println!("Gas Used:         {}", gas_used);
            println!("Execution Time:   {:?}", exec_time);
            println!("Halt Reason:      {}", format_halt_reason(reason));
        }
    }
}

/// Print what Satin counted, after the summary it adds to.
pub fn print_satin_report(report: &SatinReport) {
    println!();
    println!("=== Satin Gas ===");
    println!("Regular Gas:      {}", report.regular_gas);
    println!("State Gas:        {}", report.state_gas);
    println!("History Gas:      {}", report.history_gas);
    println!("History Bytes:    {}", report.history_bytes);
    println!("Reservoir Left:   {}", report.reservoir_remaining);
    println!("Floor Gas:        {}", report.floor_gas);
    println!("Data Size:        {}", report.data_size);
    println!("Write Records:    {}", report.write_records);
    if let Some(stop) = report.limit_exceeded {
        println!("Limit Exceeded:   {} (limit {}, used {})", stop.kind, stop.limit, stop.used);
    }
    if let Some(limits) = &report.limits_override {
        println!("Limits Override:  {}", serde_json::to_string(limits).expect("limits serialize"));
    }
}

/// Decode revert reason from output bytes using alloy's built-in decoders.
///
/// Supports:
/// - `Error(string)` via `alloy_sol_types::Revert`
/// - `Panic(uint256)` via `alloy_sol_types::Panic`
/// - Raw hex fallback
pub fn decode_revert_reason(output: &Bytes) -> String {
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
pub fn format_halt_reason(reason: &MegaHaltReason) -> String {
    match reason {
        OpHaltReason::Base(eth_reason) => format!("{:?}", eth_reason),
        _ => format!("{:?}", reason),
    }
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
    /// What Satin counted beside the result; absent on a legacy run, which has no counterpart
    #[serde(skip_serializing_if = "Option::is_none")]
    pub satin: Option<SatinReport>,
}

impl ExecutionSummary {
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

    /// Create from an `ExecutionResult` and optional contract address.
    pub fn from_result(
        exec_result: &ExecutionResult<MegaHaltReason>,
        contract_address: Option<Address>,
    ) -> Self {
        let gas_used = exec_result.tx_gas_used();
        match exec_result {
            ExecutionResult::Success { logs, output, .. } => {
                let output_data = output.data();
                Self {
                    success: true,
                    gas_used,
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
                gas_used,
                revert_reason: Some(decode_revert_reason(output)),
                ..Default::default()
            },
            ExecutionResult::Halt { reason, .. } => {
                Self { gas_used, halt_reason: Some(format!("{:?}", reason)), ..Default::default() }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use alloy_sol_types::SolError;

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
}
