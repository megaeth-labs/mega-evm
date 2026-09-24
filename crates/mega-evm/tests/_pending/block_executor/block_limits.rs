//! Tests for block-level limit enforcement in `MegaBlockExecutor`.
//!
//! These tests verify that the block executor properly enforces block-level data
//! and KV-update limits across multiple transactions within a block.

use std::convert::Infallible;

use alloy_consensus::{Signed, Transaction, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_evm::{block::BlockExecutor, Evm, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, EnrichedMegaTx, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, MegaTransactionExt, MegaTxEnvelope, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{ADD, DUP1, LOG0, PUSH0, SLOAD, SSTORE},
    context::BlockEnv,
    database::{Database, State},
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to create a recovered transaction.
fn create_transaction(
    nonce: u64,
    gas_limit: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453), // Base mainnet
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Creates a contract that generates a log with specified data size.
///
/// The contract will emit LOG0 with the specified number of zero bytes.
fn create_log_generating_contract(data_size: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Push data size
    if data_size <= 0xFF {
        builder = builder.push_number(data_size as u8);
    } else if data_size <= 0xFFFF {
        builder = builder.push_number(data_size as u16);
    } else {
        builder = builder.push_number(data_size as u32);
    }

    // Push memory offset (0)
    builder = builder.append(PUSH0);

    // LOG0(offset, size)
    builder = builder.append(LOG0);

    // Stop
    builder.stop().build()
}

/// Creates a contract that performs N SSTORE operations.
///
/// Each SSTORE loads from storage slot i, increments it, and stores it back.
fn create_sstore_contract(num_writes: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    for i in 1..=num_writes {
        // Push key (slot number) for SLOAD
        if i <= 0xFF {
            builder = builder.push_number(i as u8);
        } else if i <= 0xFFFF {
            builder = builder.push_number(i as u16);
        } else {
            builder = builder.push_number(i as u32);
        }

        // Duplicate key on stack for SSTORE later
        // DUP1 duplicates the top stack item
        builder = builder.append(DUP1);

        // SLOAD - load current value from slot
        // Stack: [key, value]
        builder = builder.append(SLOAD);

        // Push 1 to increment
        // Stack: [key, value, 1]
        builder = builder.push_number(1u8);

        // ADD - increment the loaded value
        // Stack: [key, incremented_value]
        builder = builder.append(ADD);

        // SSTORE - store incremented value back to slot
        // Stack: []
        builder = builder.append(SSTORE);
    }

    builder.stop().build()
}

const CALLER2: alloy_primitives::Address = address!("3000000000000000000000000000000000000003");
const CALLER3: alloy_primitives::Address = address!("4000000000000000000000000000000000000004");
