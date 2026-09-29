//! What execution read beside the journal, which a node's stateless witness needs: the block
//! hashes and the SALT buckets.
//!
//! `BLOCKHASH` reads the chain's history through the database, not through the journal, so a
//! witness built from a transaction's state alone would miss them. The Host records every hash it
//! serves in the [`BlockHashRecord`] the context keeps, and block execution empties the record
//! when the block starts, so what the record holds is what this block read.
//!
//! The record is kept beside revm's `State`, not read out of it: `State` caches every hash it ever
//! served, across every block it was used for, so a database reused over a range of blocks — or
//! one whose cache was filled in advance — would report hashes this block never asked for.
//!
//! A SALT bucket's capacity is read through the transaction's [`SaltEnv`](crate::SaltEnv), a side
//! channel no database sees, and a validator that lacks a bucket's proof cannot price the charge
//! that landed in it. The context records every bucket the SALT environment is asked about in the
//! [`BucketRecord`], which is emptied when a block starts and not between its transactions: the
//! per-transaction cache of multipliers ([`BucketMultipliers`](crate::BucketMultipliers)) is
//! forgotten before every transaction, and a record that lived there would lose every
//! transaction's buckets but the last one's.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use alloy_primitives::B256;

use crate::BucketId;

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

/// The SALT buckets execution has asked the SALT environment about.
///
/// One entry per bucket, recorded when the environment is asked for its capacity, whatever it
/// answers: a lookup that fails fails its transaction with its cause, so the transaction is in no
/// block and a validator never makes the lookup, and recording it costs a validator nothing but a
/// proof it does not use. What a block's execution asked about is what a validator re-executing
/// the block asks about, because the engine reads a bucket at a charge site and nothing else
/// decides whether a charge site is reached.
///
/// The record accumulates over the transactions of one block, the pre-block calls and the system
/// transactions included, which read none: block execution empties it at the start of the block,
/// and a caller that attributes the reads to one transaction empties it between transactions
/// ([`MegaEvm::clear_accessed_bucket_ids`](crate::MegaEvm)). It records what was read and decides
/// nothing, so emptying it changes no execution result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BucketRecord(BTreeSet<BucketId>);

impl BucketRecord {
    /// Records that `bucket` was asked about.
    pub fn record(&mut self, bucket: BucketId) {
        self.0.insert(bucket);
    }

    /// The buckets asked about so far, in ascending order.
    pub const fn ids(&self) -> &BTreeSet<BucketId> {
        &self.0
    }

    /// The buckets asked about so far, as a vector in ascending order.
    pub fn to_vec(&self) -> Vec<BucketId> {
        self.0.iter().copied().collect()
    }

    /// Whether nothing has been asked about.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Forgets the buckets asked about so far.
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

        assert!(!record.is_empty(), "a record that holds a read is not empty");
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

    /// The bucket record holds every bucket asked about, once each and in ascending order.
    #[test]
    fn test_the_bucket_record_holds_each_bucket_once_in_order() {
        let mut record = BucketRecord::default();
        assert!(record.is_empty());

        record.record(9);
        record.record(2);
        record.record(9);

        assert!(!record.is_empty(), "a record that holds an ask is not empty");
        assert_eq!(record.to_vec(), vec![2, 9]);
        assert_eq!(record.ids().len(), 2);
    }

    /// Clearing empties the bucket record.
    #[test]
    fn test_clearing_forgets_the_buckets_asked_about() {
        let mut record = BucketRecord::default();
        record.record(7);

        record.clear();

        assert!(record.is_empty());
        assert!(record.to_vec().is_empty());
    }
}
