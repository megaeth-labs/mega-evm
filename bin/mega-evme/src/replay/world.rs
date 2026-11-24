//! The world a replayed block executes in, built from its authenticated header.
//!
//! Both replay drivers turn a fetched block into the same pieces: the block
//! environment its transactions read, and the block-level execution context
//! (parent hash, beacon root, extra data, block limits) the executor is built
//! with. Each driver orders these steps around its own fetches and reports a
//! failure in its own shape — the single-transaction driver fails the run, the
//! batch driver fails the block's remaining targets — so this module hands out
//! the pieces rather than one assembled world.

use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::Block;
use mega_evm::{
    revm::{context::BlockEnv, primitives::eip4844},
    MegaBlockExecutionCtx,
};
use op_alloy_rpc_types::Transaction;
use tracing::trace;

use super::{ReplayError, ReplayHardforks, Result};

/// Build a [`BlockEnv`] from the RPC block header.
///
/// Reads `excess_blob_gas` directly from the header rather than using a
/// hardcoded default, so blob-fee-sensitive opcodes (e.g. `BLOBBASEFEE`)
/// match on-chain semantics during replay.
pub(super) fn retrieve_block_env(block: &Block<Transaction>) -> Result<BlockEnv> {
    let mut block_env = BlockEnv {
        number: U256::from(block.number()),
        beneficiary: block.header.beneficiary(),
        timestamp: U256::from(block.header.timestamp()),
        gas_limit: block.header.gas_limit(),
        basefee: block.header.base_fee_per_gas().unwrap_or_default(),
        difficulty: block.header.difficulty(),
        prevrandao: block.header.mix_hash(),
        blob_excess_gas_and_price: None,
        slot_num: 0,
    };

    let excess_blob_gas = block.header.excess_blob_gas().ok_or_else(|| {
        ReplayError::Other(format!(
            "block header missing excess_blob_gas (block {})",
            block.number()
        ))
    })?;
    block_env.set_blob_excess_gas_and_price(
        excess_blob_gas,
        eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
    );

    // Logged under the replay command's target, where this line has always
    // come from, so existing `RUST_LOG` filters keep matching it.
    trace!(target: "mega_evme::replay::cmd", block_env = ?block_env, "Block environment retrieved");
    Ok(block_env)
}

/// Build the block-level execution context `block` executes under, forked from
/// the parent block `parent_hash`.
///
/// The block limits come from `hardforks` at the block's timestamp, so a spec
/// override moves them together with the EVM semantics. A schedule with no fork
/// active at that timestamp has no limits to execute under; that failure is
/// returned as the schedule's own message, which each driver reports in its own
/// shape.
pub(super) fn block_ctx(
    hardforks: &ReplayHardforks<'_>,
    block: &Block<Transaction>,
    parent_hash: B256,
) -> std::result::Result<MegaBlockExecutionCtx, String> {
    let block_limits =
        hardforks.block_limits(block.header.timestamp(), block.header.gas_limit())?;
    Ok(MegaBlockExecutionCtx::new(
        parent_hash,
        block.header.parent_beacon_block_root(),
        block.header.extra_data().clone(),
        block_limits,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_rpc_types_eth::Header as RpcHeader;
    use mega_evm::revm::context_interface::block::BlobExcessGasAndPrice;

    fn make_block(excess_blob_gas: Option<u64>) -> Block<Transaction> {
        let inner = ConsensusHeader { excess_blob_gas, ..Default::default() };
        Block::empty(RpcHeader::new(inner))
    }

    #[test]
    fn test_retrieve_block_env_sets_blob_fee_from_header() {
        let excess_blob_gas: u64 = 786_432;
        let block = make_block(Some(excess_blob_gas));

        let env = retrieve_block_env(&block).expect("should build block env");

        let expected = BlobExcessGasAndPrice::new(
            excess_blob_gas,
            eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
        );
        assert_eq!(env.blob_excess_gas_and_price, Some(expected));
    }

    #[test]
    fn test_retrieve_block_env_zero_excess_blob_gas_yields_min_price() {
        let block = make_block(Some(0));

        let env = retrieve_block_env(&block).expect("should build block env");

        let blob = env.blob_excess_gas_and_price.expect("blob fields populated");
        assert_eq!(blob.excess_blob_gas, 0);
        assert_eq!(blob.blob_gasprice, u128::from(eip4844::MIN_BLOB_GASPRICE));
    }

    #[test]
    fn test_retrieve_block_env_missing_excess_blob_gas_errors() {
        let block = make_block(None);

        let err = retrieve_block_env(&block).expect_err("should reject pre-Cancun header");
        match err {
            ReplayError::Other(msg) => assert!(
                msg.contains("excess_blob_gas"),
                "error should mention missing field, got: {msg}"
            ),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
