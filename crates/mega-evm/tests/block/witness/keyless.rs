//! Keyless deployments through the harness: a deployment that succeeds, and one refused by each
//! rule that reads state — or refused before any read — replay from the witness, and the record
//! holds exactly the accounts each rule reached.

use alloy_primitives::{hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    system::keyless::{
        decode_error_result, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
    },
    test_utils::MemoryDatabase,
};
use revm::{
    bytecode::opcode::{CODECOPY, PUSH0, RETURN, REVERT},
    context::result::ExecutionResult,
};

use super::harness::{call, call_with_value, Case, Run};
use crate::common;

/// A gas limit with room for the overhead, the creation, two new accounts and a small runtime's
/// deposit at any byte price the suite runs at.
const GAS: u64 = 20_000_000;

/// A `gasLimitOverride` above what any transaction here can forward.
const LARGE_OVERRIDE: u64 = 10_000_000_000;

/// A pre-EIP-155 signed creation, as Nick's Method makes one: a fixed signature over a
/// transaction nobody holds the key of, so the signer is whatever the contents recover to.
struct Deployment {
    tx: Bytes,
    signer: Address,
    address: Address,
}

impl Deployment {
    /// A deployment of `init_code` carrying `value`, signed at nonce 0 with gas limit 1,000,000.
    fn new(init_code: Bytes, value: U256) -> Self {
        let tx = TxLegacy {
            nonce: 0,
            gas_price: 100_000_000_000,
            gas_limit: 1_000_000,
            to: TxKind::Create,
            value,
            input: init_code,
            chain_id: None,
        };
        let word = U256::from_be_bytes(hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));
        let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
        let mut encoded = Vec::new();
        signed.rlp_encode(&mut encoded);
        let signer = signed.recover_signer().expect("a Nick's-Method signature recovers");
        Self { tx: encoded.into(), signer, address: signer.create(0) }
    }

    /// The calldata of a `keylessDeploy` call deploying this.
    fn call_data(&self) -> Bytes {
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: self.tx.clone(),
            gasLimitOverride: U256::from(LARGE_OVERRIDE),
        }
        .abi_encode()
        .into()
    }
}

/// Init code that runs `prefix`, then deploys `runtime`, which it copies from its own tail.
fn constructor(prefix: &[u8], runtime: &[u8]) -> Bytes {
    let len = u8::try_from(runtime.len()).expect("a short runtime");
    let tail = u16::try_from(prefix.len() + 11).expect("a short prefix").to_be_bytes();
    let mut code = prefix.to_vec();
    code.extend_from_slice(&[0x60, len, 0x61, tail[0], tail[1], PUSH0, CODECOPY]);
    code.extend_from_slice(&[0x60, len, PUSH0, RETURN]);
    code.extend_from_slice(runtime);
    code.into()
}

/// A runtime of five bytes that stops.
fn runtime() -> Vec<u8> {
    vec![0x00; 5]
}

/// The database every case starts from: the caller funded; the system contracts are deployed by
/// the block itself.
fn db() -> MemoryDatabase {
    common::database()
}

/// What the `keylessDeploy` call at `index` returned, which the deployment succeeded or failed
/// in.
fn returned(run: &Run, index: usize) -> IKeylessDeploy::keylessDeployReturn {
    let ExecutionResult::Success { output, .. } = &run.tx(index).result else {
        panic!("the call did not succeed: {:?}", run.tx(index).result);
    };
    IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data()).expect("a return")
}

/// The error the `keylessDeploy` call at `index` reverted with.
fn refused(run: &Run, index: usize) -> KeylessDeployError {
    let ExecutionResult::Revert { output, .. } = &run.tx(index).result else {
        panic!("the call did not revert: {:?}", run.tx(index).result);
    };
    decode_error_result(output).expect("a keyless deploy error")
}

