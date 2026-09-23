//! Matching a rejected transaction against the exception its fixture expects.
//!
//! A fixture names the exception an invalid transaction must raise (`expectException`), as one
//! name or as alternatives joined by `|`: `TransactionException.INTRINSIC_GAS_TOO_LOW`. A
//! rejection passes only if it is one of the named ones: the runner maps each validation error
//! the engine raises to the names that describe it, and a rejection for another reason — or for a
//! reason with no name here — is a failure, not a pass. Accepting any error would let a
//! transaction rejected for the wrong reason, say a nonce check that fires before the gas check a
//! fixture is about, pass the gate.
//!
//! One pair of names labels a single rule. The execution specs reject a gas limit below the
//! intrinsic cost or below the EIP-7623 calldata floor with one error; the fixtures' two names,
//! `INTRINSIC_GAS_TOO_LOW` and `INTRINSIC_GAS_BELOW_FLOOR_GAS_COST`, label which of the two costs
//! the test drew the larger, in the fixture's own accounting. The Amsterdam fixtures' accounting
//! can call `INTRINSIC_GAS_TOO_LOW` a limit revm finds above the intrinsic cost and below the
//! floor, so on the Amsterdam fixtures a floor shortfall satisfies both names. The Osaka fixtures
//! never do, and on them each shortfall satisfies only its own name; an intrinsic shortfall
//! satisfies only its own on both.

use mega_evm::{
    op_revm::OpTransactionError,
    revm::context::result::{EVMError, InvalidTransaction},
};

use crate::{
    types::{TestError, TestUnit},
    Fork,
};

/// The prefix of every transaction exception name.
const PREFIX: &str = "TransactionException.";

/// The execution-spec exception names `error` satisfies on `fork`'s fixtures, without the
/// `TransactionException.` prefix; empty for an error no fixture exception describes.
pub fn names(fork: Fork, error: &InvalidTransaction) -> &'static [&'static str] {
    use InvalidTransaction as E;
    match error {
        E::PriorityFeeGreaterThanMaxFee => &["PRIORITY_GREATER_THAN_MAX_FEE_PER_GAS"],
        E::GasPriceLessThanBasefee => &["INSUFFICIENT_MAX_FEE_PER_GAS"],
        E::CallerGasLimitMoreThanBlock => &["GAS_ALLOWANCE_EXCEEDED"],
        E::CallGasCostMoreThanGasLimit { .. } => &["INTRINSIC_GAS_TOO_LOW"],
        E::GasFloorMoreThanGasLimit { .. } => match fork {
            Fork::Osaka => &["INTRINSIC_GAS_BELOW_FLOOR_GAS_COST"],
            Fork::Amsterdam => &["INTRINSIC_GAS_BELOW_FLOOR_GAS_COST", "INTRINSIC_GAS_TOO_LOW"],
        },
        E::RejectCallerWithCode => &["SENDER_NOT_EOA"],
        E::LackOfFundForMaxFee { .. } => &["INSUFFICIENT_ACCOUNT_FUNDS"],
        E::OverflowPaymentInTransaction => &["GASLIMIT_PRICE_PRODUCT_OVERFLOW"],
        E::NonceOverflowInTransaction => &["NONCE_IS_MAX"],
        E::NonceTooHigh { .. } => &["NONCE_MISMATCH_TOO_HIGH"],
        E::NonceTooLow { .. } => &["NONCE_MISMATCH_TOO_LOW"],
        E::CreateInitCodeSizeLimit => &["INITCODE_SIZE_EXCEEDED"],
        E::InvalidChainId | E::MissingChainId => &["INVALID_CHAINID"],
        E::TxGasLimitGreaterThanCap { .. } => &["GAS_LIMIT_EXCEEDS_MAXIMUM"],
        E::BlobGasPriceGreaterThanMax { .. } => &["INSUFFICIENT_MAX_FEE_PER_BLOB_GAS"],
        E::EmptyBlobs => &["TYPE_3_TX_ZERO_BLOBS"],
        E::BlobCreateTransaction => &["TYPE_3_TX_CONTRACT_CREATION"],
        E::TooManyBlobs { .. } => {
            &["TYPE_3_TX_BLOB_COUNT_EXCEEDED", "TYPE_3_TX_MAX_BLOB_GAS_ALLOWANCE_EXCEEDED"]
        }
        E::BlobVersionNotSupported => &["TYPE_3_TX_INVALID_BLOB_VERSIONED_HASH"],
        E::EmptyAuthorizationList => &["TYPE_4_EMPTY_AUTHORIZATION_LIST"],
        _ => &[],
    }
}

/// Why a rejection does not satisfy what its fixture expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    /// The error has a name, and it is not one the fixture expects.
    Wrong {
        /// The names the error satisfies.
        got: &'static [&'static str],
    },
    /// The error has no execution-spec name at all.
    Unnamed,
}

