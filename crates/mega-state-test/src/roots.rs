//! The two roots a state test's expectation is written in: the post-state root and the hash of
//! the logs, computed the way the reference runner computes them.

use alloy_trie::{root::storage_root_unhashed, HashBuilder, Nibbles, TrieAccount};
use mega_evm::revm::{
    database::PlainAccount,
    primitives::{keccak256, Address, Log, B256},
};

/// The keccak hash of the RLP list of `logs`: a state test's `logs` field.
pub fn logs_hash(logs: &[Log]) -> B256 {
    let mut out = Vec::with_capacity(alloy_rlp::list_length(logs));
    alloy_rlp::encode_list(logs, &mut out);
    keccak256(&out)
}

/// The state root over `accounts`: a state test's `hash` field.
///
/// A zero slot is not a leaf of an account's storage trie, which is how an account's storage
/// root forgets a slot the transaction cleared.
pub fn state_root<'a>(accounts: impl IntoIterator<Item = (Address, &'a PlainAccount)>) -> B256 {
    let mut leaves: Vec<_> = accounts
        .into_iter()
        .map(|(address, account)| {
            let storage_root = storage_root_unhashed(
                account
                    .storage
                    .iter()
                    .filter(|(_, value)| !value.is_zero())
                    .map(|(key, value)| (B256::from(*key), *value)),
            );
            let leaf = TrieAccount {
                nonce: account.info.nonce,
                balance: account.info.balance,
                storage_root,
                code_hash: account.info.code_hash,
            };
            (keccak256(address), leaf)
        })
        .collect();
    leaves.sort_unstable_by_key(|(key, _)| *key);

    let mut builder = HashBuilder::default();
    let mut rlp = Vec::new();
    for (key, leaf) in leaves {
        rlp.clear();
        alloy_rlp::Encodable::encode(&leaf, &mut rlp);
        builder.add_leaf(Nibbles::unpack(key), &rlp);
    }
    builder.root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::revm::{
        primitives::{b256, KECCAK_EMPTY},
        state::AccountInfo,
    };

    /// No logs hash to the keccak of an empty RLP list, the value every fixture without logs
    /// carries.
    #[test]
    fn test_logs_hash_of_no_logs() {
        assert_eq!(
            logs_hash(&[]),
            b256!("0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347")
        );
    }

    /// An empty state has the empty trie's root.
    #[test]
    fn test_state_root_of_nothing() {
        assert_eq!(state_root([]), alloy_trie::EMPTY_ROOT_HASH);
    }

    /// A slot holding zero is not a leaf: an account whose only slot is zero has the root of an
    /// account without storage.
    #[test]
    fn test_state_root_drops_zero_slots() {
        let info = AccountInfo {
            balance: mega_evm::revm::primitives::U256::from(1),
            code_hash: KECCAK_EMPTY,
            ..Default::default()
        };
        let bare = PlainAccount { info: info.clone(), storage: Default::default() };
        let mut zeroed = PlainAccount { info, storage: Default::default() };
        zeroed.storage.insert(
            mega_evm::revm::primitives::U256::from(1),
            mega_evm::revm::primitives::U256::ZERO,
        );
        let address = Address::repeat_byte(1);
        assert_eq!(state_root([(address, &bare)]), state_root([(address, &zeroed)]));
        assert_ne!(state_root([(address, &bare)]), state_root([]));
    }
}
