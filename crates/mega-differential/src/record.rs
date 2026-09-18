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
