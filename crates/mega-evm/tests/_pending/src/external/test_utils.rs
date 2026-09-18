//! Unit tests extracted from `crates/mega-evm/src/external/test_utils.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/external/test_utils.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_default_bucket_capacity_applies_to_all_buckets() {
        use crate::external::salt::SaltEnv;
        let env =
            TestExternalEnvs::<core::convert::Infallible>::new().with_default_bucket_capacity(2048);
        // Any bucket id not explicitly configured now returns the default, not MIN_BUCKET_SIZE.
        assert_eq!(env.get_bucket_capacity(123), Ok(2048));
        assert_eq!(env.get_bucket_capacity(999), Ok(2048));
        // An explicit per-bucket capacity still wins.
        let env = env.with_bucket_capacity(123, 512);
        assert_eq!(env.get_bucket_capacity(123), Ok(512));
        assert_eq!(env.get_bucket_capacity(999), Ok(2048));
    }
}
