//! Transaction configuration for mega-evme

use alloy_primitives::{Address, Bytes, B256, U256};
use clap::Args;
use mega_evm::{
    alloy_eips::eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    op_revm::transaction::deposit::DepositTransactionParts,
    revm::{context::tx::TxEnv, primitives::TxKind},
    Either, MegaTransaction, MegaTxType,
};

use super::{load_hex, EvmeError, Result};

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
    #[arg(long = "basefee", visible_aliases = ["gas-price", "price"], default_value = "0")]
    pub basefee: u64,

    /// Gas priority fee (EIP-1559)
    #[arg(long = "priority-fee", visible_aliases = ["priorityfee", "tip"])]
    pub priority_fee: Option<u64>,

    /// The transaction origin
    #[arg(long = "sender", visible_aliases = ["from"], default_value = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")]
    pub sender: Address,

    /// The transaction receiver (execution context)
    #[arg(long = "receiver", visible_aliases = ["to"])]
    pub receiver: Option<Address>,

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

    /// Source hash for deposit transactions (tx-type 126)
    #[arg(long = "source-hash", value_name = "HASH")]
    pub source_hash: Option<B256>,

    /// Amount of ETH to mint for deposit transactions (wei)
    #[arg(long = "mint")]
    pub mint: Option<u128>,

    /// EIP-7702 authorization in format `AUTHORITY:NONCE->DELEGATION` (can be repeated)
    #[arg(long = "auth", value_name = "AUTH")]
    pub auth: Vec<String>,
}

impl TxArgs {
    /// Validates transaction arguments for consistency.
    ///
    /// Checks:
    /// - `source_hash` and `mint` are only set for deposit transactions (tx-type 126)
    /// - `priority_fee` is not set for legacy or EIP-2930 transactions
    /// - `receiver` must exist when `create` is false, must not exist when `create` is true
    /// - `auth` is only set for EIP-7702 transactions (tx-type 4)
    pub fn validate(&self) -> Result<()> {
        // 1. source_hash and mint should only be set when tx_type is deposit
        if self.tx_type != 126 && (self.source_hash.is_some() || self.mint.is_some()) {
            return Err(EvmeError::InvalidInput(
                "--source-hash and --mint are only valid for deposit transactions (--tx-type 126)"
                    .to_string(),
            ));
        }
        if self.tx_type == 126 && self.source_hash.is_none() {
            return Err(EvmeError::InvalidInput(
                "--source-hash is required for deposit transactions (--tx-type 126)".to_string(),
            ));
        }

        // 2. priority_fee must not be set when tx_type is legacy or eip2930
        if matches!(self.tx_type, 0 | 1) && self.priority_fee.is_some() {
            return Err(EvmeError::InvalidInput(
                "--priority-fee is not valid for legacy (0) or EIP-2930 (1) transactions"
                    .to_string(),
            ));
        }

        // 3. receiver must exist when create is false, must not exist when create is true
        if self.create && self.receiver.is_some() {
            return Err(EvmeError::InvalidInput(
                "--receiver must not be set when --create is specified".to_string(),
            ));
        }
        if !self.create && self.receiver.is_none() {
            return Err(EvmeError::InvalidInput(
                "--receiver is required when --create is not specified".to_string(),
            ));
        }

        // 4. auth should only be set when tx_type is EIP-7702
        if self.tx_type != 4 && !self.auth.is_empty() {
            return Err(EvmeError::InvalidInput(
                "--auth is only valid for EIP-7702 transactions (--tx-type 4)".to_string(),
            ));
        }

        Ok(())
    }

    /// Parses authorization list from CLI arguments.
    ///
    /// Format: `AUTHORITY:NONCE->DELEGATION`
    /// - AUTHORITY: Address of the EOA delegating control
    /// - NONCE: Authorization nonce (decimal or 0x-prefixed hex)
    /// - DELEGATION: Address of the contract to delegate to
    fn parse_authorization_list(&self, chain_id: u64) -> Result<Vec<RecoveredAuthorization>> {
        self.auth
            .iter()
            .map(|s| Self::parse_authorization(s, chain_id))
            .collect()
    }

