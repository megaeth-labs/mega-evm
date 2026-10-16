//! The SALT bucket multipliers one transaction prices with.
//!
//! `MegaETH` keeps accounts and storage slots in SALT buckets. A bucket holds at least
//! [`MIN_BUCKET_SIZE`] entries and grows as the region fills up; the ratio between what it holds
//! and that minimum is the *multiplier* every EIP-8037 state gas charge on an account or slot of
//! that bucket is scaled by. A charge in a minimum-capacity bucket pays the schedule price, and
//! one in a bucket eight times as large pays eight times as much.
//!
//! Capacities are read through the transaction's [`SaltEnv`], which is a database read. The hook
//! that prices state gas runs on every state charge and every refill of one, so the reads are
//! cached here: [`BucketMultipliers`] queries a bucket once per transaction and answers every
//! later charge on that bucket from the cache.
//!
//! [`MIN_BUCKET_SIZE`] is the smallest capacity a backend may report. A capacity below it is a
//! broken backend rather than a cheap bucket, and is treated as a failed lookup
//! ([`BucketError::BelowMinimum`]) rather than rounded up to the minimum: a report the engine
//! cannot make sense of must not become a price nobody chose.

use core::fmt;

use alloy_primitives::Address;
use revm::primitives::{HashMap, StorageKey};

use crate::{BucketId, SaltEnv, MIN_BUCKET_SIZE};

/// Why a SALT bucket's multiplier could not be established.
///
/// Both variants fail the transaction the same way: the pricing hook reports nothing and records
/// the cause, and the charge site bails out. They are kept apart because they say different
/// things about the backend — one could not answer, the other answered something a bucket cannot
/// hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketError<E> {
    /// The SALT environment could not report the bucket's capacity.
    Env(E),
    /// The SALT environment reported a capacity below [`MIN_BUCKET_SIZE`], which is the smallest
    /// a bucket can hold.
    BelowMinimum {
        /// The bucket that was asked for.
        bucket: BucketId,
        /// The capacity the environment reported for it.
        capacity: u64,
    },
}

impl<E: fmt::Display> fmt::Display for BucketError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(error) => write!(f, "{error}"),
            Self::BelowMinimum { bucket, capacity } => write!(
                f,
                "bucket {bucket} reports capacity {capacity}, below the minimum bucket of \
                 {MIN_BUCKET_SIZE}",
            ),
        }
    }
}

/// The SALT bucket multipliers the running transaction has looked up.
///
/// One entry per bucket the transaction has priced a charge in. It is cleared with the rest of
/// the per-transaction state before every transaction and system call a
/// [`MegaEvm`](crate::MegaEvm) runs, so a transaction never prices against a capacity an earlier
/// transaction read.
///
/// The environment is passed in rather than held, so the cache adds no second copy of it and
/// puts no `Clone` bound on the engine's external environment types.
#[derive(Clone, Debug, Default)]
pub struct BucketMultipliers {
    /// Bucket id to its multiplier, for the buckets this transaction has read.
    cached: HashMap<BucketId, u64>,
}

impl BucketMultipliers {
    /// Forgets what the last transaction read, keeping the allocation.
    #[inline]
    pub(crate) fn reset(&mut self) {
        self.cached.clear();
    }

    /// The multiplier a bucket of `capacity` entries prices with: `capacity / MIN_BUCKET_SIZE`,
    /// or nothing when that capacity is below the minimum a bucket can hold.
    ///
    /// A bucket cannot hold fewer entries than the minimum, so a report that one does is a broken
    /// backend. Rounding it up to the minimum would let a misreport pass as the cheapest price
    /// there is; reporting nothing fails the transaction instead, the way any other unanswerable
    /// lookup does. The one answer that must not be reachable from outside the engine —
    /// `m = 0`, which prices every state charge in the bucket at zero — is neither.
    #[inline]
    pub const fn for_capacity(capacity: u64) -> Option<u64> {
        match capacity / MIN_BUCKET_SIZE as u64 {
            0 => None,
            multiplier => Some(multiplier),
        }
    }

    /// The multiplier of the bucket `account`'s own state lives in.
    #[inline]
    pub fn account<S: SaltEnv>(
        &mut self,
        salt_env: &S,
        account: Address,
    ) -> Result<u64, BucketError<S::Error>> {
        self.of_bucket(salt_env, S::bucket_id_for_account(account))
    }

    /// The multiplier of the bucket the slot `key` of `address` lives in.
    #[inline]
    pub fn slot<S: SaltEnv>(
        &mut self,
        salt_env: &S,
        address: Address,
        key: StorageKey,
    ) -> Result<u64, BucketError<S::Error>> {
        self.of_bucket(salt_env, S::bucket_id_for_slot(address, key))
    }

