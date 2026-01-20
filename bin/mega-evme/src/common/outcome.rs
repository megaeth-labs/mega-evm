//! Execution outcome for mega-evme commands

use std::{path::Path, time::Duration};

use super::EvmeError;

use alloy_consensus::{Eip658Value, Receipt};
use alloy_primitives::{hex, Address, BlockHash, Bytes, TxHash, B256};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_sol_types::{Panic, Revert, SolError};
use mega_evm::{
    op_revm::OpHaltReason,
    revm::{context::result::ExecutionResult, state::EvmState},
    MegaHaltReason, MegaTxType,
};
use op_alloy_consensus::{OpDepositReceipt, OpReceiptEnvelope};

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
    /// Convert the execution outcome to an OP receipt envelope.
    ///
    /// For deposit transactions (type 126), provide `deposit_nonce` and optionally
    /// `deposit_receipt_version` (introduced in Canyon hardfork).
    pub fn to_op_receipt(&self, tx_type: MegaTxType, state_nonce: u64) -> OpReceiptEnvelope {
        // Build base receipt
        let receipt = Receipt {
            status: Eip658Value::Eip658(self.exec_result.is_success()),
            cumulative_gas_used: self.exec_result.gas_used(),
            logs: self.exec_result.logs().to_vec(),
        };

        // Wrap in OpReceiptEnvelope based on transaction type
        match tx_type {
            MegaTxType::Legacy => OpReceiptEnvelope::Legacy(receipt.with_bloom()),
            MegaTxType::Eip2930 => OpReceiptEnvelope::Eip2930(receipt.with_bloom()),
            MegaTxType::Eip1559 => OpReceiptEnvelope::Eip1559(receipt.with_bloom()),
            MegaTxType::Eip7702 => OpReceiptEnvelope::Eip7702(receipt.with_bloom()),
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

    match exec_result {
        ExecutionResult::Success { gas_used, logs, output, .. } => {
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
        ExecutionResult::Revert { gas_used, output } => {
            println!("Status:           Reverted");
            println!("Gas Used:         {}", gas_used);
            println!("Execution Time:   {:?}", exec_time);
            println!("Revert Reason:    {}", decode_revert_reason(output));
        }
        ExecutionResult::Halt { gas_used, reason } => {
            println!("Status:           Halted");
            println!("Gas Used:         {}", gas_used);
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
