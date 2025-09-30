use revm::primitives::{Bytes, HashMap, StorageKey, StorageValue, U256};
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
    /// Account storage (key-value pairs)
    pub storage: HashMap<StorageKey, StorageValue>,
}
