//! Transaction type definitions for the `mega-evme` tool.

use alloy_consensus::{Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy};
use alloy_primitives::{Signature, TxKind};
use mega_evm::MegaTxEnvelope;
use revm::{
    context_interface::transaction::{AccessList, SignedAuthorization},
    primitives::{Address, Bytes, B256, U256},
};

/// Error type for transaction conversion failures
#[derive(Debug, thiserror::Error)]
pub enum TransactionConversionError {
    /// Unsupported transaction type
    #[error("Unsupported transaction type: {0}")]
    UnsupportedType(u8),
    /// Missing required field
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
    /// Invalid signature components
    #[error("Invalid signature: {0}")]
    InvalidSignature(String),
    /// EIP-7702 transactions cannot be contract creation transactions
    #[error("EIP-7702 transactions cannot be contract creation transactions")]
    Eip7702CannotBeCreate,
}

/// Transaction data for t8n (individual signed transaction)
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, 3=EIP-4844, 4=EIP-7702)
    #[serde(rename = "type", default, with = "alloy_serde::quantity::opt")]
    pub tx_type: Option<u8>,
    /// Chain ID
    pub chain_id: Option<U256>,
    /// Transaction nonce
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Gas price (legacy/EIP-2930)
    #[serde(with = "alloy_serde::quantity::opt")]
    pub gas_price: Option<u64>,
    /// Maximum fee per gas (EIP-1559)
    #[serde(rename = "gasFeeCap", with = "alloy_serde::quantity::opt", default)]
    pub max_fee_per_gas: Option<u64>,
    /// Maximum priority fee per gas (EIP-1559)
    #[serde(rename = "gasTipCap", with = "alloy_serde::quantity::opt", default)]
    pub max_priority_fee_per_gas: Option<u64>,
    /// Gas limit
    #[serde(default, with = "alloy_serde::quantity")]
    pub gas: u64,
    /// Recipient address (None for contract creation)
    pub to: Option<Address>,
    /// Ether value to transfer
    pub value: U256,
    /// Transaction data/input
    #[serde(default, alias = "input")]
    pub data: Bytes,
    /// Access list (EIP-2930, EIP-1559)
    pub access_list: Option<AccessList>,
    /// Authorization list (EIP-7702)
    pub authorization_list: Option<Vec<SignedAuthorization>>,
    /// Maximum fee per blob gas (EIP-4844)
    pub max_fee_per_blob_gas: Option<U256>,
    /// Blob versioned hashes (EIP-4844)
    #[serde(default)]
    pub blob_versioned_hashes: Vec<B256>,
    /// Signature v component
    pub v: U256,
    /// Signature r component
    pub r: U256,
    /// Signature s component
    pub s: U256,
    /// Secret key (for unsigned transactions)
    pub secret_key: Option<B256>,
}

