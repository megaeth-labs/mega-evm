//! The block hashes execution read, which a node's stateless witness needs.
//!
//! `BLOCKHASH` reads the chain's history through the database, not through the journal, so a
//! witness built from a transaction's state alone would miss them. revm's [`State`] already
//! caches every hash it served in `block_hashes`; [`BlockHashes`] is the read of that cache the
//! block executor and the node share.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::collections::BTreeMap;

use alloy_primitives::B256;
use auto_impl::auto_impl;
use revm::{
    database::{states::block_hash_cache::BlockHashCache, State},
    Database,
};

/// The block hashes execution has read so far.
///
/// The record accumulates across every transaction executed on the same database, so a caller
/// that attributes reads to one transaction clears it first
/// ([`clear_accessed_block_hashes`](Self::clear_accessed_block_hashes)). It is a cache: a
/// cleared hash is fetched again on the next read, so clearing it changes no execution result.
#[auto_impl(&mut, Box)]
pub trait BlockHashes {
    /// The block hashes read so far, by block number.
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256>;

    /// Forgets the hashes read so far.
    fn clear_accessed_block_hashes(&mut self);
}

impl<DB: Database> BlockHashes for State<DB> {
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.block_hashes.iter().collect()
    }

    fn clear_accessed_block_hashes(&mut self) {
        self.block_hashes = BlockHashCache::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::database::InMemoryDB;

    #[test]
    fn test_state_exposes_accessed_block_hashes() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(1, B256::ZERO);
        state.block_hashes.insert(2, B256::from([2_u8; 32]));

        let hashes = state.get_accessed_block_hashes();
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes.get(&1), Some(&B256::ZERO));
        assert_eq!(hashes.get(&2), Some(&B256::from([2_u8; 32])));
    }

    /// Clearing empties the record and leaves the database able to serve the hash again.
    #[test]
    fn test_clearing_forgets_the_hashes_read_so_far() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(7, B256::from([7_u8; 32]));

        state.clear_accessed_block_hashes();

        assert!(state.get_accessed_block_hashes().is_empty());
    }
}
