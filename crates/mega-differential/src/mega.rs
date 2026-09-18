//! The left arm: `MegaEvm`, configured as the Satin spec fixes it.

use std::collections::BTreeMap;

use mega_evm::{
    alloy_op_evm::OpTxError,
    op_revm::{OpHaltReason, OpTransactionError},
    revm::{
        context::result::{EVMError, ExecutionResult, ResultGas},
        state::EvmState,
    },
    test_utils::{Scenario, ScenarioTxOutcome},
};

use crate::record::{AccountRecord, LogRecord, ScenarioRecord, TxRecord};

/// Runs `scenario` on `MegaEvm` and records every transaction.
pub fn run(scenario: &Scenario) -> ScenarioRecord {
    let (outcomes, _db) = scenario.run(scenario.database());
    outcomes.iter().map(record).collect()
}

fn record(outcome: &ScenarioTxOutcome) -> TxRecord {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            return TxRecord { outcome: format!("error:{}", error(err)), ..Default::default() }
        }
    };
    let result = &outcome.result;
    TxRecord {
        outcome: match result {
            ExecutionResult::Success { .. } => "success".into(),
            ExecutionResult::Revert { .. } => "revert".into(),
            ExecutionResult::Halt { reason, .. } => format!("halt:{}", halt(reason)),
        },
        gas: gas(result.gas()),
        output: result.output().cloned().unwrap_or_default(),
        created: result.created_address(),
        logs: result
            .logs()
            .iter()
            .map(|log| LogRecord {
                address: log.address,
                topics: log.topics().to_vec(),
                data: log.data.data.clone(),
            })
            .collect(),
        state: state(&outcome.state),
    }
}

/// The gas figures of `gas`: its fields as it serializes them, which is every field it has,
/// and the derived values.
fn gas(gas: &ResultGas) -> BTreeMap<String, u64> {
    let mut figures = crate::serialized_u64_fields(gas);
    figures.insert("tx_gas_used".into(), gas.tx_gas_used());
    figures.insert("block_regular_gas_used".into(), gas.block_regular_gas_used());
    figures.insert("block_state_gas_used".into(), gas.block_state_gas_used());
    figures.insert("final_refunded".into(), gas.final_refunded());
    figures
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

fn state(state: &EvmState) -> BTreeMap<mega_evm::alloy_primitives::Address, AccountRecord> {
    state
        .iter()
        .filter(|(_, account)| account.is_touched())
        .map(|(address, account)| {
            let record = AccountRecord {
                created: account.is_created(),
                selfdestructed: account.is_selfdestructed(),
                balance: account.info.balance,
                nonce: account.info.nonce,
                code_hash: account.info.code_hash,
                storage: account
                    .storage
                    .iter()
                    .filter(|(_, slot)| slot.is_changed())
                    .map(|(key, slot)| (*key, slot.present_value))
                    .collect(),
            };
            (*address, record)
        })
        .collect()
}