impl Transaction {
    /// Converts this transaction into a `MegaTxEnvelope`
    pub fn to_envelope(&self) -> Result<MegaTxEnvelope, TransactionConversionError> {
        // Convert v, r, s to Signature
        let signature = self.to_signature()?;

        // Convert to field to TxKind
        let tx_kind = match self.to {
            Some(addr) => TxKind::Call(addr),
            None => TxKind::Create,
        };

        // Determine transaction type (default to 0 for legacy)
        let tx_type = self.tx_type.unwrap_or(0);

        match tx_type {
            // Legacy transaction (type 0)
            0 => {
                let gas_price =
                    self.gas_price.ok_or(TransactionConversionError::MissingField("gas_price"))?
                        as u128;

                let tx = TxLegacy {
                    chain_id: self.chain_id.and_then(|id| id.try_into().ok()),
                    nonce: self.nonce,
                    gas_price,
                    gas_limit: self.gas,
                    to: tx_kind,
                    value: self.value,
                    input: self.data.clone(),
                };

                let signed = Signed::new_unhashed(tx, signature);
                Ok(MegaTxEnvelope::Legacy(signed))
            }

            // EIP-2930 transaction (type 1)
            1 => {
                let chain_id = self
                    .chain_id
                    .and_then(|id| id.try_into().ok())
                    .ok_or(TransactionConversionError::MissingField("chain_id"))?;

                let gas_price =
                    self.gas_price.ok_or(TransactionConversionError::MissingField("gas_price"))?
                        as u128;

                let tx = TxEip2930 {
                    chain_id,
                    nonce: self.nonce,
                    gas_price,
                    gas_limit: self.gas,
                    to: tx_kind,
                    value: self.value,
                    access_list: self.access_list.clone().unwrap_or_default(),
                    input: self.data.clone(),
                };

                let signed = Signed::new_unhashed(tx, signature);
                Ok(MegaTxEnvelope::Eip2930(signed))
            }

            // EIP-1559 transaction (type 2)
            2 => {
                let chain_id = self
                    .chain_id
                    .and_then(|id| id.try_into().ok())
                    .ok_or(TransactionConversionError::MissingField("chain_id"))?;

                let max_fee_per_gas = self
                    .max_fee_per_gas
                    .ok_or(TransactionConversionError::MissingField("max_fee_per_gas"))?
                    as u128;

                let max_priority_fee_per_gas = self
                    .max_priority_fee_per_gas
                    .ok_or(TransactionConversionError::MissingField("max_priority_fee_per_gas"))?
                    as u128;

                let tx = TxEip1559 {
                    chain_id,
                    nonce: self.nonce,
                    gas_limit: self.gas,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    to: tx_kind,
                    value: self.value,
                    access_list: self.access_list.clone().unwrap_or_default(),
                    input: self.data.clone(),
                };

                let signed = Signed::new_unhashed(tx, signature);
                Ok(MegaTxEnvelope::Eip1559(signed))
            }

            // EIP-4844 (blob transactions) - not supported in OpTxEnvelope
            3 => Err(TransactionConversionError::UnsupportedType(3)),

            // EIP-7702 transaction (type 4)
            4 => {
                let chain_id = self
                    .chain_id
                    .and_then(|id| id.try_into().ok())
                    .ok_or(TransactionConversionError::MissingField("chain_id"))?;

                let max_fee_per_gas = self
                    .max_fee_per_gas
                    .ok_or(TransactionConversionError::MissingField("max_fee_per_gas"))?
                    as u128;

                let max_priority_fee_per_gas = self
                    .max_priority_fee_per_gas
                    .ok_or(TransactionConversionError::MissingField("max_priority_fee_per_gas"))?
                    as u128;

                // EIP-7702 transactions must have a target address (cannot be contract creation)
                let to_addr = self.to.ok_or(TransactionConversionError::Eip7702CannotBeCreate)?;

                let authorization_list = self
                    .authorization_list
                    .clone()
                    .ok_or(TransactionConversionError::MissingField("authorization_list"))?;

                let tx = TxEip7702 {
                    chain_id,
                    nonce: self.nonce,
                    gas_limit: self.gas,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    to: to_addr,
                    value: self.value,
                    access_list: self.access_list.clone().unwrap_or_default(),
                    authorization_list,
                    input: self.data.clone(),
                };

                let signed = Signed::new_unhashed(tx, signature);
                Ok(MegaTxEnvelope::Eip7702(signed))
            }

            // Unknown transaction type
            _ => Err(TransactionConversionError::UnsupportedType(tx_type)),
        }
    }

    /// Converts the v, r, s signature components to an alloy Signature
    fn to_signature(&self) -> Result<Signature, TransactionConversionError> {
        // Calculate y_parity from v
        // For EIP-155 transactions: v = chain_id * 2 + 35 + y_parity
        // For pre-EIP-155: v = 27 + y_parity
        let y_parity = if let Some(chain_id) = self.chain_id {
            let chain_id_u64: u64 = chain_id.try_into().map_err(|_| {
                TransactionConversionError::InvalidSignature("chain_id too large".to_string())
            })?;

            let v_u64: u64 = self.v.try_into().map_err(|_| {
                TransactionConversionError::InvalidSignature("v value too large".to_string())
            })?;

            // EIP-155: v = chain_id * 2 + 35 + y_parity
            if v_u64 >= chain_id_u64 * 2 + 35 {
                v_u64 - chain_id_u64 * 2 - 35 == 1
            } else {
                // Fall back to pre-EIP-155 if v doesn't match EIP-155 formula
                v_u64 == 28
            }
        } else {
            // Pre-EIP-155: v = 27 or 28
            let v_u64: u64 = self.v.try_into().map_err(|_| {
                TransactionConversionError::InvalidSignature("v value too large".to_string())
            })?;
            v_u64 == 28
        };

        // Create signature from r, s, and y_parity
        Ok(Signature::new(self.r, self.s, y_parity))
    }
}
