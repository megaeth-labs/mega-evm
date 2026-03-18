use alloy_consensus::{transaction::Recovered, Transaction};
use alloy_eips::{eip2930::AccessList, eip7702::SignedAuthorization, Encodable2718, Typed2718};
use alloy_evm::{IntoTxEnv, RecoveredTx};
use alloy_primitives::{Address, Bytes, ChainId, Selector, TxHash, TxKind, B256, U256};
use delegate::delegate;

use crate::MegaTxEnvelope;

/// Helper trait that allows attaching extra information to a transaction.
pub trait MegaTransactionExt {
    /// Get the estimated data availability size of the transaction.
    ///
    /// Note: the default implementation is not efficient since it does not cache the `da_size` and
    /// always recalculates it.
    fn estimated_da_size(&self) -> u64
    where
        Self: Encodable2718,
    {
        op_alloy_flz::tx_estimated_size_fjord_bytes(self.encoded_2718().as_slice())
    }

    /// Get the EIP-2718 encoded size of the transaction in bytes.
    fn tx_size(&self) -> u64
    where
        Self: Encodable2718,
    {
        self.encode_2718_len() as u64
    }

    /// Get the transaction hash.
    fn tx_hash(&self) -> TxHash;
}

impl MegaTransactionExt for Recovered<MegaTxEnvelope> {
    fn tx_hash(&self) -> TxHash {
        self.inner().tx_hash()
    }
}

impl MegaTransactionExt for MegaTxEnvelope {
    fn tx_hash(&self) -> TxHash {
        self.tx_hash()
    }
}

/// A wrapper that allows attaching additional information to a transaction.
#[derive(
    Debug, Clone, derive_more::Deref, derive_more::DerefMut, derive_more::AsRef, derive_more::AsMut,
)]
pub struct EnrichedMegaTx<T> {
    #[deref]
    #[deref_mut]
    #[as_ref]
    #[as_mut]
    inner: T,

    /// The transaction hash.
    pub tx_hash: TxHash,

    /// The estimated data availability size of the transaction.
    pub da_size: u64,

    /// The EIP-2718 encoded size of the transaction in bytes.
    pub tx_size: u64,
}

impl<T> EnrichedMegaTx<T> {
    /// Create a new `WithDASize` wrapper with a known data availability size.
    pub fn new(inner: T, tx_hash: TxHash, da_size: u64, tx_size: u64) -> Self {
        Self { inner, tx_hash, da_size, tx_size }
    }
}

impl<T: Encodable2718> EnrichedMegaTx<T> {
    /// Create a new `WithDASize` wrapper and do the computation to estimate the data availability
    /// size.
    pub fn new_slow(inner: T) -> Self {
        Self {
            tx_hash: inner.trie_hash(),
            da_size: op_alloy_flz::tx_estimated_size_fjord_bytes(inner.encoded_2718().as_slice()),
            tx_size: inner.encode_2718_len() as u64,
            inner,
        }
    }
}

impl<T> MegaTransactionExt for EnrichedMegaTx<T> {
    fn estimated_da_size(&self) -> u64 {
        self.da_size
    }

    fn tx_size(&self) -> u64 {
        self.tx_size
    }

    fn tx_hash(&self) -> TxHash {
        self.tx_hash
    }
}

impl<T: Typed2718> Typed2718 for EnrichedMegaTx<T> {
    delegate! {
        to self.inner {
            fn ty(&self) -> u8;
            fn is_type(&self, ty: u8) -> bool;
            fn is_legacy(&self) -> bool;
            fn is_eip2930(&self) -> bool;
            fn is_eip1559(&self) -> bool;
            fn is_eip4844(&self) -> bool;
            fn is_eip7702(&self) -> bool;
        }
    }
}

impl<T: Transaction> Transaction for EnrichedMegaTx<T> {
    delegate! {
        to self.inner {
            fn chain_id(&self) -> Option<ChainId>;
            fn nonce(&self) -> u64;
            fn gas_limit(&self) -> u64;
            fn gas_price(&self) -> Option<u128>;
            fn max_fee_per_gas(&self) -> u128;
            fn max_priority_fee_per_gas(&self) -> Option<u128>;
            fn max_fee_per_blob_gas(&self) -> Option<u128>;
            fn priority_fee_or_price(&self) -> u128;
            fn effective_gas_price(&self, base_fee: Option<u64>) -> u128;
            fn is_dynamic_fee(&self) -> bool;
            fn kind(&self) -> TxKind;
            fn is_create(&self) -> bool;
            fn value(&self) -> U256;
            fn input(&self) -> &Bytes;
            fn access_list(&self) -> Option<&AccessList>;
            fn blob_versioned_hashes(&self) -> Option<&[B256]>;
            fn authorization_list(&self) -> Option<&[SignedAuthorization]>;
            fn authorization_count(&self) -> Option<u64>;
            fn effective_tip_per_gas(&self, base_fee: u64) -> Option<u128>;
            fn to(&self) -> Option<Address>;
            fn function_selector(&self) -> Option<&Selector>;
            fn blob_count(&self) -> Option<u64>;
            fn blob_gas_used(&self) -> Option<u64>;
        }
    }
}

impl<Tx, T: RecoveredTx<Tx>> RecoveredTx<Tx> for EnrichedMegaTx<T> {
    delegate! {
        to self.inner {
            fn tx(&self) -> &Tx;
            fn signer(&self) -> &Address;
        }
    }
}

impl<Tx, T: IntoTxEnv<Tx>> IntoTxEnv<Tx> for EnrichedMegaTx<T> {
    delegate! {
        to self.inner {
            fn into_tx_env(self) -> Tx;
        }
    }
}