    /// The multiplier of `bucket`, read from `salt_env` the first time this transaction asks for
    /// it and from the cache afterwards.
    ///
    /// Only a multiplier is cached, so a bucket whose capacity could not be turned into one is
    /// asked for again rather than remembered as anything.
    #[inline]
    pub(crate) fn of_bucket<S: SaltEnv>(
        &mut self,
        salt_env: &S,
        bucket: BucketId,
    ) -> Result<u64, BucketError<S::Error>> {
        match self.cached(bucket) {
            Some(multiplier) => Ok(multiplier),
            None => self.fetch(salt_env, bucket),
        }
    }

    /// The multiplier of `bucket` this transaction has already read, if it has.
    #[inline]
    pub fn cached(&self, bucket: BucketId) -> Option<u64> {
        self.cached.get(&bucket).copied()
    }

    /// Reads the capacity of `bucket` from `salt_env`, turns it into the multiplier and caches
    /// it: the cache miss of [`of_bucket`](Self::of_bucket), on its own so a caller that must know
    /// whether the environment was asked can ask it first.
    ///
    /// A bucket the cache already holds is asked for again, and the answer replaces the cached
    /// multiplier.
    #[inline]
    pub(crate) fn fetch<S: SaltEnv>(
        &mut self,
        salt_env: &S,
        bucket: BucketId,
    ) -> Result<u64, BucketError<S::Error>> {
        let capacity = salt_env.get_bucket_capacity(bucket).map_err(BucketError::Env)?;
        let multiplier =
            Self::for_capacity(capacity).ok_or(BucketError::BelowMinimum { bucket, capacity })?;
        self.cached.insert(bucket, multiplier);
        Ok(multiplier)
    }

