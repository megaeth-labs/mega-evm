//! Unit tests extracted from `crates/mega-evm/src/external/hasher/mod.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/external/hasher/mod.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

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

    #[test]
    fn test_hash_all_write_branches() {
        let h0 = hash(b"");
        let h1 = hash(&[0x42]);
        let h2 = hash(&[0x01, 0x02]);
        let h3 = hash(&[0x01, 0x02, 0x03]);
        let h4 = hash(&[0x01, 0x02, 0x03, 0x04]);
        let h7 = hash(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        let h8 = hash(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        let h9 = hash(&[0xaa; 9]);
        let h16 = hash(&[0xbb; 16]);
        let h17 = hash(&[0xcc; 17]);
        let h33 = hash(&[0xdd; 33]);
        let h48 = hash(&[0xee; 48]);

        let hashes = [h0, h1, h2, h3, h4, h7, h8, h9, h16, h17, h33, h48];
        for i in 0..hashes.len() {
            for j in (i + 1)..hashes.len() {
                assert_ne!(hashes[i], hashes[j], "hash collision at indices {i} and {j}");
            }
        }
    }

    #[test]
    fn test_hasher_typed_write_methods() {
        use core::hash::{BuildHasher, Hasher};

        let state = fallback::RandomState::with_seeds(
            HASHER_SEEDS[0],
            HASHER_SEEDS[1],
            HASHER_SEEDS[2],
            HASHER_SEEDS[3],
        );

        let finish_with = |f: fn(&mut fallback::DeterministicHasher)| -> u64 {
            let mut h = state.build_hasher();
            f(&mut h);
            h.finish()
        };

        let h_u8 = finish_with(|h| h.write_u8(0x42));
        let h_u16 = finish_with(|h| h.write_u16(0x4200));
        let h_u32 = finish_with(|h| h.write_u32(0x42000000));
        let h_u64 = finish_with(|h| h.write_u64(0x42));
        let h_u128 = finish_with(|h| h.write_u128(0x42));
        let h_usize = finish_with(|h| h.write_usize(0x42));

        assert_eq!(h_u64, h_usize);

        let hashes = [h_u8, h_u16, h_u32, h_u128];
        for i in 0..hashes.len() {
            for j in (i + 1)..hashes.len() {
                assert_ne!(hashes[i], hashes[j], "typed write collision at {i} and {j}");
            }
        }
    }
}
