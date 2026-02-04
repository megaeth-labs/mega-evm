//! Transaction configuration for mega-evme

use alloy_primitives::{address, Address, Bytes, Signature, B256, U256};
use clap::Args;
use mega_evm::{
    alloy_consensus::{
        transaction::SignerRecoverable, Sealed, Signed, Transaction as _, TxEip1559, TxEip2930,
        TxEip7702, TxLegacy,
    },
    alloy_eips::{
        eip2930::{AccessList, AccessListItem},
        eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization, SignedAuthorization},
        Decodable2718, Encodable2718, Typed2718 as _,
    },
    op_alloy_consensus::{OpTxEnvelope, TxDeposit},
    op_revm::transaction::deposit::DepositTransactionParts,
    revm::{context::tx::TxEnv, primitives::TxKind},
    Either, MegaTransaction, MegaTxEnvelope, MegaTxType,
};
use tracing::{debug, trace};

use super::{load_hex, parse_ether_value, EvmeError, Result};

/// Default sender address (Hardhat account #0).
pub const DEFAULT_SENDER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

/// Transaction configuration arguments
#[derive(Args, Debug, Clone)]
#[command(next_help_heading = "Transaction Options")]
pub struct TxArgs {
    /// Transaction type (0=Legacy, 1=EIP-2930, 2=EIP-1559, etc.) [default: 0]
    #[arg(long = "tx-type", visible_aliases = ["type", "ty"])]
    pub tx_type: Option<u8>,

    /// Gas limit for the evm [default: 10000000]
    #[arg(long = "gas", visible_aliases = ["gas-limit"])]
    pub gas: Option<u64>,

    /// Price set for the evm (gas price) [default: 0]
    #[arg(long = "basefee", visible_aliases = ["gas-price", "price", "base-fee"])]
    pub basefee: Option<u64>,

    /// Gas priority fee (EIP-1559)
    #[arg(long = "priority-fee", visible_aliases = ["priorityfee", "tip"])]
    pub priority_fee: Option<u64>,

    /// The transaction origin [default: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266]
    #[arg(long = "sender", visible_aliases = ["from"])]
    pub sender: Option<Address>,

    /// The transaction receiver (execution context)
    #[arg(long = "receiver", visible_aliases = ["to"])]
    pub receiver: Option<Address>,

    /// The transaction nonce
    #[arg(long = "nonce")]
    pub nonce: Option<u64>,

    /// Indicates the action should be create rather than call
    #[arg(long = "create")]
    pub create: Option<bool>,

    /// Value set for the evm.
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    /// Examples: `--value 1ether`, `--value 100gwei`, `--value 1000000000000000000`
    #[arg(long = "value")]
    pub value: Option<String>,

    /// Transaction data (input) as hex string
    #[arg(long = "input", visible_aliases = ["data"])]
    pub input: Option<String>,

    /// File containing transaction data (input). If '-' is specified, input is read from stdin
    #[arg(long = "inputfile", visible_aliases = ["datafile", "input-file", "data-file"])]
    pub inputfile: Option<String>,

    /// Source hash for deposit transactions (tx-type 126)
    #[arg(long = "source-hash", visible_aliases = ["sourcehash"], value_name = "HASH")]
    pub source_hash: Option<B256>,

    /// Amount of ETH to mint for deposit transactions (wei)
    #[arg(long = "mint")]
    pub mint: Option<u128>,

    /// EIP-7702 authorization in format `AUTHORITY:NONCE->DELEGATION` (can be repeated)
    #[arg(long = "auth", visible_aliases = ["authorization"], value_name = "AUTH")]
    pub auth: Vec<String>,

    /// EIP-2930 access list entry in format `ADDRESS` or `ADDRESS:KEY1,KEY2,...` (can be repeated)
    #[arg(long = "access", visible_aliases = ["accesslist", "access-list"], value_name = "ACCESS")]
    pub access: Vec<String>,
}

