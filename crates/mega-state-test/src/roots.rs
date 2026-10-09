//! The roots a test's expectation is written in: the post-state root and the hash of the logs a
//! state test carries, computed the way the reference runner computes them, and the receipts root
//! and logs bloom a block header carries.

use alloy_trie::{root::storage_root_unhashed, HashBuilder, Nibbles, TrieAccount};
use mega_evm::{
    alloy_consensus::{proofs::calculate_receipt_root, TxReceipt},
    alloy_primitives::Bloom,
    op_alloy_consensus::OpReceiptEnvelope,
    revm::{
        database::PlainAccount,
        primitives::{keccak256, Address, Log, B256},
    },
};

/// The keccak hash of the RLP list of `logs`: a state test's `logs` field.
pub fn logs_hash(logs: &[Log]) -> B256 {
    let mut out = Vec::with_capacity(alloy_rlp::list_length(logs));
    alloy_rlp::encode_list(logs, &mut out);
    keccak256(&out)
}

/// The receipts root over `receipts`: a block header's `receiptsRoot`.
///
/// Each receipt is a leaf at the RLP of its index, encoded as EIP-2718 has it with its bloom. A
/// receipt of an OP transaction that is not a deposit is an Ethereum receipt, and encodes as one.
pub fn receipts_root(receipts: &[OpReceiptEnvelope]) -> B256 {
    calculate_receipt_root(receipts)
}

/// The bloom of every log of `receipts`: a block header's `logsBloom`.
pub fn logs_bloom(receipts: &[OpReceiptEnvelope]) -> Bloom {
    let mut bloom = Bloom::ZERO;
    for receipt in receipts {
        bloom.accrue_bloom(&receipt.bloom());
    }
    bloom
}

/// The state root over `accounts`: a state test's `hash` field, and a block header's
/// `stateRoot`.
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
    use mega_evm::{
        alloy_consensus::{proofs::calculate_receipt_root, Receipt, ReceiptEnvelope},
        revm::{
            primitives::{b256, Bytes, KECCAK_EMPTY},
            state::AccountInfo,
        },
    };

    /// No receipts have the empty trie's root and an empty bloom.
    #[test]
    fn test_no_receipts() {
        assert_eq!(receipts_root(&[]), alloy_trie::EMPTY_ROOT_HASH);
        assert_eq!(logs_bloom(&[]), Bloom::ZERO);
    }

    /// A receipt of an OP transaction that is not a deposit has the root the same Ethereum
    /// receipt has, whatever its type, and the block's bloom holds every receipt's logs.
    #[test]
    fn test_op_receipts_root_and_bloom_as_ethereum_s() {
        let log = |byte: u8| {
            Log::new_unchecked(
                Address::repeat_byte(byte),
                vec![B256::repeat_byte(byte)],
                Bytes::from(vec![byte]),
            )
        };
        let receipt = |status: bool, gas: u64, byte: u8| {
            Receipt { status: status.into(), cumulative_gas_used: gas, logs: vec![log(byte)] }
                .with_bloom()
        };
        let op = [
            OpReceiptEnvelope::Legacy(receipt(true, 21_000, 1)),
            OpReceiptEnvelope::Eip2930(receipt(false, 42_000, 2)),
            OpReceiptEnvelope::Eip1559(receipt(true, 63_000, 3)),
            OpReceiptEnvelope::Eip7702(receipt(true, 84_000, 4)),
        ];
        let ethereum = [
            ReceiptEnvelope::Legacy(receipt(true, 21_000, 1)),
            ReceiptEnvelope::Eip2930(receipt(false, 42_000, 2)),
            ReceiptEnvelope::Eip1559(receipt(true, 63_000, 3)),
            ReceiptEnvelope::Eip7702(receipt(true, 84_000, 4)),
        ];
        assert_eq!(receipts_root(&op), calculate_receipt_root(&ethereum));
        assert_ne!(receipts_root(&op), receipts_root(&op[..3]));

        let bloom = logs_bloom(&op);
        let mut expected = Bloom::ZERO;
        expected.accrue_logs(&[log(1), log(2), log(3), log(4)]);
        assert_eq!(bloom, expected);
        assert_ne!(bloom, logs_bloom(&op[..3]));
    }

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
