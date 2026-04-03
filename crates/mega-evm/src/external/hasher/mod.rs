//! Deterministic hashing for mapping keys to SALT buckets.
//!
//! This module provides the [`bucket_id`] function which maps plain keys (account addresses,
//! storage slot keys) to SALT bucket IDs. The hash algorithm is a vendored subset of `AHash`
//! v0.8.12's deterministic fallback, ensuring identical results across all platforms.
//!
//! # Attribution
//!
//! The hashing implementation is derived from [AHash v0.8.12](https://github.com/tkaitchuck/aHash)
//! under Apache License 2.0. See [`NOTICE.md`](./NOTICE.md) for full attribution.

mod convert;
mod fallback;

use self::fallback::RandomState;
use super::BucketId;
use core::hash::{BuildHasher, Hasher};

/// Width of each trie level in bits (256-ary trie).
const TRIE_WIDTH_BITS: usize = 8;

/// Number of levels in the main SALT trie.
const MAIN_TRIE_LEVELS: usize = 4;

/// Total number of buckets: 256^3 = 16,777,216.
const NUM_BUCKETS: usize = 1 << ((MAIN_TRIE_LEVELS - 1) * TRIE_WIDTH_BITS);

/// Number of metadata buckets reserved at the start: 65,536.
const NUM_META_BUCKETS: usize = NUM_BUCKETS / super::MIN_BUCKET_SIZE;

/// Number of key-value data buckets: 16,711,680.
const NUM_KV_BUCKETS: usize = NUM_BUCKETS - NUM_META_BUCKETS;

/// Fixed seeds derived from the lower 32 bytes of keccak256("Make Ethereum Great Again").
const HASHER_SEEDS: [u64; 4] = [0x921321f4, 0x2ccb667e, 0x60d68842, 0x077ada9d];

/// Computes a deterministic 64-bit hash of the input bytes using the `AHash` fallback algorithm.
#[inline(always)]
fn hash(bytes: &[u8]) -> u64 {
    static HASH_BUILDER: RandomState =
        RandomState::with_seeds(HASHER_SEEDS[0], HASHER_SEEDS[1], HASHER_SEEDS[2], HASHER_SEEDS[3]);

    let mut hasher = HASH_BUILDER.build_hasher();
    hasher.write(bytes);
    hasher.finish()
}

/// Maps a plain key (account address or address+slot) to a SALT bucket ID.
///
/// Returns a bucket ID in the range `[NUM_META_BUCKETS, NUM_BUCKETS)`.
/// The first `NUM_META_BUCKETS` buckets are reserved for metadata storage.
#[inline(always)]
pub(crate) fn bucket_id(key: &[u8]) -> BucketId {
    (hash(key) % NUM_KV_BUCKETS as u64 + NUM_META_BUCKETS as u64) as BucketId
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_stability() {
        assert_eq!(hash(b"hello"), 1027176506268606463);
        assert_eq!(hash(b"world"), 2337896903564117184);
        assert_eq!(hash(b"hash test"), 2116618212096523432);
    }

    #[test]
    fn test_bucket_id_range() {
        let test_keys: &[&[u8]] = &[
            b"",
            b"a",
            b"test",
            &[0u8; 20],
            &[255u8; 20],
            &[0u8; 32],
            &[0u8; 52],
            &[255u8; 32],
            &[255u8; 1024],
        ];

        for key in test_keys {
            let id = bucket_id(key);
            assert!(
                id >= NUM_META_BUCKETS as BucketId,
                "bucket_id for {:?} is too small: {}",
                key,
                id
            );
            assert!(id < NUM_BUCKETS as BucketId, "bucket_id for {:?} is too large: {}", key, id);
        }
    }

    // Cross-verified against salt::state::hasher::bucket_id at salt v1.0.1.
    #[test]
    fn test_bucket_id_cross_check_with_ethereum_shaped_inputs() {
        let addr: [u8; 20] = [
            0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
            0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        ];
        assert_eq!(bucket_id(&addr), 12_196_828);

        let mut slot_key = [0u8; 52];
        slot_key[..20].copy_from_slice(&addr);
        slot_key[51] = 1;
        assert_eq!(bucket_id(&slot_key), 9_450_146);

        assert_eq!(bucket_id(&[0u8; 20]), 12_666_336);
    }
}
