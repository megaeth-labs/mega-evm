use std::collections::BTreeMap;

use mega_evm::revm::primitives::{Bytes, StorageKey, StorageValue, U256};
use serde::{Deserialize, Serialize};

/// Account information
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountInfo {
    /// Account balance in wei
    pub balance: U256,
    /// Account bytecode
    pub code: Bytes,
    /// Account nonce (transaction count)
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Account storage (key-value pairs), ordered by slot so serialized fixtures
    /// are byte-reproducible.
    pub storage: BTreeMap<StorageKey, StorageValue>,
}