/// A deployment that succeeds: the signer's and the deploy address's accounts are in the record
/// as absent, both buckets are exported, and the deployed code is in the block's state.
#[test]
fn test_a_keyless_deployment_replays() {
    let deployment = Deployment::new(constructor(&[], &runtime()), U256::ZERO);
    let replay = Case::new("keyless deployment", db())
        .tx(call(0, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    let ret = returned(run, 0);
    assert_eq!(ret.deployedAddress, deployment.address, "{:?}", ret.errorData);
    assert_eq!(run.record.accounts.get(&deployment.signer), Some(&None), "the signer was read");
    assert_eq!(run.record.accounts.get(&deployment.address), Some(&None), "the address was read");
    if !common::state_is_free() {
        assert_eq!(run.bucket_ids.len(), 2, "the signer's bucket and the deploy address's");
    }
    assert!(
        run.tx(0)
            .state
            .get(&deployment.address)
            .is_some_and(|account| !account.info.is_empty_code_hash()),
        "the code was deployed"
    );
}

/// A call carrying value is refused before any read: neither the signer nor the deploy address
/// is in the record.
#[test]
fn test_a_value_bearing_keyless_call_reads_no_account() {
    let deployment = Deployment::new(constructor(&[], &runtime()), U256::ZERO);
    let replay = Case::new("keyless value", db())
        .tx(call_with_value(0, KEYLESS_DEPLOY_ADDRESS, U256::from(1), deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    assert_eq!(refused(run, 0), KeylessDeployError::NoEtherTransfer);
    assert!(!run.record.accounts.contains_key(&deployment.signer), "no signer read");
    assert!(!run.record.accounts.contains_key(&deployment.address), "no deploy-address read");
    assert!(run.bucket_ids.is_empty(), "no charge was priced");
}

/// A signer whose nonce is too high is refused after the signer's account was read and before
/// the deploy address's is.
#[test]
fn test_a_signer_nonce_refusal_reads_the_signer_alone() {
    let deployment = Deployment::new(constructor(&[], &runtime()), U256::ZERO);
    let replay = Case::new("keyless signer nonce", db().account_nonce(deployment.signer, 2))
        .tx(call(0, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    assert_eq!(refused(run, 0), KeylessDeployError::SignerNonceTooHigh { signer_nonce: 2 });
    assert!(run.record.accounts.contains_key(&deployment.signer), "the signer was read");
    assert!(!run.record.accounts.contains_key(&deployment.address), "the address was not");
    assert!(run.bucket_ids.is_empty(), "an existing signer's account is not charged");
}

/// A deploy address that holds code is refused after both accounts were read.
#[test]
fn test_an_occupied_deploy_address_refusal_reads_both_accounts() {
    let deployment = Deployment::new(constructor(&[], &runtime()), U256::ZERO);
    let db = db().account_code(deployment.address, Bytes::from(vec![0x00]));
    let replay = Case::new("keyless occupied", db)
        .tx(call(0, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    assert_eq!(refused(run, 0), KeylessDeployError::ContractAlreadyExists);
    assert!(run.record.accounts.contains_key(&deployment.signer));
    assert!(run.record.accounts.contains_key(&deployment.address));
}

/// A signer that cannot fund the deployment's value is refused after both accounts were read.
#[test]
fn test_an_insufficient_balance_refusal_replays() {
    let deployment = Deployment::new(constructor(&[], &runtime()), U256::from(10));
    let replay = Case::new("keyless balance", db())
        .tx(call(0, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    assert_eq!(refused(run, 0), KeylessDeployError::InsufficientBalance);
    assert!(run.record.accounts.contains_key(&deployment.signer));
    assert!(run.record.accounts.contains_key(&deployment.address));
}

/// A constructor that reverts: the call succeeds with the failure in its return, and the signer's
/// nonce bump stands from 0 to 1.
#[test]
fn test_a_reverting_constructor_replays() {
    let deployment = Deployment::new(constructor(&[PUSH0, PUSH0, REVERT], &runtime()), U256::ZERO);
    let replay = Case::new("keyless revert", db())
        .tx(call(0, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    let ret = returned(run, 0);
    assert_eq!(ret.deployedAddress, Address::ZERO);
    assert!(matches!(
        decode_error_result(&ret.errorData),
        Some(KeylessDeployError::ExecutionReverted { .. })
    ));
    assert_eq!(run.tx(0).state[&deployment.signer].info.nonce, 1, "the bump stands");
}

/// Two deployments of one signer in one block: the second finds the address taken, having read
/// the state the first left in the block's cache rather than the witness.
#[test]
fn test_a_resubmission_in_the_same_block_is_refused() {
    let deployment = Deployment::new(constructor(&[], &runtime()), U256::ZERO);
    let replay = Case::new("keyless twice", db())
        .tx(call(0, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .tx(call(1, KEYLESS_DEPLOY_ADDRESS, deployment.call_data(), GAS))
        .run();
    let run = &replay.recorded;
    assert_eq!(returned(run, 0).deployedAddress, deployment.address);
    assert_eq!(refused(run, 1), KeylessDeployError::ContractAlreadyExists);
}

/// A malformed call is refused before any read.
#[test]
fn test_a_malformed_keyless_call_reads_nothing() {
    let data: Bytes =
        IKeylessDeploy::keylessDeployCall::SELECTOR.iter().copied().chain([0xff_u8; 8]).collect();
    let replay =
        Case::new("keyless malformed", db()).tx(call(0, KEYLESS_DEPLOY_ADDRESS, data, GAS)).run();
    let run = &replay.recorded;
    assert_eq!(refused(run, 0), KeylessDeployError::MalformedEncoding);
    assert!(run.bucket_ids.is_empty());
}