impl TxArgs {
    /// Validates transaction arguments for consistency.
    ///
    /// Checks:
    /// - `source_hash` and `mint` are only set for deposit transactions (tx-type 126)
    /// - `priority_fee` is not set for legacy or EIP-2930 transactions
    /// - `receiver` must exist when `create` is false, must not exist when `create` is true
    /// - `auth` is only set for EIP-7702 transactions (tx-type 4)
    /// - `access` is only set for EIP-2930, EIP-1559, or EIP-7702 transactions (tx-type 1, 2, 4)
    pub fn validate(&self) -> Result<()> {
        let tx_type = self.mega_tx_type()?;

        // 1. source_hash and mint should only be set when tx_type is deposit
        if tx_type != MegaTxType::Deposit && (self.source_hash.is_some() || self.mint.is_some()) {
            return Err(EvmeError::InvalidInput(
                "--source-hash and --mint are only valid for deposit transactions (--tx-type 126)"
                    .to_string(),
            ));
        }
        if tx_type == MegaTxType::Deposit && self.source_hash.is_none() {
            return Err(EvmeError::InvalidInput(
                "--source-hash is required for deposit transactions (--tx-type 126)".to_string(),
            ));
        }

        // 2. priority_fee must not be set when tx_type is legacy or eip2930
        if matches!(tx_type, MegaTxType::Legacy | MegaTxType::Eip2930) &&
            self.priority_fee.is_some()
        {
            return Err(EvmeError::InvalidInput(
                "--priority-fee is not valid for legacy (0) or EIP-2930 (1) transactions"
                    .to_string(),
            ));
        }

        // 3. receiver must exist when create is false, must not exist when create is true
        if self.create() && self.receiver.is_some() {
            return Err(EvmeError::InvalidInput(
                "--receiver must not be set when --create is specified".to_string(),
            ));
        }

        // 4. auth should only be set when tx_type is EIP-7702
        if tx_type != MegaTxType::Eip7702 && !self.auth.is_empty() {
            return Err(EvmeError::InvalidInput(
                "--auth is only valid for EIP-7702 transactions (--tx-type 4)".to_string(),
            ));
        }

        // 5. access should only be set when tx_type supports access lists (EIP-2930, EIP-1559,
        //    EIP-7702)
        if !self.access.is_empty() &&
            !matches!(tx_type, MegaTxType::Eip2930 | MegaTxType::Eip1559 | MegaTxType::Eip7702)
        {
            return Err(EvmeError::InvalidInput(
                "--access is only valid for EIP-2930 (1), EIP-1559 (2), or EIP-7702 (4) transactions"
                    .to_string(),
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
    pub(crate) fn parse_authorization_list(
        &self,
        chain_id: u64,
    ) -> Result<Vec<RecoveredAuthorization>> {
        self.auth.iter().map(|s| Self::parse_authorization(s, chain_id)).collect()
    }

    /// Parses authorization fields (authority, nonce, delegation) from a single auth string.
    ///
    /// Returns `(Authorization, authority_address)`.
    fn parse_auth_fields(s: &str, chain_id: u64) -> Result<(Authorization, Address)> {
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
            u64::from_str_radix(auth_parts[1].trim().trim_start_matches("0x"), 16).map_err(
                |_| EvmeError::InvalidInput(format!("Invalid nonce: {}", auth_parts[1].trim())),
            )?
        } else {
            auth_parts[1].trim().parse().map_err(|_| {
                EvmeError::InvalidInput(format!("Invalid nonce: {}", auth_parts[1].trim()))
            })?
        };

        let auth = Authorization { chain_id: U256::from(chain_id), address: delegation, nonce };
        Ok((auth, authority))
    }

    /// Parses a single authorization string.
    fn parse_authorization(s: &str, chain_id: u64) -> Result<RecoveredAuthorization> {
        let (auth, authority) = Self::parse_auth_fields(s, chain_id)?;

        trace!(string = %s, chain_id = %chain_id, authority = %authority, delegation = %auth.address, nonce = %auth.nonce, "Parsed authorization");
        Ok(RecoveredAuthorization::new_unchecked(auth, RecoveredAuthority::Valid(authority)))
    }

    /// Parses access list from CLI arguments.
    ///
    /// Format: `ADDRESS` or `ADDRESS:KEY1,KEY2,...`
    /// - ADDRESS: The accessed contract address
    /// - KEY1,KEY2,...: Comma-separated storage keys (B256 hex values)
    pub(crate) fn parse_access_list(&self) -> Result<AccessList> {
        let items: Result<Vec<AccessListItem>> =
            self.access.iter().map(|s| Self::parse_access_list_item(s)).collect();
        Ok(AccessList(items?))
    }

    /// Parses a single access list item.
    fn parse_access_list_item(s: &str) -> Result<AccessListItem> {
        // Check if there's a colon (storage keys present)
        if let Some((addr_str, keys_str)) = s.split_once(':') {
            let address: Address = addr_str.trim().parse().map_err(|_| {
                EvmeError::InvalidInput(format!("Invalid access list address: {}", addr_str.trim()))
            })?;

            let storage_keys: Result<Vec<B256>> = keys_str
                .split(',')
                .map(|k| {
                    k.trim().parse().map_err(|_| {
                        EvmeError::InvalidInput(format!("Invalid storage key: {}", k.trim()))
                    })
                })
                .collect();

            trace!(string = %s, address = %address, storage_keys = ?storage_keys, "Parsed access list item");
            Ok(AccessListItem { address, storage_keys: storage_keys? })
        } else {
            // No storage keys, just address
            let address: Address = s.trim().parse().map_err(|_| {
                EvmeError::InvalidInput(format!("Invalid access list address: {}", s.trim()))
            })?;

            trace!(string = %s, address = %address, "Parsed access list item");
            Ok(AccessListItem { address, storage_keys: Vec::new() })
        }
    }

    /// Returns the gas limit, defaulting to 10,000,000.
    pub fn gas(&self) -> u64 {
        self.gas.unwrap_or(10_000_000)
    }

    /// Returns the base fee, defaulting to 0.
    pub fn basefee(&self) -> u64 {
        self.basefee.unwrap_or(0)
    }

    /// Returns the sender address, defaulting to Hardhat account #0.
    pub fn sender(&self) -> Address {
        self.sender.unwrap_or(DEFAULT_SENDER)
    }

    /// Returns whether this is a create transaction, defaulting to false.
    pub fn create(&self) -> bool {
        self.create.unwrap_or(false)
    }

    /// Returns the receiver address.
    pub fn receiver(&self) -> Address {
        self.receiver.unwrap_or_default()
    }

    /// Returns the parsed value, defaulting to 0.
    pub fn value(&self) -> Result<U256> {
        self.value.as_deref().map(parse_ether_value).transpose().map(|v| v.unwrap_or_default())
    }

    /// Returns the raw transaction type, defaulting to 0 (Legacy).
    pub fn tx_type(&self) -> u8 {
        self.tx_type.unwrap_or(0)
    }

    /// Converts the transaction type to a [`MegaTxType`].
    pub fn mega_tx_type(&self) -> Result<MegaTxType> {
        let ty = self.tx_type();
        match ty {
            0 => Ok(MegaTxType::Legacy),
            1 => Ok(MegaTxType::Eip2930),
            2 => Ok(MegaTxType::Eip1559),
            4 => Ok(MegaTxType::Eip7702),
            126 => Ok(MegaTxType::Deposit),
            _ => Err(EvmeError::UnsupportedTxType(ty)),
        }
    }

    /// Calculates the effective gas price for the transaction.
    pub fn effective_gas_price(&self) -> Result<u128> {
        let basefee = self.basefee() as u128;
        Ok(match self.mega_tx_type()? {
            MegaTxType::Legacy | MegaTxType::Eip2930 => basefee,
            MegaTxType::Eip1559 | MegaTxType::Eip7702 => {
                basefee + self.priority_fee.unwrap_or(0) as u128
            }
            MegaTxType::Deposit => 0,
        })
    }

    /// Creates a [`TxEnv`] from the transaction arguments.
    ///
    /// Loads input data from `--input` or `--inputfile` arguments.
    /// Parses authorization list from `--auth` for EIP-7702 transactions.
    /// Parses access list from `--access` for EIP-2930/EIP-1559/EIP-7702 transactions.
    pub fn create_tx_env(&self, chain_id: u64) -> Result<TxEnv> {
        self.validate()?;

        let data = load_hex(self.input.clone(), self.inputfile.clone())?.unwrap_or_default();
        let kind = if self.create() { TxKind::Create } else { TxKind::Call(self.receiver()) };
        let authorization_list =
            self.parse_authorization_list(chain_id)?.into_iter().map(Either::Right).collect();
        let access_list = self.parse_access_list()?;

        let tx = TxEnv {
            caller: self.sender(),
            gas_price: self.basefee() as u128,
            gas_priority_fee: self.priority_fee.map(|pf| pf as u128),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: self.tx_type(),
            gas_limit: self.gas(),
            data,
            nonce: self.nonce.unwrap_or(0),
            value: self.value()?,
            access_list,
            authorization_list,
            kind,
            chain_id: Some(chain_id),
        };
        debug!(tx = ?tx, "Creating TxEnv");
        Ok(tx)
    }

    /// Creates a [`MegaTransaction`] from the transaction arguments.
    ///
    /// Loads input data from `--input` or `--inputfile` arguments.
    pub fn create_tx(&self, chain_id: u64) -> Result<MegaTransaction> {
        let tx_env = self.create_tx_env(chain_id)?;
        let envelope = create_fake_envelope(&tx_env)?;
        let mut tx = MegaTransaction::new(tx_env);
        tx.enveloped_tx = Some(Bytes::from(envelope.encoded_2718()));

        // Set deposit fields if this is a deposit transaction (type 126)
        if self.mega_tx_type()? == MegaTxType::Deposit {
            tx.deposit = DepositTransactionParts {
                source_hash: self.source_hash.unwrap_or(B256::ZERO),
                mint: self.mint,
                is_system_transaction: false,
            };
        }

        Ok(tx)
    }
}

/// Result of decoding a raw EIP-2718 transaction.
#[derive(Debug)]
pub struct DecodedRawTx {
    /// The decoded transaction environment.
    pub tx_env: TxEnv,
    /// The original raw EIP-2718 encoded bytes.
    pub raw_bytes: Bytes,
    /// Deposit-specific fields, if this is a deposit transaction.
    /// `(source_hash, mint, is_system_transaction)`
    pub deposit: Option<(B256, Option<u128>, bool)>,
}

impl DecodedRawTx {
    /// Decodes raw EIP-2718 encoded transaction bytes into a [`TxEnv`].
    ///
    /// Recovers the signer from the signature (or uses the `from` field for deposits)
    /// and extracts all transaction fields. No CLI overrides are applied.
    pub fn from_raw(raw_bytes: impl Into<Bytes>) -> Result<Self> {
        let raw_bytes = raw_bytes.into();
        let envelope = OpTxEnvelope::decode_2718(&mut &raw_bytes[..]).map_err(|e| {
            EvmeError::InvalidInput(format!("Failed to decode raw transaction: {e}"))
        })?;

        let caller = envelope
            .recover_signer()
            .map_err(|e| EvmeError::InvalidInput(format!("Failed to recover signer: {e}")))?;

        let deposit = envelope.as_deposit().map(|d| {
            let mint = if d.mint == 0 { None } else { Some(d.mint) };
            (d.source_hash, mint, d.is_system_transaction)
        });

        let decoded_chain_id = envelope.chain_id();
        let (gas_price, gas_priority_fee) = match envelope {
            OpTxEnvelope::Legacy(_) | OpTxEnvelope::Eip2930(_) => {
                (envelope.gas_price().unwrap_or(0), None)
            }
            OpTxEnvelope::Eip1559(_) | OpTxEnvelope::Eip7702(_) => {
                (envelope.max_fee_per_gas(), envelope.max_priority_fee_per_gas())
            }
            OpTxEnvelope::Deposit(_) => (0, None),
        };

        let authorization_list = envelope
            .authorization_list()
            .map(|list| list.iter().map(|sa| Either::Right(sa.clone().into_recovered())).collect())
            .unwrap_or_default();

        let tx_env = TxEnv {
            caller,
            gas_price,
            gas_priority_fee,
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: envelope.ty(),
            gas_limit: envelope.gas_limit(),
            data: envelope.input().clone(),
            nonce: envelope.nonce(),
            value: envelope.value(),
            access_list: envelope.access_list().cloned().unwrap_or_default(),
            authorization_list,
            kind: envelope.kind(),
            chain_id: decoded_chain_id,
        };

        Ok(Self { tx_env, raw_bytes, deposit })
    }

    /// Applies explicitly-set [`TxArgs`] fields as overrides to the decoded [`TxEnv`].
    ///
    /// Only fields that were explicitly provided via CLI flags are overridden;
    /// `None` / empty fields in `tx_args` leave the base value unchanged.
    pub fn override_tx_env(mut self, tx_args: &TxArgs) -> Result<Self> {
        if let Some(tx_type) = tx_args.tx_type {
            self.tx_env.tx_type = tx_type;
        }
        if let Some(gas) = tx_args.gas {
            self.tx_env.gas_limit = gas;
        }
        if let Some(basefee) = tx_args.basefee {
            self.tx_env.gas_price = basefee as u128;
        }
        if let Some(priority_fee) = tx_args.priority_fee {
            self.tx_env.gas_priority_fee = Some(priority_fee as u128);
        }
        if let Some(sender) = tx_args.sender {
            self.tx_env.caller = sender;
        }
        if let Some(ref value) = tx_args.value {
            self.tx_env.value = parse_ether_value(value)?;
        }
        if let Some(nonce) = tx_args.nonce {
            self.tx_env.nonce = nonce;
        }
        if tx_args.input.is_some() || tx_args.inputfile.is_some() {
            self.tx_env.data =
                load_hex(tx_args.input.clone(), tx_args.inputfile.clone())?.unwrap_or_default();
        }
        if tx_args.create.unwrap_or(false) {
            self.tx_env.kind = TxKind::Create;
        } else if let Some(receiver) = tx_args.receiver {
            self.tx_env.kind = TxKind::Call(receiver);
        }
        if !tx_args.access.is_empty() {
            self.tx_env.access_list = tx_args.parse_access_list()?;
        }
        if !tx_args.auth.is_empty() {
            let chain_id = self.tx_env.chain_id.unwrap_or(0);
            self.tx_env.authorization_list = tx_args
                .parse_authorization_list(chain_id)?
                .into_iter()
                .map(Either::Right)
                .collect();
        }
        if let Some((ref mut source_hash, ref mut mint, _)) = self.deposit {
            if let Some(sh) = tx_args.source_hash {
                *source_hash = sh;
            }
            if tx_args.mint.is_some() {
                *mint = tx_args.mint;
            }
        }
        Ok(self)
    }

    /// Converts the decoded raw transaction into a [`MegaTransaction`].
    ///
    /// Uses the stored raw bytes for `enveloped_tx` (used in L1 fee calculation).
    pub fn into_tx(self) -> MegaTransaction {
        let mut tx = MegaTransaction::new(self.tx_env);
        tx.enveloped_tx = Some(self.raw_bytes);
        if let Some((source_hash, mint, is_system_transaction)) = self.deposit {
            tx.deposit = DepositTransactionParts { source_hash, mint, is_system_transaction };
        }
        tx
    }
}

/// Constructs a fake [`MegaTxEnvelope`] from a [`TxEnv`] for EIP-2718 encoding.
///
/// The created envelope only contains the information available in [`TxEnv`] and fills many
/// fields with placeholder values. The encoded envelope is used for L1 data fee calculation.
/// A dummy signature is used since the CLI doesn't have access to signing keys — the non-zero
/// r/s bytes ensure the encoded size is realistic (matching real signed transactions).
fn create_fake_envelope(tx_env: &TxEnv) -> Result<MegaTxEnvelope> {
    let dummy_sig = Signature::new(U256::from(1u64), U256::from(1u64), false);
    let chain_id = tx_env.chain_id.unwrap_or(0);
    let tx_type = MegaTxType::try_from(tx_env.tx_type)
        .map_err(|_| EvmeError::UnsupportedTxType(tx_env.tx_type))?;

    match tx_type {
        MegaTxType::Legacy => {
            let tx = TxLegacy {
                chain_id: tx_env.chain_id,
                nonce: tx_env.nonce,
                gas_price: tx_env.gas_price,
                gas_limit: tx_env.gas_limit,
                to: tx_env.kind,
                value: tx_env.value,
                input: tx_env.data.clone(),
            };
            Ok(MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, dummy_sig, Default::default())))
        }
        MegaTxType::Eip2930 => {
            let tx = TxEip2930 {
                chain_id,
                nonce: tx_env.nonce,
                gas_price: tx_env.gas_price,
                gas_limit: tx_env.gas_limit,
                to: tx_env.kind,
                value: tx_env.value,
                access_list: tx_env.access_list.clone(),
                input: tx_env.data.clone(),
            };
            Ok(MegaTxEnvelope::Eip2930(Signed::new_unchecked(tx, dummy_sig, Default::default())))
        }
        MegaTxType::Eip1559 => {
            let tx = TxEip1559 {
                chain_id,
                nonce: tx_env.nonce,
                gas_limit: tx_env.gas_limit,
                max_fee_per_gas: tx_env.gas_price,
                max_priority_fee_per_gas: tx_env.gas_priority_fee.unwrap_or(0),
                to: tx_env.kind,
                value: tx_env.value,
                access_list: tx_env.access_list.clone(),
                input: tx_env.data.clone(),
            };
            Ok(MegaTxEnvelope::Eip1559(Signed::new_unchecked(tx, dummy_sig, Default::default())))
        }
        MegaTxType::Eip7702 => {
            let to = match tx_env.kind {
                TxKind::Call(addr) => addr,
                TxKind::Create => {
                    return Err(EvmeError::InvalidInput(
                        "EIP-7702 transactions cannot be contract creation".to_string(),
                    ));
                }
            };

            let authorization_list: Vec<SignedAuthorization> = tx_env
                .authorization_list
                .iter()
                .map(|either| match either {
                    Either::Left(signed) => signed.clone(),
                    Either::Right(recovered) => SignedAuthorization::new_unchecked(
                        Authorization::clone(recovered),
                        0,
                        U256::from(1u64),
                        U256::from(1u64),
                    ),
                })
                .collect();

            let tx = TxEip7702 {
                chain_id,
                nonce: tx_env.nonce,
                gas_limit: tx_env.gas_limit,
                max_fee_per_gas: tx_env.gas_price,
                max_priority_fee_per_gas: tx_env.gas_priority_fee.unwrap_or(0),
                to,
                value: tx_env.value,
                access_list: tx_env.access_list.clone(),
                authorization_list,
                input: tx_env.data.clone(),
            };
            Ok(MegaTxEnvelope::Eip7702(Signed::new_unchecked(tx, dummy_sig, Default::default())))
        }
        MegaTxType::Deposit => {
            let tx = TxDeposit {
                source_hash: B256::ZERO,
                from: tx_env.caller,
                to: tx_env.kind,
                mint: 0,
                value: tx_env.value,
                gas_limit: tx_env.gas_limit,
                is_system_transaction: false,
                input: tx_env.data.clone(),
            };
            Ok(MegaTxEnvelope::Deposit(Sealed::new_unchecked(tx, B256::ZERO)))
        }
    }
}
