use mega_evm::{revm::primitives::U256, BucketHasher, TestExternalEnvs};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;

/// `MegaETH` external-environment inputs that are not part of the standard EEST
/// state-test schema but are required to deterministically reproduce a `MegaETH`
/// transaction (dynamic SALT-bucket gas pricing and oracle reads).
///
/// When a `TestUnit` omits this field the runner falls back to the empty
/// external environment, leaving pure-Ethereum tests unaffected.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MegaEnv {
    /// SALT bucket capacity pairs `(bucket_id, capacity)`.
    #[serde(default)]
    pub bucket_capacities: Vec<(u32, u64)>,
    /// Oracle contract storage `(slot, value)` pairs served during execution.
    #[serde(default)]
    pub oracle_storage: Vec<(U256, U256)>,
}

impl MegaEnv {
    /// Checks that the recorded values are usable by the runner.
    ///
    /// Every bucket capacity must be at least [`mega_evm::MIN_BUCKET_SIZE`]:
    /// `DynamicGasCost` asserts this at lookup time, so a smaller capacity in a
    /// hand-edited fixture would abort the process instead of failing the test.
    pub fn validate(&self) -> Result<(), String> {
        for &(bucket_id, capacity) in &self.bucket_capacities {
            if capacity < mega_evm::MIN_BUCKET_SIZE as u64 {
                return Err(format!(
                    "megaEnv bucket {bucket_id} capacity {capacity} is below \
                     MIN_BUCKET_SIZE ({})",
                    mega_evm::MIN_BUCKET_SIZE
                ));
            }
        }
        Ok(())
    }

    /// Build a [`TestExternalEnvs`] that reproduces the recorded SALT bucket
    /// capacities and oracle storage.
    ///
    /// The hasher is left generic so callers in different crates (which use
    /// different concrete bucket hashers) can pick their own.
    pub fn to_external_envs<H: BucketHasher>(&self) -> TestExternalEnvs<Infallible, H> {
        let mut envs = TestExternalEnvs::new();
        for &(bucket_id, capacity) in &self.bucket_capacities {
            envs = envs.with_bucket_capacity(bucket_id, capacity);
        }
        for &(slot, value) in &self.oracle_storage {
            envs = envs.with_oracle_storage(slot, value);
        }
        envs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mega_env_json_roundtrip() {
        let env = MegaEnv {
            bucket_capacities: vec![(1, 100), (42, 2_000_000)],
            oracle_storage: vec![(U256::from(0), U256::from(7)), (U256::from(9), U256::MAX)],
        };

        let json = serde_json::to_string(&env).expect("serialize");
        let back: MegaEnv = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(env, back);
    }

    #[test]
    fn test_mega_env_camel_case_field_names() {
        let env = MegaEnv {
            bucket_capacities: vec![(1, 100)],
            oracle_storage: vec![(U256::from(0), U256::from(7))],
        };
        let value = serde_json::to_value(&env).expect("serialize");
        assert!(value.get("bucketCapacities").is_some(), "expected camelCase bucketCapacities");
        assert!(value.get("oracleStorage").is_some(), "expected camelCase oracleStorage");
    }

    #[test]
    fn test_mega_env_empty_fields_default() {
        // Missing fields should default to empty vectors.
        let env: MegaEnv = serde_json::from_str("{}").expect("deserialize empty");
        assert!(env.bucket_capacities.is_empty());
        assert!(env.oracle_storage.is_empty());
    }
}
