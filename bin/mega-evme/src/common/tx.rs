//! Transaction configuration for mega-evme

use alloy_primitives::{Address, Bytes, U256};
use clap::Args;
use mega_evm::{
    revm::{context::tx::TxEnv, primitives::TxKind},
    MegaTransaction, TxType,
};

use super::{load_hex, Result};

/// Transaction configuration arguments
#[derive(Args, Debug, Clone)]
pub struct TxArgs {
    /// Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, etc.)
    #[arg(long = "tx-type", default_value = "0")]
    pub tx_type: u8,

    /// Gas limit for the evm
    #[arg(long = "gas", default_value = "10000000")]
    pub gas: u64,

    /// Price set for the evm (gas price)
    #[arg(long = "basefee", visible_aliases = ["gas-price"], default_value = "0")]
    pub basefee: u64,

    /// Gas priority fee (EIP-1559)
    #[arg(long = "priorityfee")]
    pub priority_fee: Option<u64>,

    /// The transaction origin
    #[arg(long = "sender", visible_aliases = ["from"], default_value = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")]
    pub sender: Address,

    /// The transaction receiver (execution context)
    #[arg(long = "receiver", visible_aliases = ["to"], default_value = "0x0000000000000000000000000000000000000000")]
    pub receiver: Address,

    /// The transaction nonce
    #[arg(long = "nonce")]
    pub nonce: Option<u64>,

    /// Indicates the action should be create rather than call
    #[arg(long = "create")]
    pub create: bool,

    /// Value set for the evm
    #[arg(long = "value", default_value = "0")]
    pub value: U256,

    /// Transaction data (input) as hex string
    #[arg(long = "input")]
    pub input: Option<String>,

    /// File containing transaction data (input). If '-' is specified, input is read from stdin
    #[arg(long = "inputfile")]
    pub inputfile: Option<String>,
}

impl TxArgs {
    /// Converts the transaction type to a [`TxType`].
    pub fn tx_type(&self) -> TxType {
        match self.tx_type {
            0 => TxType::Legacy,
            1 => TxType::Eip2930,
            2 => TxType::Eip1559,
            4 => TxType::Eip7702,
            _ => panic!("Unsupported transaction type: {}", self.tx_type),
        }
    }

    /// Calculates the effective gas price for the transaction.
    pub fn effective_gas_price(&self) -> u128 {
        match self.tx_type() {
            TxType::Legacy => self.basefee as u128,
            TxType::Eip2930 => self.basefee as u128,
            TxType::Eip1559 => self.basefee as u128 + self.priority_fee.unwrap_or(0) as u128,
            TxType::Eip7702 => self.basefee as u128 + self.priority_fee.unwrap_or(0) as u128,
            TxType::Deposit => 0,
        }
    }

    /// Creates a [`TxEnv`] from the transaction arguments.
    ///
    /// Loads input data from `--input` or `--inputfile` arguments.
    pub fn create_tx_env(&self, chain_id: u64) -> Result<TxEnv> {
        let data = load_hex(self.input.clone(), self.inputfile.clone())?.unwrap_or_default();
        let kind = if self.create { TxKind::Create } else { TxKind::Call(self.receiver) };

        Ok(TxEnv {
            caller: self.sender,
            gas_price: self.basefee as u128,
            gas_priority_fee: self.priority_fee.map(|pf| pf as u128),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: self.tx_type,
            gas_limit: self.gas,
            data,
            nonce: self.nonce.unwrap_or(0),
            value: self.value,
            access_list: Default::default(),
            authorization_list: Vec::new(),
            kind,
            chain_id: Some(chain_id),
        })
    }

    /// Creates a [`MegaTransaction`] from the transaction arguments.
    ///
    /// Loads input data from `--input` or `--inputfile` arguments.
    pub fn create_tx(&self, chain_id: u64) -> Result<MegaTransaction> {
        let tx_env = self.create_tx_env(chain_id)?;
        let mut tx = MegaTransaction::new(tx_env);
        tx.enveloped_tx = Some(Bytes::default());
        Ok(tx)
    }
}
