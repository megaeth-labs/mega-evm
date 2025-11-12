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

/// A wrapper that allows attaching an estimated data availability size.
#[derive(
    Debug, Clone, derive_more::Deref, derive_more::DerefMut, derive_more::AsRef, derive_more::AsMut,
)]
pub struct WithExtraTxInfo<T> {
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

impl<T> WithExtraTxInfo<T> {
    /// Create a new `WithDASize` wrapper with a known data availability size.
    pub fn new(inner: T, tx_hash: TxHash, da_size: u64, tx_size: u64) -> Self {
        Self { inner, tx_hash, da_size, tx_size }
    }
}

impl<T: Encodable2718> WithExtraTxInfo<T> {
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

impl<T> MegaTransactionExt for WithExtraTxInfo<T> {
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

impl<T: Typed2718> Typed2718 for WithExtraTxInfo<T> {
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

impl<T: Transaction> Transaction for WithExtraTxInfo<T> {
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

impl<Tx, T: RecoveredTx<Tx>> RecoveredTx<Tx> for WithExtraTxInfo<T> {
    delegate! {
        to self.inner {
            fn tx(&self) -> &Tx;
            fn signer(&self) -> &Address;
        }
    }
}

impl<Tx, T: IntoTxEnv<Tx>> IntoTxEnv<Tx> for WithExtraTxInfo<T> {
    delegate! {
        to self.inner {
            fn into_tx_env(self) -> Tx;
        }
    }
}

impl<T: Copy> Copy for WithExtraTxInfo<T> {}
