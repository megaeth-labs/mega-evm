//! The keyless deploy system contract for the `MegaETH` EVM.
//!
//! This contract enables deploying contracts using pre-EIP-155 transactions (Nick's Method)
//! with custom gas limits, solving the problem of contracts failing to deploy on MegaETH
//! due to the different gas model.
//!
//! ## Nick's Method Overview
//!
//! Nick's Method allows deterministic contract deployment across different EVM chains without
//! needing the deployer's private key:
//!
//! 1. Create a contract deployment transaction (to = null, nonce = 0)
//! 2. Generate a random signature (v, r, s) with v = 27 or 28 (pre-EIP-155)
//! 3. Recover the signer address from the signature (nobody knows this private key)
//! 4. Fund the signer address with enough ETH for gas
//! 5. Broadcast the signed transaction
//!
//! The deployment address is deterministic: `keccak256(rlp([signer, 0]))[12:]`

use alloy_evm::Database;
use alloy_primitives::{address, keccak256, Address, Bytes, U256};
use alloy_rlp::Decodable;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

/// The address of the keyless deploy system contract.
pub const KEYLESS_DEPLOY_ADDRESS: Address = address!("0x6342000000000000000000000000000000000003");

/// The code of the keyless deploy contract (version 1.0.0).
pub use mega_system_contracts::keyless_deploy::V1_0_0_CODE as KEYLESS_DEPLOY_CODE;

/// The code hash of the keyless deploy contract (version 1.0.0).
pub use mega_system_contracts::keyless_deploy::V1_0_0_CODE_HASH as KEYLESS_DEPLOY_CODE_HASH;

pub use mega_system_contracts::keyless_deploy::{IKeylessDeploy, InvalidReason};