/// Whether `error` is one of the exceptions `expected`, in a fixture of `fork`, names.
pub fn check<DBError>(
    fork: Fork,
    expected: &str,
    error: &EVMError<DBError, OpTransactionError>,
) -> Result<(), Mismatch> {
    let EVMError::Transaction(OpTransactionError::Base(invalid)) = error else {
        return Err(Mismatch::Unnamed);
    };
    check_names(expected, names(fork, invalid))
}

/// The exception names a transaction the fixture types cannot build satisfies: an invalid
/// signature for one without a key to sign it or with a key that recovers no sender, a creation
/// for a blob or EIP-7702 transaction without a recipient. Empty for anything else.
pub fn unbuildable_names(error: &TestError, unit: &TestUnit) -> &'static [&'static str] {
    let invalid_type = || {
        if unit.transaction.max_fee_per_blob_gas.is_some() {
            &["TYPE_3_TX_CONTRACT_CREATION"][..]
        } else if unit.transaction.authorization_list.is_some() {
            &["TYPE_4_TX_CONTRACT_CREATION"][..]
        } else {
            &[][..]
        }
    };
    match error {
        TestError::UnknownPrivateKey(_) => &["INVALID_SIGNATURE_VRS"],
        TestError::InvalidTransactionType => invalid_type(),
        TestError::UnexpectedException { got_exception, .. } => match got_exception.as_deref() {
            Some("Missing secret key") => &["INVALID_SIGNATURE_VRS"],
            Some("Invalid transaction type") => invalid_type(),
            _ => &[],
        },
    }
}

/// Whether a transaction the fixture types cannot build is invalid for one of the reasons
/// `expected` names.
pub fn check_unbuildable(
    expected: &str,
    error: &TestError,
    unit: &TestUnit,
) -> Result<(), Mismatch> {
    check_names(expected, unbuildable_names(error, unit))
}

