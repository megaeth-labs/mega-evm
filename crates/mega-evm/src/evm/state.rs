//! The block hashes execution read, which a node's stateless witness needs.
//!
//! `BLOCKHASH` reads the chain's history through the database, not through the journal, so a
//! witness built from a transaction's state alone would miss them. The Host records every hash it
//! serves in the [`BlockHashRecord`] the context keeps, and block execution empties the record
//! when the block starts, so what the record holds is what this block read.
//!
//! The record is kept beside revm's `State`, not read out of it: `State` caches every hash it ever
//! served, across every block it was used for, so a database reused over a range of blocks — or
//! one whose cache was filled in advance — would report hashes this block never asked for.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::collections::BTreeMap;

use alloy_primitives::B256;

/// The block hashes execution has read, by block number.
///
/// The record accumulates over the transactions of one block; block execution empties it at the
/// start of the block, and a caller that attributes reads to one transaction empties it between
/// transactions ([`MegaEvm::clear_accessed_block_hashes`](crate::MegaEvm)). It records what was
/// read and decides nothing, so emptying it changes no execution result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockHashRecord(BTreeMap<u64, B256>);

impl BlockHashRecord {
    /// Records that `number`'s hash was read, and what it was.
    pub fn record(&mut self, number: u64, hash: B256) {
        self.0.insert(number, hash);
    }

    /// The hashes read so far, by block number.
    pub const fn hashes(&self) -> &BTreeMap<u64, B256> {
        &self.0
    }

    /// Whether nothing has been read.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Forgets the hashes read so far.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_record_holds_what_was_read() {
        let mut record = BlockHashRecord::default();
        assert!(record.is_empty());

        record.record(1, B256::ZERO);
        record.record(2, B256::from([2_u8; 32]));

        assert_eq!(record.hashes().len(), 2);
        assert_eq!(record.hashes().get(&1), Some(&B256::ZERO));
        assert_eq!(record.hashes().get(&2), Some(&B256::from([2_u8; 32])));
    }

    /// Reading the same block twice records it once, with the hash it was served with.
    #[test]
    fn test_the_record_keeps_one_entry_per_block() {
        let mut record = BlockHashRecord::default();
        record.record(7, B256::from([7_u8; 32]));
        record.record(7, B256::from([7_u8; 32]));

        assert_eq!(record.hashes().len(), 1);
    }

    /// Clearing empties the record.
    #[test]
    fn test_clearing_forgets_the_hashes_read_so_far() {
        let mut record = BlockHashRecord::default();
        record.record(7, B256::from([7_u8; 32]));

        record.clear();

        assert!(record.is_empty());
        assert!(record.hashes().is_empty());
    }
}
