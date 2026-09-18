//! The left arm: `MegaEvm`, configured as the Satin spec fixes it.

use mega_evm::{
    alloy_op_evm::OpTxError,
    op_revm::{OpHaltReason, OpTransactionError},
    revm::context::result::{EVMError, ExecutionResult},
    test_utils::{Scenario, ScenarioTxOutcome},
};

use crate::record::{ScenarioRecord, TxRecord};

/// Runs `scenario` on `MegaEvm` and records every transaction.
pub fn run(scenario: &Scenario) -> ScenarioRecord {
    let (outcomes, _db) = scenario.run(scenario.database());
    outcomes.iter().map(record).collect()
}

fn record(outcome: &ScenarioTxOutcome) -> TxRecord {
    match outcome {
        // `ResultGas` serializes `reservoir_remaining` on the fork.
        Ok(outcome) => tx_record!(ExecutionResult, &outcome.result, &outcome.state, halt, None),
        Err(err) => TxRecord { outcome: format!("error:{}", error(err)), ..Default::default() },
    }
}

/// The halt reason with the OP wrapper taken off: op-revm wraps every Ethereum halt reason in
/// `OpHaltReason::Base`, which is naming, not behavior. A halt reason of op-revm's own keeps an
/// `op:` prefix.
fn halt(reason: &OpHaltReason) -> String {
    match reason {
        OpHaltReason::Base(reason) => format!("{reason:?}"),
        other => format!("op:{other:?}"),
    }
}

/// The error with the OP wrappers taken off, the same way as [`halt`].
fn error(err: &EVMError<core::convert::Infallible, OpTxError>) -> String {
    match err {
        EVMError::Transaction(OpTxError(OpTransactionError::Base(invalid))) => {
            format!("invalid:{invalid:?}")
        }
        EVMError::Transaction(OpTxError(other)) => format!("op:{other:?}"),
        EVMError::Header(header) => format!("header:{header:?}"),
        other => format!("other:{other:?}"),
    }
}