/// Whether `got`, the names an error satisfies, includes one of the ones `expected` names.
fn check_names(expected: &str, got: &'static [&'static str]) -> Result<(), Mismatch> {
    if got.is_empty() {
        return Err(Mismatch::Unnamed);
    }
    let mut expected =
        expected.split('|').map(|name| name.trim().strip_prefix(PREFIX).unwrap_or(name)).peekable();
    if expected.peek().is_some() && expected.any(|name| got.contains(&name)) {
        Ok(())
    } else {
        Err(Mismatch::Wrong { got })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;

    fn tx_error(error: InvalidTransaction) -> EVMError<Infallible, OpTransactionError> {
        EVMError::Transaction(OpTransactionError::Base(error))
    }

    #[test]
    fn test_check_accepts_a_named_exception() {
        let error = tx_error(InvalidTransaction::RejectCallerWithCode);
        assert_eq!(check(Fork::Osaka, "TransactionException.SENDER_NOT_EOA", &error), Ok(()));
    }

    #[test]
    fn test_check_accepts_any_alternative() {
        let error = tx_error(InvalidTransaction::LackOfFundForMaxFee {
            fee: Box::default(),
            balance: Box::default(),
        });
        assert_eq!(
            check(Fork::Osaka, "TransactionException.INTRINSIC_GAS_TOO_LOW|TransactionException.INSUFFICIENT_ACCOUNT_FUNDS",
                &error
            ),
            Ok(())
        );
    }

    #[test]
    fn test_check_rejects_the_wrong_reason() {
        let error = tx_error(InvalidTransaction::NonceTooLow { tx: 0, state: 1 });
        assert_eq!(
            check(Fork::Osaka, "TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Wrong { got: &["NONCE_MISMATCH_TOO_LOW"] })
        );
    }

    #[test]
    fn test_check_rejects_an_unnamed_error() {
        let error = tx_error(InvalidTransaction::Eip7873NotSupported);
        assert_eq!(
            check(Fork::Osaka, "TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Unnamed)
        );
        let error: EVMError<Infallible, OpTransactionError> = EVMError::Custom("boom".into());
        assert_eq!(
            check(Fork::Osaka, "TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Unnamed)
        );
        let error = EVMError::Transaction(OpTransactionError::MissingEnvelopedTx);
        assert_eq!(
            check::<Infallible>(Fork::Osaka, "TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Unnamed)
        );
    }

    /// On the Amsterdam fixtures a gas limit below the calldata floor satisfies either name of the
    /// one rule, and on the Osaka fixtures only its own; one below the intrinsic cost satisfies
    /// only its own on both.
    #[test]
    fn test_floor_and_intrinsic_shortfalls() {
        const FLOOR: &str = "TransactionException.INTRINSIC_GAS_BELOW_FLOOR_GAS_COST";
        const TOO_LOW: &str = "TransactionException.INTRINSIC_GAS_TOO_LOW";
        let floor =
            tx_error(InvalidTransaction::GasFloorMoreThanGasLimit { gas_floor: 2, gas_limit: 1 });
        let intrinsic = tx_error(InvalidTransaction::CallGasCostMoreThanGasLimit {
            initial_gas: 2,
            gas_limit: 1,
        });
        for fork in Fork::ALL {
            assert_eq!(check(fork, FLOOR, &floor), Ok(()), "{fork}");
            assert_eq!(check(fork, TOO_LOW, &intrinsic), Ok(()), "{fork}");
            assert_eq!(
                check(fork, FLOOR, &intrinsic),
                Err(Mismatch::Wrong { got: &["INTRINSIC_GAS_TOO_LOW"] }),
                "{fork}"
            );
        }
        assert_eq!(check(Fork::Amsterdam, TOO_LOW, &floor), Ok(()));
        assert_eq!(
            check(Fork::Osaka, TOO_LOW, &floor),
            Err(Mismatch::Wrong { got: &["INTRINSIC_GAS_BELOW_FLOOR_GAS_COST"] })
        );
    }

    /// A name without the prefix is not a different name; an empty expectation matches nothing.
    #[test]
    fn test_check_reads_names_with_or_without_the_prefix() {
        let error = tx_error(InvalidTransaction::EmptyBlobs);
        assert_eq!(check(Fork::Osaka, "TYPE_3_TX_ZERO_BLOBS", &error), Ok(()));
        assert!(check(Fork::Osaka, "", &error).is_err());
    }

    fn unit(transaction: serde_json::Value) -> TestUnit {
        let mut tx = serde_json::json!({
            "data": ["0x"], "gasLimit": ["0x5208"], "nonce": "0x00", "value": ["0x00"],
        });
        for (key, value) in transaction.as_object().unwrap() {
            tx[key] = value.clone();
        }
        serde_json::from_value(serde_json::json!({
            "env": {
                "currentCoinbase": "0x0000000000000000000000000000000000000000",
                "currentGasLimit": "0x01", "currentNumber": "0x01", "currentTimestamp": "0x01",
            },
            "pre": {}, "post": {}, "transaction": tx,
        }))
        .unwrap()
    }

    /// A transaction that cannot be built is invalid for the reason its shape says, and a fixture
    /// that expects another reason does not get a skip.
    #[test]
    fn test_unbuildable_transactions_are_matched_by_name() {
        let missing_key = TestError::UnexpectedException {
            expected_exception: None,
            got_exception: Some("Missing secret key".into()),
        };
        let plain = unit(serde_json::json!({}));
        assert_eq!(
            check_unbuildable("TransactionException.INVALID_SIGNATURE_VRS", &missing_key, &plain),
            Ok(())
        );
        assert_eq!(
            check_unbuildable("TransactionException.INTRINSIC_GAS_TOO_LOW", &missing_key, &plain),
            Err(Mismatch::Wrong { got: &["INVALID_SIGNATURE_VRS"] })
        );
        let bad_key = TestError::UnknownPrivateKey(Default::default());
        assert_eq!(
            check_unbuildable("TransactionException.INVALID_SIGNATURE_VRS", &bad_key, &plain),
            Ok(())
        );

        let blob = unit(serde_json::json!({ "maxFeePerBlobGas": "0x01" }));
        let auth = unit(serde_json::json!({ "authorizationList": [] }));
        for error in [
            TestError::InvalidTransactionType,
            TestError::UnexpectedException {
                expected_exception: None,
                got_exception: Some("Invalid transaction type".into()),
            },
        ] {
            assert_eq!(
                check_unbuildable(
                    "TransactionException.TYPE_3_TX_CONTRACT_CREATION",
                    &error,
                    &blob
                ),
                Ok(())
            );
            assert!(check_unbuildable(
                "TransactionException.TYPE_4_TX_CONTRACT_CREATION",
                &error,
                &blob
            )
            .is_err());
            assert_eq!(
                check_unbuildable(
                    "TransactionException.TYPE_4_TX_CONTRACT_CREATION",
                    &error,
                    &auth
                ),
                Ok(())
            );
            assert_eq!(
                check_unbuildable(
                    "TransactionException.TYPE_3_TX_CONTRACT_CREATION",
                    &error,
                    &plain
                ),
                Err(Mismatch::Unnamed)
            );
        }
        let overflow = TestError::UnexpectedException {
            expected_exception: None,
            got_exception: Some("Nonce overflow".into()),
        };
        assert_eq!(
            check_unbuildable("TransactionException.NONCE_IS_MAX", &overflow, &plain),
            Err(Mismatch::Unnamed)
        );
    }
}