    /// Parses a single authorization string.
    fn parse_authorization(s: &str, chain_id: u64) -> Result<RecoveredAuthorization> {
        // Split by "->" to get authority:nonce and delegation
        let parts: Vec<&str> = s.split("->").collect();
        if parts.len() != 2 {
            return Err(EvmeError::InvalidInput(format!(
                "Invalid authorization format '{}'. Expected: AUTHORITY:NONCE->DELEGATION",
                s
            )));
        }

        let delegation: Address = parts[1].trim().parse().map_err(|_| {
            EvmeError::InvalidInput(format!("Invalid delegation address: {}", parts[1].trim()))
        })?;

        // Split authority:nonce
        let auth_parts: Vec<&str> = parts[0].split(':').collect();
        if auth_parts.len() != 2 {
            return Err(EvmeError::InvalidInput(format!(
                "Invalid authorization format '{}'. Expected: AUTHORITY:NONCE->DELEGATION",
                s
            )));
        }

        let authority: Address = auth_parts[0].trim().parse().map_err(|_| {
            EvmeError::InvalidInput(format!("Invalid authority address: {}", auth_parts[0].trim()))
        })?;

        let nonce: u64 = if auth_parts[1].trim().starts_with("0x") {
            u64::from_str_radix(auth_parts[1].trim().trim_start_matches("0x"), 16).map_err(|_| {
                EvmeError::InvalidInput(format!("Invalid nonce: {}", auth_parts[1].trim()))
            })?
        } else {
            auth_parts[1].trim().parse().map_err(|_| {
                EvmeError::InvalidInput(format!("Invalid nonce: {}", auth_parts[1].trim()))
            })?
        };

        Ok(RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::from(chain_id), address: delegation, nonce },
            RecoveredAuthority::Valid(authority),
        ))
    }

    /// Returns the receiver address.
    pub fn receiver(&self) -> Result<Address> {
        self.receiver.ok_or(EvmeError::MissingReceiver)
    }

    /// Converts the transaction type to a [`MegaTxType`].
    pub fn tx_type(&self) -> Result<MegaTxType> {
        match self.tx_type {
            0 => Ok(MegaTxType::Legacy),
            1 => Ok(MegaTxType::Eip2930),
            2 => Ok(MegaTxType::Eip1559),
            4 => Ok(MegaTxType::Eip7702),
            126 => Ok(MegaTxType::Deposit),
            _ => Err(EvmeError::UnsupportedTxType(self.tx_type)),
        }
    }

    /// Calculates the effective gas price for the transaction.
    pub fn effective_gas_price(&self) -> Result<u128> {
        Ok(match self.tx_type()? {
            MegaTxType::Legacy | MegaTxType::Eip2930 => self.basefee as u128,
            MegaTxType::Eip1559 | MegaTxType::Eip7702 => {
                self.basefee as u128 + self.priority_fee.unwrap_or(0) as u128
            }
            MegaTxType::Deposit => 0,
        })
    }

    /// Creates a [`TxEnv`] from the transaction arguments.
    ///
    /// Loads input data from `--input` or `--inputfile` arguments.
    /// Parses authorization list from `--auth` for EIP-7702 transactions.
    pub fn create_tx_env(&self, chain_id: u64) -> Result<TxEnv> {
        let data = load_hex(self.input.clone(), self.inputfile.clone())?.unwrap_or_default();
        let kind = if self.create { TxKind::Create } else { TxKind::Call(self.receiver()?) };
        let authorization_list = self
            .parse_authorization_list(chain_id)?
            .into_iter()
            .map(Either::Right)
            .collect();

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
            authorization_list,
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

        // Set deposit fields if this is a deposit transaction (type 126)
        if self.tx_type()? == MegaTxType::Deposit {
            tx.deposit = DepositTransactionParts {
                source_hash: self.source_hash.unwrap_or(B256::ZERO),
                mint: self.mint,
                is_system_transaction: false,
            };
        }

        Ok(tx)
    }
}