    /// The buckets this transaction has read, for tests and tools that check the read set.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cached_buckets(&self) -> impl ExactSizeIterator<Item = BucketId> + '_ {
        self.cached.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TestExternalEnvs, MIN_BUCKET_SIZE};
    use alloy_primitives::{address, U256};

    const ACCOUNT: Address = address!("00000000000000000000000000000000000000a1");
    const OTHER: Address = address!("00000000000000000000000000000000000000a2");

    type Env = TestExternalEnvs;

    fn account_bucket(account: Address) -> BucketId {
        <Env as SaltEnv>::bucket_id_for_account(account)
    }

    fn slot_bucket(address: Address, key: StorageKey) -> BucketId {
        <Env as SaltEnv>::bucket_id_for_slot(address, key)
    }

    /// A bucket at the minimum capacity multiplies by one, so the schedule's own price stands.
    #[test]
    fn test_the_minimum_bucket_multiplies_by_one() {
        let env = Env::new();
        let mut cache = BucketMultipliers::default();

        assert_eq!(cache.account(&env, ACCOUNT), Ok(1));
        assert_eq!(cache.slot(&env, ACCOUNT, U256::ZERO), Ok(1));
    }

    /// The multiplier is the capacity in minimum buckets, over the range a capacity can take.
    #[test]
    fn test_the_multiplier_is_the_capacity_in_minimum_buckets() {
        let min = MIN_BUCKET_SIZE as u64;
        for factor in [1, 2, 8, 1_000, u32::MAX as u64] {
            assert_eq!(BucketMultipliers::for_capacity(min * factor), Some(factor), "{factor}x");
        }
        // A capacity between two whole multiples rounds down to the lower one: a bucket pays for
        // the minimum buckets it has filled, not for the one it has started.
        assert_eq!(BucketMultipliers::for_capacity(min * 2 + min / 2), Some(2));
        assert_eq!(BucketMultipliers::for_capacity(u64::MAX), Some(u64::MAX / min));
    }

    /// A capacity below the minimum is a capacity no bucket can have, so it is reported as a
    /// failed lookup rather than rounded up to the minimum: a backend that misreports must fail
    /// the transaction, not hand it the cheapest price there is.
    #[test]
    fn test_a_capacity_below_the_minimum_is_a_failed_lookup() {
        for capacity in [0, 1, MIN_BUCKET_SIZE as u64 - 1] {
            assert_eq!(BucketMultipliers::for_capacity(capacity), None, "capacity {capacity}");
        }

        let bucket = account_bucket(ACCOUNT);
        let env = Env::new().with_bucket_capacity(bucket, 0);
        let mut cache = BucketMultipliers::default();
        assert_eq!(
            cache.account(&env, ACCOUNT),
            Err(BucketError::BelowMinimum { bucket, capacity: 0 }),
        );
        assert_eq!(cache.cached_buckets().len(), 0, "nothing was cached for it");
    }

    /// The two failures read differently, so a node can tell a backend that could not answer from
    /// one that answered something impossible.
    #[test]
    fn test_the_two_bucket_failures_say_what_went_wrong() {
        let unreachable: BucketError<String> = BucketError::Env("salt backend unreachable".into());
        assert_eq!(unreachable.to_string(), "salt backend unreachable");

        let below: BucketError<String> = BucketError::BelowMinimum { bucket: 7, capacity: 3 };
        assert_eq!(
            below.to_string(),
            format!("bucket 7 reports capacity 3, below the minimum bucket of {MIN_BUCKET_SIZE}"),
        );
    }

    /// A crowded bucket scales every charge on it, and an account and a slot are read through
    /// their own buckets.
    #[test]
    fn test_a_crowded_bucket_scales_the_account_and_the_slot_separately() {
        let min = MIN_BUCKET_SIZE as u64;
        let env = Env::new()
            .with_bucket_capacity(account_bucket(ACCOUNT), min * 8)
            .with_bucket_capacity(slot_bucket(ACCOUNT, U256::ZERO), min * 2);
        let mut cache = BucketMultipliers::default();

        assert_eq!(cache.account(&env, ACCOUNT), Ok(8));
        assert_eq!(cache.slot(&env, ACCOUNT, U256::ZERO), Ok(2));
        // A slot of the same account in another bucket is untouched by either.
        assert_eq!(cache.slot(&env, ACCOUNT, U256::from(1)), Ok(1));
        // So is another account.
        assert_eq!(cache.account(&env, OTHER), Ok(1));
    }

    /// A bucket is read from the environment once; every later charge on it is answered from the
    /// cache. This is what keeps the pricing hook off the database on the hot path.
    #[test]
    fn test_a_bucket_is_read_once_and_cached() {
        let bucket = account_bucket(ACCOUNT);
        let env = Env::new().with_bucket_capacity(bucket, MIN_BUCKET_SIZE as u64 * 4);
        let mut cache = BucketMultipliers::default();

        for _ in 0..8 {
            assert_eq!(cache.account(&env, ACCOUNT), Ok(4));
        }
        assert_eq!(env.bucket_queries(bucket), 1, "one capacity query for eight charges");
        assert_eq!(cache.cached_buckets().collect::<Vec<_>>(), vec![bucket]);
    }

    /// The cache belongs to one transaction: a reset forgets it, and the next transaction reads
    /// the capacity again.
    #[test]
    fn test_a_reset_forgets_what_the_transaction_read() {
        let bucket = account_bucket(ACCOUNT);
        let env = Env::new().with_bucket_capacity(bucket, MIN_BUCKET_SIZE as u64 * 4);
        let mut cache = BucketMultipliers::default();

        assert_eq!(cache.account(&env, ACCOUNT), Ok(4));
        cache.reset();
        assert_eq!(cache.cached_buckets().len(), 0, "the reset emptied the cache");
        assert_eq!(cache.account(&env, ACCOUNT), Ok(4));
        assert_eq!(env.bucket_queries(bucket), 2, "one query per transaction");
    }

    /// A failed capacity query is reported, not swallowed: the caller must fail the transaction
    /// rather than price the charge at some other number.
    #[test]
    fn test_a_failed_capacity_query_is_reported() {
        let bucket = account_bucket(ACCOUNT);
        let env = TestExternalEnvs::<String>::new()
            .with_failing_bucket(bucket, "salt backend unreachable".into());
        let mut cache = BucketMultipliers::default();

        assert_eq!(
            cache.account(&env, ACCOUNT),
            Err(BucketError::Env("salt backend unreachable".into())),
        );
        assert_eq!(cache.cached_buckets().len(), 0, "a failed lookup caches nothing");
        // It is not cached either, so the next charge asks again.
        assert!(cache.account(&env, ACCOUNT).is_err());
        assert_eq!(env.bucket_queries(bucket), 2);
    }

    /// One bucket failing does not stop another from being priced.
    #[test]
    fn test_only_the_failing_bucket_fails() {
        let env = TestExternalEnvs::<String>::new()
            .with_bucket_capacity(account_bucket(OTHER), MIN_BUCKET_SIZE as u64 * 2)
            .with_failing_bucket(account_bucket(ACCOUNT), "unreachable".into());
        let mut cache = BucketMultipliers::default();

        assert!(cache.account(&env, ACCOUNT).is_err());
        assert_eq!(cache.account(&env, OTHER), Ok(2));
    }
}
