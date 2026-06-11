use mega_evm::{
    revm::{
        context::tx::TxEnv,
        primitives::{Address, Bytes, HashMap, TxKind, B256},
    },
    Either,
};
use serde::{Deserialize, Serialize};

use super::{error::TestError, transaction::TxPartIndices, AccountInfo, TestUnit};
use crate::utils::recover_address;

/// State test indexed state result deserialization.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Test {
    /// Expected exception for this test case, if any.
    ///
    /// This field contains an optional string describing an expected error or exception
    /// that should occur during the execution of this state test. If present, the test
    /// is expected to fail with this specific error message or exception type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_exception: Option<String>,

    /// Indexes
    pub indexes: TxPartIndices,
    /// Post state hash
    pub hash: B256,
    /// Post state
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub post_state: HashMap<Address, AccountInfo>,

    /// Logs root
    pub logs: B256,

    /// Output state.
    ///
    /// Note: Not used.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    state: HashMap<Address, AccountInfo>,

    /// Tx bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txbytes: Option<Bytes>,

    /// `MegaETH`: expected total gas used by the transaction.
    ///
    /// When present, the runner checks the actual gas used against this value
    /// and reports a readable diff on mismatch (in addition to the state-root
    /// backstop). Absent for pure-Ethereum tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mega_gas_used: Option<u64>,

    /// `MegaETH`: expected execution status — one of `"success"`, `"revert"`,
    /// or `"halt"`. When present, the runner checks the actual status against
    /// this value. Absent for pure-Ethereum tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mega_status: Option<String>,
}

impl Test {
    /// Construct a `post` expectation for a dumped replay fixture.
    ///
    /// Records the canonical state/logs roots plus the explicit `MegaETH` gas and
    /// status expectations, at transaction index 0. `expect_exception`,
    /// `post_state`, `state`, and `txbytes` are left empty/`None` — they are not
    /// part of a replay-derived fixture.
    pub fn for_dump(hash: B256, logs: B256, mega_gas_used: u64, mega_status: String) -> Self {
        Self {
            expect_exception: None,
            indexes: TxPartIndices { data: 0, gas: 0, value: 0 },
            hash,
            post_state: HashMap::default(),
            logs,
            state: HashMap::default(),
            txbytes: None,
            mega_gas_used: Some(mega_gas_used),
            mega_status: Some(mega_status),
        }
    }

    /// Create a transaction environment from this test and the test unit.
    ///
    /// This function sets up the transaction environment using the test's
    /// indices to select the appropriate transaction parameters from the
    /// test unit.
    ///
    /// # Arguments
    ///
    /// * `unit` - The test unit containing transaction parts
    ///
    /// # Returns
    ///
    /// A configured [`TxEnv`] ready for execution, or an error if setup fails
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The private key cannot be used to recover the sender address
    /// - The transaction type is invalid and no exception is expected
    /// - A transaction field value does not fit its EVM integer width
    pub fn tx_env(&self, unit: &TestUnit) -> Result<TxEnv, TestError> {
        tx_env_at(unit, self.indexes).map_err(|e| match e {
            // Preserve the existing expect-exception messaging for state tests that
            // intentionally encode an invalid transaction type.
            TestError::InvalidTransactionType if self.expect_exception.is_some() => {
                TestError::UnexpectedException {
                    expected_exception: self.expect_exception.clone(),
                    got_exception: Some("Invalid transaction type".to_string()),
                }
            }
            other => other,
        })
    }
}

