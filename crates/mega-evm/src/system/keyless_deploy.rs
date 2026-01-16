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

// Re-export error types from sandbox
pub use crate::sandbox::{encode_error_result, encode_success_result, KeylessDeployError};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        .map_err(|_| KeylessDeployError::InvalidSignature)?;

    // Create the signature
    let signature =
        Signature::from_slice(&sig_bytes).map_err(|_| KeylessDeployError::InvalidSignature)?;

    // Recover the public key
    let recovered_key = VerifyingKey::recover_from_prehash(&msg_hash[..], &signature, recovery_id)
        .map_err(|_| KeylessDeployError::InvalidSignature)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes, hex};

    // =========================================================================
    // Test vectors generated using Foundry's `cast` command
    // =========================================================================
    //
    // Pre-EIP-155 (CREATE2 factory deployment - Nick's Method):
    //   This is the canonical CREATE2 factory deployment transaction.
    //   Source: https://github.com/Arachnid/deterministic-deployment-proxy
    //   Generated/verified with: cast decode-tx <tx>
    //
    // Post-EIP-155 transactions generated with:
    //   cast mktx --private-key 0x0123...def --gas-limit 100000 --gas-price 20gwei \
    //             --nonce 0 --legacy --chain <chain_id> --create 0x6080604052
    //
    // Non-contract creation generated with:
    //   cast mktx --private-key 0x0123...def --gas-limit 21000 --gas-price 20gwei \
    //             --nonce 0 --legacy 0x4242424242424242424242424242424242424242

    /// The canonical CREATE2 factory deployment transaction (pre-EIP-155, v=27).
    /// Signer: 0x3fab184622dc19b6109349b94811493bf2a45362
    /// Deployed to: 0x4e59b44847b379578588920ca78fbf26c0b4956c
    const CREATE2_FACTORY_TX: &[u8] = &hex!("f8a58085174876e800830186a08080b853604580600e600039806000f350fe7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf31ba02222222222222222222222222222222222222222222222222222222222222222a02222222222222222222222222222222222222222222222222222222222222222");

    /// Post-EIP-155 transaction with chain ID 1 (v=0x26=38).
    /// Generated: cast mktx --private-key 0x0123..def --legacy --chain 1 --create 0x6080604052
    const POST_EIP155_CHAIN_1_TX: &[u8] = &hex!("f856808504a817c800830186a0808085608060405226a0fceb37453e90ac5ec2780748b7a4907b1dcfb87708697de2e6be19831938c77ba0224ee4c1aaa6a1490b4e3a1fbed7c5151668a12b6f6e3227c2692a64cf79e81f");

    /// Post-EIP-155 transaction with chain ID 1337 (v=0x0a95=2709).
    /// Generated: cast mktx --private-key 0x0123..def --legacy --chain 1337 --create 0x6080604052
    const POST_EIP155_CHAIN_1337_TX: &[u8] = &hex!("f858808504a817c800830186a08080856080604052820a95a0bea22b3c93e686c12e09c4c519919244bd710de249e2588b22cfb28a2d9ecc22a04b8d3598bae247ce8846aafa41fdaadff2e2154034f5789448bf263d905f20c3");

    /// Non-contract creation transaction (to=0x4242...42, pre-EIP-155, v=27).
    /// Generated: cast mktx --private-key 0x0123..def --legacy 0x4242424242424242424242424242424242424242
    const NON_CONTRACT_CREATION_TX: &[u8] = &hex!("f866808504a817c800825208944242424242424242424242424242424242424242808082072ba094a1d148b08c268261581dd9e90478bae0c937e26eec574809876bdd34de82daa03e2fb4dd2cb99703feeb0da3c3a1062a047f0091aa09610c3a7feecfda6f6bad");

    #[test]
    fn test_decode_create2_factory_deployment() {
        // The canonical CREATE2 factory deployment - a well-known pre-EIP-155 transaction
        let tx = decode_keyless_tx(CREATE2_FACTORY_TX).expect("should decode CREATE2 factory tx");

        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.gas_price, 100_000_000_000); // 100 gwei
        assert_eq!(tx.gas_limit, 100_000);
        assert_eq!(tx.value, U256::ZERO);
        assert_eq!(tx.v, 27); // pre-EIP-155
        // r and s are both 0x2222...22 (deterministic signature for Nick's Method)
        assert_eq!(
            tx.r,
            U256::from_be_bytes(hex!("2222222222222222222222222222222222222222222222222222222222222222"))
        );
        assert_eq!(
            tx.s,
            U256::from_be_bytes(hex!("2222222222222222222222222222222222222222222222222222222222222222"))
        );
        // Verify init_code matches CREATE2 factory bytecode
        assert_eq!(
            tx.init_code,
            bytes!("604580600e600039806000f350fe7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3")
        );
    }

    #[test]
    fn test_recover_signer_create2_factory() {
        // Verify we can recover the correct signer from the CREATE2 factory tx
        let tx = decode_keyless_tx(CREATE2_FACTORY_TX).expect("should decode");
        let signer = recover_signer(&tx).expect("should recover signer");

        // The canonical CREATE2 factory signer address
        assert_eq!(signer, address!("3fab184622dc19b6109349b94811493bf2a45362"));
    }

    #[test]
    fn test_calculate_create2_factory_deploy_address() {
        // Verify the deployment address calculation matches the known CREATE2 factory address
        let tx = decode_keyless_tx(CREATE2_FACTORY_TX).expect("should decode");
        let signer = recover_signer(&tx).expect("should recover signer");
        let deploy_address = calculate_keyless_deploy_address(signer);

        // The canonical CREATE2 factory is deployed at this address
        assert_eq!(deploy_address, address!("4e59b44847b379578588920ca78fbf26c0b4956c"));
    }

    #[test]
    fn test_decode_rejects_post_eip155_chain_1() {
        // Transaction generated with: cast mktx ... --chain 1 --create 0x6080604052
        // v = 1 * 2 + 35 + 1 = 38 (0x26)
        let result = decode_keyless_tx(POST_EIP155_CHAIN_1_TX);
        assert_eq!(result, Err(KeylessDeployError::NotPreEIP155));
    }

    #[test]
    fn test_decode_rejects_post_eip155_chain_1337() {
        // Transaction generated with: cast mktx ... --chain 1337 --create 0x6080604052
        // v = 1337 * 2 + 35 + 0 = 2709 (0x0a95)
        let result = decode_keyless_tx(POST_EIP155_CHAIN_1337_TX);
        assert_eq!(result, Err(KeylessDeployError::NotPreEIP155));
    }

    #[test]
    fn test_decode_rejects_non_contract_creation() {
        // Transaction with to=0x4242...42 (not a contract creation)
        // Generated with: cast mktx ... --legacy 0x4242424242424242424242424242424242424242
        let result = decode_keyless_tx(NON_CONTRACT_CREATION_TX);
        assert_eq!(result, Err(KeylessDeployError::NotContractCreation));
    }

    #[test]
    fn test_decode_rejects_malformed_rlp() {
        // Random bytes, not valid RLP
        let invalid_rlp = hex!("deadbeef");
        let result = decode_keyless_tx(&invalid_rlp);
        assert_eq!(result, Err(KeylessDeployError::MalformedEncoding));
    }

    #[test]
    fn test_decode_rejects_empty_input() {
        let result = decode_keyless_tx(&[]);
        assert_eq!(result, Err(KeylessDeployError::MalformedEncoding));
    }

    #[test]
    fn test_decode_rejects_truncated_rlp() {
        // Truncate the CREATE2 factory tx
        let truncated = &CREATE2_FACTORY_TX[..CREATE2_FACTORY_TX.len() - 10];
        let result = decode_keyless_tx(truncated);
        assert_eq!(result, Err(KeylessDeployError::MalformedEncoding));
    }
}