impl<T: Copy> Copy for EnrichedMegaTx<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
    use alloy_primitives::{address, bytes::BufMut, Signature};
    use revm::context::TxEnv;

    const CALLER: Address = address!("2000000000000000000000000000000000000001");
    const CONTRACT: Address = address!("3000000000000000000000000000000000000001");

    #[derive(Debug, Clone)]
    struct MockTx {
        nonce: u64,
        gas_limit: u64,
        value: U256,
        input: Bytes,
    }

    impl Typed2718 for MockTx {
        fn ty(&self) -> u8 {
            0
        }
    }

    impl Encodable2718 for MockTx {
        fn encode_2718_len(&self) -> usize {
            self.input.len()
        }

        fn encode_2718(&self, out: &mut dyn BufMut) {
            out.put_slice(&self.input);
        }
    }

    impl Transaction for MockTx {
        fn chain_id(&self) -> Option<ChainId> {
            Some(1)
        }

        fn nonce(&self) -> u64 {
            self.nonce
        }

        fn gas_limit(&self) -> u64 {
            self.gas_limit
        }

        fn gas_price(&self) -> Option<u128> {
            Some(9)
        }

        fn max_fee_per_gas(&self) -> u128 {
            9
        }

        fn max_priority_fee_per_gas(&self) -> Option<u128> {
            None
        }

        fn max_fee_per_blob_gas(&self) -> Option<u128> {
            None
        }

        fn priority_fee_or_price(&self) -> u128 {
            9
        }

        fn effective_gas_price(&self, _base_fee: Option<u64>) -> u128 {
            9
        }

        fn is_dynamic_fee(&self) -> bool {
            false
        }

        fn kind(&self) -> TxKind {
            TxKind::Call(CONTRACT)
        }

        fn is_create(&self) -> bool {
            false
        }

        fn value(&self) -> U256 {
            self.value
        }

        fn input(&self) -> &Bytes {
            &self.input
        }

        fn access_list(&self) -> Option<&AccessList> {
            None
        }

        fn blob_versioned_hashes(&self) -> Option<&[B256]> {
            None
        }

        fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
            None
        }
    }

    #[derive(Debug, Clone)]
    struct MockRecoveredTx {
        tx: TxEnv,
        signer: Address,
    }

    impl RecoveredTx<TxEnv> for MockRecoveredTx {
        fn tx(&self) -> &TxEnv {
            &self.tx
        }

        fn signer(&self) -> &Address {
            &self.signer
        }
    }

    impl IntoTxEnv<TxEnv> for MockRecoveredTx {
        fn into_tx_env(self) -> TxEnv {
            self.tx
        }
    }

    fn legacy_tx() -> TxLegacy {
        TxLegacy {
            chain_id: Some(1),
            nonce: 7,
            gas_price: 9,
            gas_limit: 21_000,
            to: TxKind::Call(CONTRACT),
            value: U256::from(11),
            input: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
        }
    }

    fn legacy_envelope() -> MegaTxEnvelope {
        MegaTxEnvelope::Legacy(Signed::new_unchecked(
            legacy_tx(),
            Signature::test_signature(),
            Default::default(),
        ))
    }

    #[test]
    fn mega_transaction_ext_works_for_envelope_and_recovered_types() {
        let tx = legacy_envelope();
        let recovered = Recovered::new_unchecked(tx.clone(), CALLER);

        assert_eq!(MegaTransactionExt::tx_hash(&tx), tx.tx_hash());
        assert_eq!(MegaTransactionExt::tx_hash(&recovered), tx.tx_hash());
        assert!(MegaTransactionExt::estimated_da_size(&tx) > 0);
        assert!(MegaTransactionExt::tx_size(&tx) > 0);
    }

    #[test]
    fn enriched_mega_tx_new_slow_computes_hash_and_sizes() {
        let tx = MockTx {
            nonce: 7,
            gas_limit: 21_000,
            value: U256::from(11),
            input: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
        };
        let expected_hash = tx.trie_hash();
        let expected_da_size =
            op_alloy_flz::tx_estimated_size_fjord_bytes(tx.encoded_2718().as_slice());
        let expected_tx_size = tx.encode_2718_len() as u64;

        let enriched = EnrichedMegaTx::new_slow(tx);

        assert_eq!(MegaTransactionExt::tx_hash(&enriched), expected_hash);
        assert_eq!(enriched.da_size, expected_da_size);
        assert_eq!(enriched.tx_size, expected_tx_size);
        assert_eq!(enriched.nonce(), 7);
        assert_eq!(enriched.gas_limit(), 21_000);
        assert_eq!(enriched.value(), U256::from(11));
        assert_eq!(enriched.kind(), TxKind::Call(CONTRACT));
    }

    #[test]
    fn enriched_mega_tx_delegates_recovered_transaction_methods() {
        let tx_env = TxEnv {
            caller: CALLER,
            gas_limit: 21_000,
            kind: TxKind::Call(CONTRACT),
            value: U256::from(11),
            data: Bytes::from_static(&[0xaa, 0xbb]),
            ..Default::default()
        };
        let recovered = MockRecoveredTx { tx: tx_env.clone(), signer: CALLER };
        let enriched = EnrichedMegaTx::new(recovered, TxHash::ZERO, 1, 2);

        assert_eq!(enriched.tx(), &tx_env);
        assert_eq!(*enriched.signer(), CALLER);

        let converted: TxEnv = enriched.into_tx_env();
        assert_eq!(converted, tx_env);
    }
}
