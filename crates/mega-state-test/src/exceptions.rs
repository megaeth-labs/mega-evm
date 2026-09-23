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
//! the test drew the larger, in the fixture's own accounting. From Amsterdam on that accounting
//! can call `INTRINSIC_GAS_TOO_LOW` a limit revm finds above the intrinsic cost and below the
//! floor, so a floor shortfall satisfies both names; an intrinsic shortfall satisfies only its own.

use mega_evm::{
    op_revm::OpTransactionError,
    revm::context::result::{EVMError, InvalidTransaction},
};

/// The prefix of every transaction exception name.
const PREFIX: &str = "TransactionException.";

/// The execution-spec exception names `error` satisfies, without the `TransactionException.`
/// prefix; empty for an error no fixture exception describes.
pub fn names(error: &InvalidTransaction) -> &'static [&'static str] {
    use InvalidTransaction as E;
    match error {
        E::PriorityFeeGreaterThanMaxFee => &["PRIORITY_GREATER_THAN_MAX_FEE_PER_GAS"],
        E::GasPriceLessThanBasefee => &["INSUFFICIENT_MAX_FEE_PER_GAS"],
        E::CallerGasLimitMoreThanBlock => &["GAS_ALLOWANCE_EXCEEDED"],
        E::CallGasCostMoreThanGasLimit { .. } => &["INTRINSIC_GAS_TOO_LOW"],
        E::GasFloorMoreThanGasLimit { .. } => {
            &["INTRINSIC_GAS_BELOW_FLOOR_GAS_COST", "INTRINSIC_GAS_TOO_LOW"]
        }
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

/// Whether `error` is one of the exceptions `expected` names.
pub fn check<DBError>(
    expected: &str,
    error: &EVMError<DBError, OpTransactionError>,
) -> Result<(), Mismatch> {
    let EVMError::Transaction(OpTransactionError::Base(invalid)) = error else {
        return Err(Mismatch::Unnamed);
    };
    let got = names(invalid);
    if got.is_empty() {
        return Err(Mismatch::Unnamed);
    }
    let expected = expected.split('|').map(|name| name.trim().strip_prefix(PREFIX).unwrap_or(name));
    let mut expected = expected.peekable();
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
        assert_eq!(check("TransactionException.SENDER_NOT_EOA", &error), Ok(()));
    }

    #[test]
    fn test_check_accepts_any_alternative() {
        let error = tx_error(InvalidTransaction::LackOfFundForMaxFee {
            fee: Box::default(),
            balance: Box::default(),
        });
        assert_eq!(
            check(
                "TransactionException.INTRINSIC_GAS_TOO_LOW|TransactionException.INSUFFICIENT_ACCOUNT_FUNDS",
                &error
            ),
            Ok(())
        );
    }

    #[test]
    fn test_check_rejects_the_wrong_reason() {
        let error = tx_error(InvalidTransaction::NonceTooLow { tx: 0, state: 1 });
        assert_eq!(
            check("TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Wrong { got: &["NONCE_MISMATCH_TOO_LOW"] })
        );
    }

    #[test]
    fn test_check_rejects_an_unnamed_error() {
        let error = tx_error(InvalidTransaction::Eip7873NotSupported);
        assert_eq!(
            check("TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Unnamed)
        );
        let error: EVMError<Infallible, OpTransactionError> = EVMError::Custom("boom".into());
        assert_eq!(
            check("TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Unnamed)
        );
        let error = EVMError::Transaction(OpTransactionError::MissingEnvelopedTx);
        assert_eq!(
            check::<Infallible>("TransactionException.INTRINSIC_GAS_TOO_LOW", &error),
            Err(Mismatch::Unnamed)
        );
    }

    /// A gas limit below the calldata floor satisfies either name of the one rule; one below the
    /// intrinsic cost only its own.
    #[test]
    fn test_floor_and_intrinsic_shortfalls() {
        let floor =
            tx_error(InvalidTransaction::GasFloorMoreThanGasLimit { gas_floor: 2, gas_limit: 1 });
        assert_eq!(
            check("TransactionException.INTRINSIC_GAS_BELOW_FLOOR_GAS_COST", &floor),
            Ok(())
        );
        assert_eq!(check("TransactionException.INTRINSIC_GAS_TOO_LOW", &floor), Ok(()));
        let intrinsic = tx_error(InvalidTransaction::CallGasCostMoreThanGasLimit {
            initial_gas: 2,
            gas_limit: 1,
        });
        assert_eq!(check("TransactionException.INTRINSIC_GAS_TOO_LOW", &intrinsic), Ok(()));
        assert!(
            check("TransactionException.INTRINSIC_GAS_BELOW_FLOOR_GAS_COST", &intrinsic).is_err()
        );
    }

    /// A name without the prefix is not a different name; an empty expectation matches nothing.
    #[test]
    fn test_check_reads_names_with_or_without_the_prefix() {
        let error = tx_error(InvalidTransaction::EmptyBlobs);
        assert_eq!(check("TYPE_3_TX_ZERO_BLOBS", &error), Ok(()));
        assert!(check("", &error).is_err());
    }
}
