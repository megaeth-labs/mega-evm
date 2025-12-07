//! Execution outcome for mega-evme commands

use std::time::Duration;

use alloy_consensus::{Eip658Value, Receipt};
use alloy_primitives::{Address, BlockHash, TxHash, B256};
use alloy_rpc_types_eth::TransactionReceipt;
use mega_evm::{
    revm::{context::result::ExecutionResult, state::EvmState},
    MegaTxType,
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
