//! What an arm reports for a scenario, in a form both arms produce.
//!
//! Each arm turns its own result types into these records; the comparison only ever sees
//! records, so it cannot depend on which revm produced them.

use std::collections::BTreeMap;

use alloy_primitives::{Address, Bytes, B256, U256};

/// Marker for a value one arm has and the other does not.
pub const ABSENT: &str = "<absent>";

/// The records of every transaction of one scenario, in order.
pub type ScenarioRecord = Vec<TxRecord>;

/// What one transaction produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxRecord {
    /// `success`, `revert`, `halt:<reason>` or `error:<reason>`.
    pub outcome: String,
    /// Every gas figure of the result, by name: the fields of `ResultGas` as it serializes
    /// them, the reservoir left over, and the derived values a block builder reads.
    pub gas: BTreeMap<String, u64>,
    /// Return or revert data.
    pub output: Bytes,
    /// Address a create transaction deployed to.
    pub created: Option<Address>,
    /// Logs, in emission order.
    pub logs: Vec<LogRecord>,
    /// Every account the transaction touched.
    pub state: BTreeMap<Address, AccountRecord>,
}

/// A log entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// Emitting contract.
    pub address: Address,
    /// Topics.
    pub topics: Vec<B256>,
    /// Data.
    pub data: Bytes,
}

/// A touched account after the transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountRecord {
    /// Created in this transaction (its storage starts empty).
    pub created: bool,
    /// Self-destructed in this transaction.
    pub selfdestructed: bool,
    /// Balance.
    pub balance: U256,
    /// Nonce.
    pub nonce: u64,
    /// Code hash.
    pub code_hash: B256,
    /// The slots whose value the transaction changed, with their new value.
    pub storage: BTreeMap<U256, U256>,
}

impl LogRecord {
    /// One-line rendering used in difference reports and the registry.
    pub fn render(&self) -> String {
        let topics: Vec<String> = self.topics.iter().map(|topic| format!("{topic}")).collect();
        format!("{:#x} [{}] {}", self.address, topics.join(","), self.data)
    }
}

impl AccountRecord {
    /// One-line rendering used when only one arm has the account.
    pub fn render(&self) -> String {
        let storage: Vec<String> =
            self.storage.iter().map(|(slot, value)| format!("{slot:#x}={value:#x}")).collect();
        format!(
            "created={} selfdestructed={} balance={:#x} nonce={} code_hash={} storage={{{}}}",
            self.created,
            self.selfdestructed,
            self.balance,
            self.nonce,
            self.code_hash,
            storage.join(","),
        )
    }
}

/// Builds the [`TxRecord`] of an executed transaction, for either arm.
///
/// Both revms expose the same accessors on their execution result, logs and state, but as
/// types of different crates, so one function cannot take both; this macro is that function, so
/// the two arms cannot drift apart in what they record.
///
/// - `$exec`: the arm's `ExecutionResult` type, to match its variants;
/// - `$result`, `$state`: the result and the state the transaction produced;
/// - `$halt`: renders the arm's halt reason;
/// - `$reservoir`: the leftover reservoir when the arm's `ResultGas` does not serialize it.
macro_rules! tx_record {
    ($exec:ident, $result:expr, $state:expr, $halt:expr, $reservoir:expr $(,)?) => {{
        let result = $result;
        let gas = result.gas();
        let mut figures = $crate::serialized_u64_fields(gas);
        if let Some(reservoir) = $reservoir {
            figures.insert("reservoir_remaining".to_string(), reservoir);
        }
        figures.insert("tx_gas_used".to_string(), gas.tx_gas_used());
        figures.insert("block_regular_gas_used".to_string(), gas.block_regular_gas_used());
        figures.insert("block_state_gas_used".to_string(), gas.block_state_gas_used());
        figures.insert("final_refunded".to_string(), gas.final_refunded());
        $crate::record::TxRecord {
            outcome: match result {
                $exec::Success { .. } => "success".to_string(),
                $exec::Revert { .. } => "revert".to_string(),
                $exec::Halt { reason, .. } => format!("halt:{}", $halt(reason)),
            },
            gas: figures,
            output: result.output().cloned().unwrap_or_default(),
            created: result.created_address(),
            logs: result
                .logs()
                .iter()
                .map(|log| $crate::record::LogRecord {
                    address: log.address,
                    topics: log.topics().to_vec(),
                    data: log.data.data.clone(),
                })
                .collect(),
            state: $state
                .iter()
                .filter(|(_, account)| account.is_touched())
                .map(|(address, account)| {
                    let record = $crate::record::AccountRecord {
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
                .collect(),
        }
    }};
}