/// Build a [`TxEnv`] from a unit's transaction at the given part indices.
///
/// Shared by [`Test::tx_env`] (which selects indices per post-state entry) and by
/// single-unit dump/replay execution (which always uses index 0). Returns
/// [`TestError::InvalidTransactionType`] when the transaction type cannot be
/// derived; callers that expect an exception remap it.
pub fn tx_env_at(unit: &TestUnit, indexes: TxPartIndices) -> Result<TxEnv, TestError> {
    // Setup sender
    let caller = if let Some(address) = unit.transaction.sender {
        address
    } else {
        recover_address(unit.transaction.secret_key.as_slice())
            .ok_or(TestError::UnknownPrivateKey(unit.transaction.secret_key))?
    };

    // Transaction specific fields
    let tx_type =
        unit.transaction.tx_type(indexes.data).ok_or(TestError::InvalidTransactionType)?;

    let tx = TxEnv {
        caller,
        gas_price: unit
            .transaction
            .gas_price
            .or(unit.transaction.max_fee_per_gas)
            .unwrap_or_default()
            .try_into()
            .unwrap_or(u128::MAX),
        gas_priority_fee: unit
            .transaction
            .max_priority_fee_per_gas
            .map(|b| {
                u128::try_from(b).map_err(|_| TestError::ValueOutOfRange {
                    field: "maxPriorityFeePerGas",
                    value: b,
                })
            })
            .transpose()?,
        blob_hashes: unit.transaction.blob_versioned_hashes.clone(),
        max_fee_per_blob_gas: unit
            .transaction
            .max_fee_per_blob_gas
            .map(|b| {
                u128::try_from(b)
                    .map_err(|_| TestError::ValueOutOfRange { field: "maxFeePerBlobGas", value: b })
            })
            .transpose()?
            .unwrap_or(u128::MAX),
        tx_type: tx_type as u8,
        gas_limit: unit
            .transaction
            .gas_limit
            .get(indexes.gas)
            .ok_or(TestError::PartIndexOutOfBounds {
                part: "gasLimit",
                index: indexes.gas,
                len: unit.transaction.gas_limit.len(),
            })?
            .saturating_to(),
        data: unit
            .transaction
            .data
            .get(indexes.data)
            .ok_or(TestError::PartIndexOutOfBounds {
                part: "data",
                index: indexes.data,
                len: unit.transaction.data.len(),
            })?
            .clone(),
        nonce: u64::try_from(unit.transaction.nonce).map_err(|_| TestError::ValueOutOfRange {
            field: "nonce",
            value: unit.transaction.nonce,
        })?,
        value: *unit.transaction.value.get(indexes.value).ok_or(
            TestError::PartIndexOutOfBounds {
                part: "value",
                index: indexes.value,
                len: unit.transaction.value.len(),
            },
        )?,
        access_list: unit
            .transaction
            .access_lists
            .get(indexes.data)
            .cloned()
            .flatten()
            .unwrap_or_default(),
        authorization_list: unit
            .transaction
            .authorization_list
            .clone()
            .map(|auth_list| {
                auth_list.into_iter().map(|i| Either::Left(i.into())).collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        kind: match unit.transaction.to {
            Some(add) => TxKind::Call(add),
            None => TxKind::Create,
        },
        ..TxEnv::default()
    };

    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::revm::primitives::U256;

    /// Minimal valid unit: explicit sender (no key recovery), legacy transfer.
    fn unit_json() -> serde_json::Value {
        serde_json::json!({
            "env": {
                "currentCoinbase": "0x3000000000000000000000000000000000000003",
                "currentDifficulty": "0x0",
                "currentGasLimit": "0x1c9c380",
                "currentNumber": "0x10",
                "currentTimestamp": "0x3e8"
            },
            "pre": {},
            "post": {},
            "transaction": {
                "type": 0,
                "data": ["0x"],
                "gasLimit": ["0x30d40"],
                "gasPrice": "0x0",
                "nonce": "0x0",
                "secretKey": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "sender": "0x1000000000000000000000000000000000000001",
                "to": "0x2000000000000000000000000000000000000002",
                "value": ["0x0"]
            }
        })
    }

    fn unit_with(patch: impl FnOnce(&mut serde_json::Value)) -> TestUnit {
        let mut json = unit_json();
        patch(&mut json["transaction"]);
        serde_json::from_value(json).expect("parse unit")
    }

    const ZERO_INDEXES: TxPartIndices = TxPartIndices { data: 0, gas: 0, value: 0 };

    fn assert_out_of_range(unit: &TestUnit, field: &'static str) {
        match tx_env_at(unit, ZERO_INDEXES) {
            Err(TestError::ValueOutOfRange { field: got, .. }) => assert_eq!(got, field),
            other => panic!("expected ValueOutOfRange for `{field}`, got {other:?}"),
        }
    }

    #[test]
    fn tx_env_at_in_range_values_succeed() {
        // Largest representable values must still build (full predicate domain).
        let unit = unit_with(|tx| {
            tx["nonce"] = serde_json::json!(format!("{:#x}", u64::MAX));
            tx["maxPriorityFeePerGas"] = serde_json::json!(format!("{:#x}", u128::MAX));
            tx["maxFeePerGas"] = serde_json::json!(format!("{:#x}", u128::MAX));
        });
        let tx = tx_env_at(&unit, ZERO_INDEXES).expect("in-range values must build");
        assert_eq!(tx.nonce, u64::MAX);
        assert_eq!(tx.gas_priority_fee, Some(u128::MAX));
    }

    #[test]
    fn tx_env_at_nonce_overflow_is_structured_error() {
        let unit =
            unit_with(|tx| tx["nonce"] = serde_json::json!(format!("{:#x}", u64::MAX as u128 + 1)));
        assert_out_of_range(&unit, "nonce");
    }

    #[test]
    fn tx_env_at_priority_fee_overflow_is_structured_error() {
        let unit = unit_with(|tx| {
            tx["maxPriorityFeePerGas"] = serde_json::json!(format!("{:#x}", U256::MAX));
        });
        assert_out_of_range(&unit, "maxPriorityFeePerGas");
    }

    #[test]
    fn tx_env_at_blob_fee_overflow_is_structured_error() {
        let unit = unit_with(|tx| {
            // EIP-4844 shape needs an explicit type + destination (already set).
            tx["type"] = serde_json::json!(3);
            tx["maxFeePerBlobGas"] = serde_json::json!(format!("{:#x}", U256::MAX));
        });
        assert_out_of_range(&unit, "maxFeePerBlobGas");
    }

    #[test]
    fn tx_env_at_gas_price_and_gas_limit_keep_saturating() {
        // Intentionally-preserved EEST behavior: gasPrice and gasLimit saturate
        // instead of erroring, so upstream corpus runs are unaffected.
        let unit = unit_with(|tx| {
            tx["gasPrice"] = serde_json::json!(format!("{:#x}", U256::MAX));
            tx["gasLimit"] = serde_json::json!([format!("{:#x}", U256::MAX)]);
        });
        let tx = tx_env_at(&unit, ZERO_INDEXES).expect("saturating fields must not error");
        assert_eq!(tx.gas_price, u128::MAX);
        assert_eq!(tx.gas_limit, u64::MAX);
    }
}
