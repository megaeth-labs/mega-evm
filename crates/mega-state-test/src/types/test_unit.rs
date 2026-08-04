use mega_evm::{revm::primitives::eip4844, MegaSpecId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{AccountInfo, Env, MegaEnv, SpecName, Test, TransactionParts};
use mega_evm::revm::{
    context::{block::BlockEnv, cfg::CfgEnv},
    database::CacheState,
    primitives::{hardfork::SpecId, keccak256, Address, Bytes, B256},
    state::Bytecode,
};

/// Single test unit struct
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
//#[serde(deny_unknown_fields)]
// field config
pub struct TestUnit {
    /// Test info is optional.
    #[serde(default, rename = "_info")]
    pub info: Option<serde_json::Value>,

    /// Test environment configuration.
    ///
    /// Contains the environmental information for executing the test, including
    /// block information, coinbase address, difficulty, gas limit, and other
    /// blockchain state parameters required for proper test execution.
    pub env: Env,

    /// Pre-execution state.
    ///
    /// A mapping of addresses to their account information before the transaction
    /// is executed. This represents the initial state of all accounts involved
    /// in the test, including their balances, nonces, code, and storage. Ordered
    /// by address so serialized fixtures are byte-reproducible.
    pub pre: BTreeMap<Address, AccountInfo>,

    /// Post-execution expectations per specification.
    ///
    /// Maps each Ethereum specification name (hardfork) to a vector of expected
    /// test results. This allows a single test to define different expected outcomes
    /// for different protocol versions, enabling comprehensive testing across
    /// multiple Ethereum upgrades.
    pub post: BTreeMap<SpecName, Vec<Test>>,

    /// Transaction details to be executed.
    ///
    /// Contains the transaction parameters that will be executed against the
    /// pre-state. This includes sender, recipient, value, data, gas limits,
    /// and other transaction fields that may vary based on indices.
    pub transaction: TransactionParts,

    /// Expected output data from the transaction execution.
    ///
    /// Optional field containing the expected return data from the transaction.
    /// This is typically used for testing contract calls that return specific
    /// values or for CREATE operations that return deployed contract addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<Bytes>,

    /// `MegaETH` external-environment inputs (SALT bucket capacities, oracle
    /// storage) required to deterministically reproduce a `MegaETH` transaction.
    ///
    /// Absent for pure-Ethereum tests, which run against the empty external
    /// environment.
    #[serde(default, rename = "megaEnv", skip_serializing_if = "Option::is_none")]
    pub mega_env: Option<MegaEnv>,

    /// Fields outside the schema (e.g. the EEST `config` block or custom
    /// metadata), captured verbatim so a deserialize→serialize round-trip —
    /// notably a `--fill` rewrite — preserves them instead of silently
    /// deleting them. Execution ignores these fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl TestUnit {
    /// Prepare the state from the test unit.
    ///
    /// This function uses [`TestUnit::pre`] to prepare the pre-state from the test unit.
    /// It creates a new cache state and inserts the accounts from the test unit.
    ///
    /// # Returns
    ///
    /// A [`CacheState`] object containing the pre-state accounts and storages.
    pub fn state(&self) -> CacheState {
        let mut cache_state = CacheState::new();
        for (address, info) in &self.pre {
            let code_hash = keccak256(&info.code);
            let bytecode = Bytecode::new_raw_checked(info.code.clone())
                .unwrap_or_else(|_| Bytecode::new_legacy(info.code.clone()));
            let acc_info = mega_evm::revm::state::AccountInfo {
                balance: info.balance,
                code_hash,
                account_id: None,
                code: Some(bytecode),
                nonce: info.nonce,
            };
            cache_state.insert_account_with_storage(
                *address,
                acc_info,
                info.storage.iter().map(|(k, v)| (*k, *v)).collect(),
            );
        }
        cache_state
    }

    /// Create a block environment from the test unit.
    ///
    /// This function sets up the block environment using the current test unit's
    /// environment settings and the provided configuration.
    ///
    /// # Arguments
    ///
    /// * `cfg` - The configuration environment containing spec and blob settings
    ///
    /// # Returns
    ///
    /// A configured [`BlockEnv`] ready for execution
    pub fn block_env(&self, cfg: &CfgEnv<MegaSpecId>) -> BlockEnv {
        let mut block = BlockEnv {
            number: self.env.current_number,
            beneficiary: self.env.current_coinbase,
            timestamp: self.env.current_timestamp,
            gas_limit: self.env.current_gas_limit.try_into().unwrap_or(u64::MAX),
            basefee: self.env.current_base_fee.unwrap_or_default().try_into().unwrap_or(u64::MAX),
            difficulty: self.env.current_difficulty,
            prevrandao: self.env.current_random.map(|i| i.into()),
            ..BlockEnv::default()
        };

        // Handle EIP-4844 blob gas
        if let Some(current_excess_blob_gas) = self.env.current_excess_blob_gas {
            block.set_blob_excess_gas_and_price(
                current_excess_blob_gas.to(),
                eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        } else if let (Some(parent_blob_gas_used), Some(parent_excess_blob_gas)) =
            (self.env.parent_blob_gas_used, self.env.parent_excess_blob_gas)
        {
            // A fixture may pin the parent's target blob gas per block; only fall
            // back to the Cancun target when it does not.
            let parent_target_blob_gas_per_block = self
                .env
                .parent_target_blobs_per_block
                .map_or(eip4844::TARGET_BLOB_GAS_PER_BLOCK_CANCUN, |target| target.to());
            block.set_blob_excess_gas_and_price(
                calc_excess_blob_gas(
                    parent_excess_blob_gas.to(),
                    parent_blob_gas_used.to(),
                    parent_target_blob_gas_per_block,
                ),
                eip4844::BLOB_BASE_FEE_UPDATE_FRACTION_CANCUN,
            );
        }

        // Set default prevrandao for merge
        if cfg.spec.into_eth_spec().is_enabled_in(SpecId::MERGE) && block.prevrandao.is_none() {
            block.prevrandao = Some(B256::default());
        }

        block
    }
}

/// Calculates a block's `excess_blob_gas` from its parent's blob-gas fields.
///
/// The EIP-4844 update rule, parameterized on the parent's target blob gas per
/// block. Alloy's `calc_excess_blob_gas` pins the Cancun target instead of taking
/// it as an argument, so it cannot serve fixtures that declare their own target.
fn calc_excess_blob_gas(
    parent_excess_blob_gas: u64,
    parent_blob_gas_used: u64,
    parent_target_blob_gas_per_block: u64,
) -> u64 {
    parent_excess_blob_gas
        .saturating_add(parent_blob_gas_used)
        .saturating_sub(parent_target_blob_gas_per_block)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a fixture whose only meaningful content is the blob-gas part of
    /// `env`, so [`TestUnit::block_env`] can be exercised in isolation.
    ///
    /// `env_blob_fields` is spliced verbatim into the `env` object, so it must be
    /// a comma-terminated list of JSON members.
    fn blob_fixture(env_blob_fields: &str) -> TestUnit {
        let json = format!(
            r#"{{
                "env": {{
                    {env_blob_fields}
                    "currentCoinbase": "0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba",
                    "currentGasLimit": "0x016345785d8a0000",
                    "currentNumber": "0x01",
                    "currentTimestamp": "0x03e8"
                }},
                "pre": {{}},
                "post": {{}},
                "transaction": {{
                    "data": [],
                    "gasLimit": [],
                    "nonce": "0x00",
                    "secretKey": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "value": []
                }}
            }}"#
        );
        serde_json::from_str(&json).expect("blob fixture deserializes")
    }

    fn excess_blob_gas(unit: &TestUnit) -> u64 {
        unit.block_env(&CfgEnv::<MegaSpecId>::default())
            .blob_excess_gas_and_price
            .expect("blob excess gas and price is set")
            .excess_blob_gas
    }

    #[test]
    fn test_block_env_derives_excess_blob_gas_from_fixture_target() {
        // Parent used exactly its own (non-Cancun) target, so the excess does not
        // grow. Deriving against the Cancun target instead would leave
        // 786432 - 393216 = 393216 behind.
        let unit = blob_fixture(
            r#""parentExcessBlobGas": "0x00",
               "parentBlobGasUsed": "0xc0000",
               "parentTargetBlobsPerBlock": "0xc0000","#,
        );
        assert_eq!(excess_blob_gas(&unit), 0);
    }

    #[test]
    fn test_block_env_derives_excess_blob_gas_above_fixture_target() {
        // 131072 + 786432 - 262144 = 655360.
        let unit = blob_fixture(
            r#""parentExcessBlobGas": "0x20000",
               "parentBlobGasUsed": "0xc0000",
               "parentTargetBlobsPerBlock": "0x40000","#,
        );
        assert_eq!(excess_blob_gas(&unit), 655_360);
    }

    #[test]
    fn test_block_env_falls_back_to_cancun_target_without_fixture_target() {
        // 0 + 786432 - 393216 = 393216.
        let unit = blob_fixture(
            r#""parentExcessBlobGas": "0x00",
               "parentBlobGasUsed": "0xc0000","#,
        );
        assert_eq!(excess_blob_gas(&unit), 786_432 - eip4844::TARGET_BLOB_GAS_PER_BLOCK_CANCUN);
    }

    #[test]
    fn test_block_env_current_excess_blob_gas_wins_over_parent_fields() {
        // An explicit `currentExcessBlobGas` is the block's value as-is; the
        // parent fields, fixture target included, are not consulted.
        let unit = blob_fixture(
            r#""currentExcessBlobGas": "0x20000",
               "parentExcessBlobGas": "0x00",
               "parentBlobGasUsed": "0xc0000",
               "parentTargetBlobsPerBlock": "0xc0000","#,
        );
        assert_eq!(excess_blob_gas(&unit), 131_072);
    }

    #[test]
    fn test_cancun_target_derivation_matches_alloy() {
        // Fixtures without a target keep deriving exactly what the Cancun-pinned
        // alloy helper produced, so replacing that call changed nothing for them.
        for parent_excess_blob_gas in [0, 1, 131_072, 393_215, 393_216, 786_432, 10_000_000] {
            for parent_blob_gas_used in [0, 1, 131_072, 393_216, 786_432, 1_000_000] {
                assert_eq!(
                    calc_excess_blob_gas(
                        parent_excess_blob_gas,
                        parent_blob_gas_used,
                        eip4844::TARGET_BLOB_GAS_PER_BLOCK_CANCUN,
                    ),
                    alloy_eips::eip4844::calc_excess_blob_gas(
                        parent_excess_blob_gas,
                        parent_blob_gas_used,
                    ),
                    "excess={parent_excess_blob_gas} used={parent_blob_gas_used}"
                );
            }
        }
    }
}
