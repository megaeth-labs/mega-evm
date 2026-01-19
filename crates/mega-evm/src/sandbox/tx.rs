//! Keyless deploy transaction types and functions.
//!
//! This module provides functions for decoding and validating pre-EIP-155 legacy transactions
//! used in Nick's Method for deterministic contract deployment.

use alloy_consensus::{transaction::RlpEcdsaDecodableTx, Signed, TxLegacy};
use alloy_primitives::Address;

use super::error::KeylessDeployError;

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
/// - `Ok(Signed<TxLegacy>)` if the transaction is valid
/// - `Err(KeylessDeployError::InvalidTransaction(...))` if validation fails
pub fn decode_keyless_tx(rlp_bytes: &[u8]) -> Result<Signed<TxLegacy>, KeylessDeployError> {
    let mut buf = rlp_bytes;
    let signed =
        TxLegacy::rlp_decode_signed(&mut buf).map_err(|_| KeylessDeployError::MalformedEncoding)?;

    if !signed.tx().to.is_create() {
        return Err(KeylessDeployError::NotContractCreation);
    }
    if signed.tx().chain_id.is_some() {
        return Err(KeylessDeployError::NotPreEIP155);
    }
    Ok(signed)
}

/// Recovers the signer address from a keyless deployment transaction.
///
/// Uses alloy's built-in signature recovery to derive the signer address
/// from the signed transaction.
///
/// # Returns
/// - `Ok(Address)` - The recovered signer address
/// - `Err(KeylessDeployError::InvalidSignature)` - If signature recovery fails
pub fn recover_signer(signed_tx: &Signed<TxLegacy>) -> Result<Address, KeylessDeployError> {
    signed_tx.recover_signer().map_err(|_| KeylessDeployError::InvalidSignature)
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

#[allow(missing_docs, unused)]
#[cfg(any(test, feature = "test-utils"))]
pub mod tests {
    use super::*;
    use alloy_primitives::{address, b256, bytes, hex, B256, U256};

    // =============================================================================
    // Test vectors generated using Foundry's `cast` command
    // =============================================================================
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
    pub const CREATE2_FACTORY_TX: &[u8] = &hex!("f8a58085174876e800830186a08080b853604580600e600039806000f350fe7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf31ba02222222222222222222222222222222222222222222222222222222222222222a02222222222222222222222222222222222222222222222222222222222222222");

    /// CREATE2 factory deployer address recovered from signature.
    pub const CREATE2_FACTORY_DEPLOYER: Address =
        address!("3fab184622dc19b6109349b94811493bf2a45362");

    /// CREATE2 factory contract address.
    pub const CREATE2_FACTORY_CONTRACT: Address =
        address!("4e59b44847b379578588920ca78fbf26c0b4956c");

    /// The code hash of the CREATE2 factory contract.
    pub const CREATE2_FACTORY_CODE_HASH: B256 =
        b256!("0x2fa86add0aed31f33a762c9d88e807c475bd51d0f52bd0955754b2608f7e4989");

    /// EIP-1820 pre-signed deployment transaction.
    /// Source: https://eips.ethereum.org/EIPS/eip-1820
    pub const EIP1820_TX: &[u8] = &hex!("f90a388085174876e800830c35008080b909e5608060405234801561001057600080fd5b506109c5806100206000396000f3fe608060405234801561001057600080fd5b50600436106100a5576000357c010000000000000000000000000000000000000000000000000000000090048063a41e7d5111610078578063a41e7d51146101d4578063aabbb8ca1461020a578063b705676514610236578063f712f3e814610280576100a5565b806329965a1d146100aa5780633d584063146100e25780635df8122f1461012457806365ba36c114610152575b600080fd5b6100e0600480360360608110156100c057600080fd5b50600160a060020a038135811691602081013591604090910135166102b6565b005b610108600480360360208110156100f857600080fd5b5035600160a060020a0316610570565b60408051600160a060020a039092168252519081900360200190f35b6100e06004803603604081101561013a57600080fd5b50600160a060020a03813581169160200135166105bc565b6101c26004803603602081101561016857600080fd5b81019060208101813564010000000081111561018357600080fd5b82018360208201111561019557600080fd5b803590602001918460018302840111640100000000831117156101b757600080fd5b5090925090506106b3565b60408051918252519081900360200190f35b6100e0600480360360408110156101ea57600080fd5b508035600160a060020a03169060200135600160e060020a0319166106ee565b6101086004803603604081101561022057600080fd5b50600160a060020a038135169060200135610778565b61026c6004803603604081101561024c57600080fd5b508035600160a060020a03169060200135600160e060020a0319166107ef565b604080519115158252519081900360200190f35b61026c6004803603604081101561029657600080fd5b508035600160a060020a03169060200135600160e060020a0319166108aa565b6000600160a060020a038416156102cd57836102cf565b335b9050336102db82610570565b600160a060020a031614610339576040805160e560020a62461bcd02815260206004820152600f60248201527f4e6f7420746865206d616e616765720000000000000000000000000000000000604482015290519081900360640190fd5b6103428361092a565b15610397576040805160e560020a62461bcd02815260206004820152601a60248201527f4d757374206e6f7420626520616e204552433136352068617368000000000000604482015290519081900360640190fd5b600160a060020a038216158015906103b85750600160a060020a0382163314155b156104ff5760405160200180807f455243313832305f4143434550545f4d4147494300000000000000000000000081525060140190506040516020818303038152906040528051906020012082600160a060020a031663249cb3fa85846040518363ffffffff167c01000000000000000000000000000000000000000000000000000000000281526004018083815260200182600160a060020a0316600160a060020a031681526020019250505060206040518083038186803b15801561047e57600080fd5b505afa158015610492573d6000803e3d6000fd5b505050506040513d60208110156104a857600080fd5b5051146104ff576040805160e560020a62461bcd02815260206004820181905260248201527f446f6573206e6f7420696d706c656d656e742074686520696e74657266616365604482015290519081900360640190fd5b600160a060020a03818116600081815260208181526040808320888452909152808220805473ffffffffffffffffffffffffffffffffffffffff19169487169485179055518692917f93baa6efbd2244243bfee6ce4cfdd1d04fc4c0e9a786abd3a41313bd352db15391a450505050565b600160a060020a03818116600090815260016020526040812054909116151561059a5750806105b7565b50600160a060020a03808216600090815260016020526040902054165b919050565b336105c683610570565b600160a060020a031614610624576040805160e560020a62461bcd02815260206004820152600f60248201527f4e6f7420746865206d616e616765720000000000000000000000000000000000604482015290519081900360640190fd5b81600160a060020a031681600160a060020a0316146106435780610646565b60005b600160a060020a03838116600081815260016020526040808220805473ffffffffffffffffffffffffffffffffffffffff19169585169590951790945592519184169290917f605c2dbf762e5f7d60a546d42e7205dcb1b011ebc62a61736a57c9089d3a43509190a35050565b600082826040516020018083838082843780830192505050925050506040516020818303038152906040528051906020012090505b92915050565b6106f882826107ef565b610703576000610705565b815b600160a060020a03928316600081815260208181526040808320600160e060020a031996909616808452958252808320805473ffffffffffffffffffffffffffffffffffffffff19169590971694909417909555908152600284528181209281529190925220805460ff19166001179055565b600080600160a060020a038416156107905783610792565b335b905061079d8361092a565b156107c357826107ad82826108aa565b6107b85760006107ba565b815b925050506106e8565b600160a060020a0390811660009081526020818152604080832086845290915290205416905092915050565b6000808061081d857f01ffc9a70000000000000000000000000000000000000000000000000000000061094c565b909250905081158061082d575080155b1561083d576000925050506106e8565b61084f85600160e060020a031961094c565b909250905081158061086057508015155b15610870576000925050506106e8565b61087a858561094c565b909250905060018214801561088f5750806001145b1561089f576001925050506106e8565b506000949350505050565b600160a060020a0382166000908152600260209081526040808320600160e060020a03198516845290915281205460ff1615156108f2576108eb83836107ef565b90506106e8565b50600160a060020a03808316600081815260208181526040808320600160e060020a0319871684529091529020549091161492915050565b7bffffffffffffffffffffffffffffffffffffffffffffffffffffffff161590565b6040517f01ffc9a7000000000000000000000000000000000000000000000000000000008082526004820183905260009182919060208160248189617530fa90519096909550935050505056fea165627a7a72305820377f4a2d4301ede9949f163f319021a6e9c687c292a5e2b2c4734c126b524e6c00291ba01820182018201820182018201820182018201820182018201820182018201820a01820182018201820182018201820182018201820182018201820182018201820");

    /// The deployer address recovered from EIP-1820 signature.
    pub const EIP1820_DEPLOYER: Address = address!("a990077c3205cbDf861e17Fa532eeB069cE9fF96");

    /// The contract address where EIP-1820 registry will be deployed.
    pub const EIP1820_CONTRACT: Address = address!("1820a4B7618BdE71Dce8cdc73aAB6C95905faD24");

    /// The code hash of the EIP-1820 registry contract.
    pub const EIP1820_CODE_HASH: B256 =
        b256!("0xf0aa940bb32e37c5f7268b53acc48c7cdd148cd0fc196f30faa00a4d66c0443a");

    /// Post-EIP-155 transaction with chain ID 1 (v=0x26=38).
    /// Generated: cast mktx --private-key 0x0123..def --legacy --chain 1 --create 0x6080604052
    pub const POST_EIP155_CHAIN_1_TX: &[u8] = &hex!("f856808504a817c800830186a0808085608060405226a0fceb37453e90ac5ec2780748b7a4907b1dcfb87708697de2e6be19831938c77ba0224ee4c1aaa6a1490b4e3a1fbed7c5151668a12b6f6e3227c2692a64cf79e81f");

    /// Post-EIP-155 transaction with chain ID 1337 (v=0x0a95=2709).
    /// Generated: cast mktx --private-key 0x0123..def --legacy --chain 1337 --create 0x6080604052
    pub const POST_EIP155_CHAIN_1337_TX: &[u8] = &hex!("f858808504a817c800830186a08080856080604052820a95a0bea22b3c93e686c12e09c4c519919244bd710de249e2588b22cfb28a2d9ecc22a04b8d3598bae247ce8846aafa41fdaadff2e2154034f5789448bf263d905f20c3");

    /// Non-contract creation transaction (to=0x4242...42, pre-EIP-155, v=27).
    /// Generated: cast mktx --private-key 0x0123..def --legacy
    /// 0x4242424242424242424242424242424242424242
    pub const NON_CONTRACT_CREATION_TX: &[u8] = &hex!("f866808504a817c800825208944242424242424242424242424242424242424242808082072ba094a1d148b08c268261581dd9e90478bae0c937e26eec574809876bdd34de82daa03e2fb4dd2cb99703feeb0da3c3a1062a047f0091aa09610c3a7feecfda6f6bad");

    #[test]
    fn test_decode_create2_factory_deployment() {
        // The canonical CREATE2 factory deployment - a well-known pre-EIP-155 transaction
        let signed =
            decode_keyless_tx(CREATE2_FACTORY_TX).expect("should decode CREATE2 factory tx");

        let tx = signed.tx();
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.gas_price, 100_000_000_000); // 100 gwei
        assert_eq!(tx.gas_limit, 100_000);
        assert_eq!(tx.value, U256::ZERO);
        // Pre-EIP-155 means chain_id is None
        assert!(tx.chain_id.is_none());
        // r and s are both 0x2222...22 (deterministic signature for Nick's Method)
        assert_eq!(
            signed.signature().r(),
            U256::from_be_bytes(hex!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ))
        );
        assert_eq!(
            signed.signature().s(),
            U256::from_be_bytes(hex!(
                "2222222222222222222222222222222222222222222222222222222222222222"
            ))
        );
        // Verify init_code matches CREATE2 factory bytecode
        assert_eq!(
            tx.input,
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
