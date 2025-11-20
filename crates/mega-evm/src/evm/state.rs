#[cfg(not(feature = "std"))]
use alloc::collections::BTreeMap;
#[cfg(feature = "std")]
use std::collections::BTreeMap;

use alloy_primitives::B256;
use auto_impl::auto_impl;
use revm::{database::State, Database};

/// A helper trait to get the block hashes used during transaction execution.
#[auto_impl(&, &mut, Box, Rc, Arc)]
pub trait BlockHashes {
    /// Get the block hashes used during transaction execution.
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256>;
}

impl<DB: Database> BlockHashes for State<DB> {
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.block_hashes.clone()
    }
}