/// Ensures the keyless deploy contract is deployed in the designated address and returns the
/// state changes. Note that the database `db` is not modified in this function. The caller is
/// responsible to commit the changes to database.
pub fn transact_deploy_keyless_deploy_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_2_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the keyless deploy contract account from the cache
    let acc = db.load_cache_account(KEYLESS_DEPLOY_ADDRESS)?;

    // If the contract is already deployed with the correct code, return early
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == KEYLESS_DEPLOY_CODE_HASH {
            // Although we do not need to update the account, we need to mark it as read
            return Ok(Some(EvmState::from_iter([(
                KEYLESS_DEPLOY_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update the account info with the contract code
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = KEYLESS_DEPLOY_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(KEYLESS_DEPLOY_CODE));

    // Convert the cache account back into a revm account and mark it as touched.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(KEYLESS_DEPLOY_ADDRESS, revm_acc)])))
}

// ============================================================================
// Keyless Deploy Transaction Types and Functions
// ============================================================================

/// A decoded pre-EIP-155 legacy transaction for keyless deployment.
///
/// This represents the RLP-decoded structure of a legacy Ethereum transaction
/// that uses pre-EIP-155 signature format (v = 27 or 28, no chain ID).
#[derive(Debug, Clone)]
pub struct KeylessDeployTx {
    /// Transaction nonce (typically 0 for Nick's Method)
    pub nonce: u64,
    /// Gas price in wei
    pub gas_price: u128,
    /// Gas limit for the transaction
    pub gas_limit: u64,
    /// Value to transfer (typically 0)
    pub value: U256,
    /// Contract initialization code
    pub init_code: Bytes,
    /// Signature v component (must be 27 or 28 for pre-EIP-155)
    pub v: u8,
    /// Signature r component
    pub r: U256,
    /// Signature s component
    pub s: U256,
}

/// Error types for keyless deployment operations.
///
/// These map directly to the Solidity errors defined in IKeylessDeploy.
#[derive(Debug, Clone, Copy)]
pub enum KeylessDeployError {
    /// The gas limit for sandbox execution is too low
    GasLimitLessThanIntrinsic {
        /// The intrinsic gas required for the operation
        intrinsic_gas: u64,
        /// The gas limit provided by the caller
        provided_gas: u64,
    },
    /// The transaction data is malformed (invalid RLP encoding)
    MalformedEncoding,
    /// The transaction is not a contract creation (to address is not empty)
    NotContractCreation,
    /// The transaction is not pre-EIP-155 (v must be 27 or 28)
    NotPreEIP155,
    /// The call tried to transfer ether (maps to NoEtherTransfer)
    NoEtherTransfer,
    /// The contract deployment failed (maps to DeploymentFailed)
    DeploymentFailed,
}

impl From<InvalidReason> for KeylessDeployError {
    fn from(reason: InvalidReason) -> Self {
        match reason {
            InvalidReason::MalformedEncoding => KeylessDeployError::MalformedEncoding,
            InvalidReason::NotContractCreation => KeylessDeployError::NotContractCreation,
            InvalidReason::NotPreEIP155 => KeylessDeployError::NotPreEIP155,
            _ => KeylessDeployError::DeploymentFailed, // Fallback for any new variants
        }
    }
}

impl KeylessDeployError {
    /// Converts this error to the corresponding Solidity InvalidReason if applicable.
    pub fn to_invalid_reason(self) -> Option<InvalidReason> {
        match self {
            KeylessDeployError::MalformedEncoding => Some(InvalidReason::MalformedEncoding),
            KeylessDeployError::NotContractCreation => Some(InvalidReason::NotContractCreation),
            KeylessDeployError::NotPreEIP155 => Some(InvalidReason::NotPreEIP155),
            _ => None,
        }
    }
}

/// Decodes a pre-EIP-155 legacy transaction from RLP bytes.
///
/// The expected RLP structure is: `[nonce, gasPrice, gasLimit, to, value, data, v, r, s]`
///
/// # Validation
/// - The RLP encoding must be valid
/// - The `to` field must be empty (contract creation)
/// - The `v` value must be 27 or 28 (pre-EIP-155)
///
/// # Returns
/// - `Ok(KeylessDeployTx)` if the transaction is valid
/// - `Err(KeylessDeployError::InvalidTransaction(...))` if validation fails
pub fn decode_keyless_tx(rlp_bytes: &[u8]) -> Result<KeylessDeployTx, KeylessDeployError> {
    let mut buf = rlp_bytes;

    // Decode the RLP list header
    let header =
        alloy_rlp::Header::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    if !header.list {
        return Err(KeylessDeployError::MalformedEncoding);
    }

    // Decode nonce
    let nonce = u64::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode gasPrice
    let gas_price = u128::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode gasLimit
    let gas_limit = u64::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode 'to' field - must be empty for contract creation
    let to_header =
        alloy_rlp::Header::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // For contract creation, 'to' must be an empty string (not a list, payload_length = 0)
    if to_header.list || to_header.payload_length != 0 {
        return Err(KeylessDeployError::NotContractCreation);
    }

    // Decode value
    let value = U256::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode init_code (data field)
    let init_code = Bytes::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode v
    let v_raw = u64::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode r
    let r = U256::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Decode s
    let s = U256::decode(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    // Validate v is 27 or 28 (pre-EIP-155)
    if v_raw != 27 && v_raw != 28 {
        return Err(KeylessDeployError::NotPreEIP155);
    }

    Ok(KeylessDeployTx { nonce, gas_price, gas_limit, value, init_code, v: v_raw as u8, r, s })
}

/// Recovers the signer address from a keyless deployment transaction.
///
/// This performs ECDSA signature recovery to derive the public key from the
/// transaction signature, then computes the Ethereum address from the public key.
///
/// # Algorithm
/// 1. Compute the signing hash = keccak256(RLP([nonce, gasPrice, gasLimit, to, value, data]))
/// 2. Recover the public key from (hash, v, r, s)
/// 3. Compute address = keccak256(pubkey)[12:]
///
/// # Returns
/// - `Ok(Address)` - The recovered signer address
/// - `Err(KeylessDeployError::DeploymentFailed)` - If signature recovery fails
pub fn recover_signer(tx: &KeylessDeployTx) -> Result<Address, KeylessDeployError> {
    // Build the unsigned transaction RLP for hashing
    // Structure: [nonce, gasPrice, gasLimit, to, value, data]
    // Note: 'to' is empty bytes for contract creation
    let unsigned_tx_rlp = {
        use alloy_rlp::Encodable;

        let mut buf = Vec::new();

        // We need to encode as a list: [nonce, gasPrice, gasLimit, "", value, data]
        // First calculate the payload length
        let nonce_len = tx.nonce.length();
        let gas_price_len = tx.gas_price.length();
        let gas_limit_len = tx.gas_limit.length();
        let to_len = 1_usize; // Empty string is encoded as 0x80 (1 byte)
        let value_len = tx.value.length();
        let data_len = tx.init_code.length();

        let payload_length =
            nonce_len + gas_price_len + gas_limit_len + to_len + value_len + data_len;

        // Encode the list header
        alloy_rlp::Header { list: true, payload_length }.encode(&mut buf);

        // Encode each field
        tx.nonce.encode(&mut buf);
        tx.gas_price.encode(&mut buf);
        tx.gas_limit.encode(&mut buf);
        // Empty 'to' field for contract creation
        buf.push(0x80); // RLP encoding of empty string
        tx.value.encode(&mut buf);
        tx.init_code.encode(&mut buf);

        buf
    };

    // Compute the message hash
    let msg_hash = keccak256(&unsigned_tx_rlp);

    // Convert r and s to signature bytes (64 bytes total)
    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(&tx.r.to_be_bytes::<32>());
    sig_bytes[32..].copy_from_slice(&tx.s.to_be_bytes::<32>());

    // Recovery ID: v - 27 (v is 27 or 28, so recovery_id is 0 or 1)
    let recovery_id = RecoveryId::try_from((tx.v - 27) as u8)
        .map_err(|_| KeylessDeployError::DeploymentFailed)?;

    // Create the signature
    let signature =
        Signature::from_slice(&sig_bytes).map_err(|_| KeylessDeployError::DeploymentFailed)?;

    // Recover the public key
    let recovered_key = VerifyingKey::recover_from_prehash(&msg_hash[..], &signature, recovery_id)
        .map_err(|_| KeylessDeployError::DeploymentFailed)?;

    // Convert to uncompressed public key point and derive address
    // The uncompressed point is 65 bytes: 0x04 || x (32 bytes) || y (32 bytes)
    // We take keccak256 of x || y (skip the 0x04 prefix) and take the last 20 bytes
    let pubkey_point = recovered_key.to_encoded_point(false);
    let pubkey_bytes = pubkey_point.as_bytes();

    // Skip the 0x04 prefix (1 byte), hash the remaining 64 bytes
    let pubkey_hash = keccak256(&pubkey_bytes[1..]);

    // Take the last 20 bytes as the address
    Ok(Address::from_slice(&pubkey_hash[12..]))
}

/// Calculates the deployment address for a contract created by the given signer.
///
/// For Nick's Method, the nonce is always 0, so the deployment address is:
/// `keccak256(rlp([signer, 0]))[12:]`
///
/// This is equivalent to `signer.create(0)` in alloy.
#[inline]
pub fn calculate_keyless_deploy_address(signer: Address) -> Address {
    signer.create(0)
}

// ============================================================================
// ABI Encoding for Errors and Results
// ============================================================================

/// Encodes a successful keyless deploy result as ABI-encoded bytes.
///
/// The return type is `address`, which is ABI-encoded as a 32-byte value
/// with the address right-aligned (first 12 bytes are zeros).
pub fn encode_success_result(deployed_address: Address) -> Bytes {
    let mut result = [0u8; 32];
    result[12..].copy_from_slice(deployed_address.as_slice());
    Bytes::copy_from_slice(&result)
}

/// Encodes a keyless deploy error as ABI-encoded revert data.
///
/// This matches the Solidity error selectors from IKeylessDeploy.sol.
pub fn encode_error_result(error: KeylessDeployError) -> Bytes {
    match error {
        KeylessDeployError::MalformedEncoding => {
            // InvalidKeylessDeploymentTransaction(InvalidReason.MalformedEncoding)
            // selector: keccak256("InvalidKeylessDeploymentTransaction(uint8)")[:4]
            // = 0x5a3c9cf3
            let mut data = Vec::with_capacity(36);
            data.extend_from_slice(&[0x5a, 0x3c, 0x9c, 0xf3]); // selector
            data.extend_from_slice(&[0u8; 31]); // padding
            data.push(0); // MalformedEncoding = 0
            Bytes::from(data)
        }
        KeylessDeployError::NotContractCreation => {
            // InvalidKeylessDeploymentTransaction(InvalidReason.NotContractCreation)
            let mut data = Vec::with_capacity(36);
            data.extend_from_slice(&[0x5a, 0x3c, 0x9c, 0xf3]); // selector
            data.extend_from_slice(&[0u8; 31]); // padding
            data.push(1); // NotContractCreation = 1
            Bytes::from(data)
        }
        KeylessDeployError::NotPreEIP155 => {
            // InvalidKeylessDeploymentTransaction(InvalidReason.NotPreEIP155)
            let mut data = Vec::with_capacity(36);
            data.extend_from_slice(&[0x5a, 0x3c, 0x9c, 0xf3]); // selector
            data.extend_from_slice(&[0u8; 31]); // padding
            data.push(2); // NotPreEIP155 = 2
            Bytes::from(data)
        }
        KeylessDeployError::NoEtherTransfer => {
            // NoEtherTransfer()
            // selector: keccak256("NoEtherTransfer()")[:4]
            // = 0x6a12f104
            Bytes::copy_from_slice(&[0x6a, 0x12, 0xf1, 0x04])
        }
        KeylessDeployError::DeploymentFailed => {
            // DeploymentFailed()
            // selector: keccak256("DeploymentFailed()")[:4]
            // = 0x30116425
            Bytes::copy_from_slice(&[0x30, 0x11, 0x64, 0x25])
        }
        KeylessDeployError::GasLimitLessThanIntrinsic { .. } => {
            // Map to DeploymentFailed() since the Solidity interface doesn't
            // define a specific error for insufficient gas
            // selector: keccak256("DeploymentFailed()")[:4]
            // = 0x30116425
            Bytes::copy_from_slice(&[0x30, 0x11, 0x64, 0x25])
        }
    }
}
