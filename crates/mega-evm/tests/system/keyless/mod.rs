//! `KeylessDeploy`: the dispatch of `keylessDeploy`, the rules a deployment is held to, and the
//! native creation it runs as — what it deploys, what it is charged, the limits it is held to and
//! what an inspector sees of it.
//!
//! Every case that depends on the pools runs at a gas limit under the execution cap, where the
//! transaction has no state-gas reservoir, and at one above it, where the reservoir pays the state
//! and history gas first ([`GAS_LIMITS`]).

mod charges;
mod deploy;
mod detention;
mod differential;
mod dispatch;
mod frame;
mod inspector;
mod limits;
mod precedence;
mod rules;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    constants::TX_GAS_LIMIT_CAP,
    satin_gas_params,
    system::keyless::{
        decode_error_result, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
    },
    test_utils::MemoryDatabase,
    BucketId, EvmTxRuntimeLimits, ExternalEnvs, MegaContext, MegaEvm, MegaSpecId,
    MegaTransactionOutcome, SaltEnv, TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{
    bytecode::opcode::{CODECOPY, PUSH0, RETURN},
    context::result::ExecutionResult,
    context_interface::cfg::GasId,
};

use crate::common::{block, call_tx, context, system_db};

/// A gas limit under the execution cap, and one above it.
pub(crate) const GAS_LIMITS: [u64; 2] = [TX_GAS_LIMIT_CAP / 4, TX_GAS_LIMIT_CAP * 2];

/// The gas limits a transaction carrying `calldata_len` bytes of calldata runs at: three quarters
/// of the execution cap, below it, where the body's history is paid out of regular gas; and ten
/// times the cap, above it, where the reservoir pays it.
///
/// The tier below the cap is left out where the body's history at the byte prices in effect leaves
/// it less than 25,000,000 and two new accounts for the rest: no transaction that large can pay for
/// its own body below the cap there.
pub(crate) fn tiers_carrying(calldata_len: usize) -> Vec<u64> {
    let below = TX_GAS_LIMIT_CAP * 3 / 4;
    let room = 25_000_000 + 2 * crate::common::account_state_gas();
    let fits = crate::common::body_history(calldata_len as u64) + room <= below;
    [below, 10 * TX_GAS_LIMIT_CAP].into_iter().filter(|&tier| fits || tier > below).collect()
}

/// The gas limit the tests sign their deployments with.
pub(crate) const SIGNED_GAS_LIMIT: u64 = 1_000_000;

/// A `gasLimitOverride` above what any transaction here can forward, so the forward is capped
/// to what the call has left.
pub(crate) const LARGE_OVERRIDE: u64 = 10_000_000_000;

/// The gas price the tests sign their deployments at.
pub(crate) const SIGNED_GAS_PRICE: u128 = 100_000_000_000;

/// A pre-EIP-155 signed creation, as Nick's Method makes one: a fixed signature (`r = s =
/// 0x2222…22`, `v = 27`) over a transaction nobody holds the key of, so the signer is whatever the
/// transaction's contents recover to.
#[derive(Clone, Debug)]
pub(crate) struct Deployment {
    /// The signed transaction, RLP-encoded: `keylessDeploymentTransaction`.
    pub(crate) tx: Bytes,
    /// Its signer.
    pub(crate) signer: Address,
    /// Where it deploys: the signer's first creation address.
    pub(crate) address: Address,
}

impl Deployment {
    /// A deployment of `init_code` with no value, signed at nonce 0.
    pub(crate) fn new(init_code: Bytes) -> Self {
        Self::signed(0, SIGNED_GAS_LIMIT, U256::ZERO, init_code)
    }

    /// A deployment of `init_code` carrying `value`, signed at nonce 0.
    pub(crate) fn with_value(init_code: Bytes, value: U256) -> Self {
        Self::signed(0, SIGNED_GAS_LIMIT, value, init_code)
    }

    /// A deployment signed with every field chosen.
    pub(crate) fn signed(nonce: u64, gas_limit: u64, value: U256, init_code: Bytes) -> Self {
        let tx = TxLegacy {
            nonce,
            gas_price: SIGNED_GAS_PRICE,
            gas_limit,
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

    /// The calldata of a `keylessDeploy` call deploying this, with `gas_limit_override`.
    pub(crate) fn call_data(&self, gas_limit_override: u64) -> Bytes {
        keyless_deploy_call(&self.tx, U256::from(gas_limit_override))
    }
}

/// The calldata of a `keylessDeploy` call carrying `tx` and `gas_limit_override`.
pub(crate) fn keyless_deploy_call(tx: &[u8], gas_limit_override: U256) -> Bytes {
    IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::copy_from_slice(tx),
        gasLimitOverride: gas_limit_override,
    }
    .abi_encode()
    .into()
}

/// Init code that deploys `runtime`.
pub(crate) fn deploying(runtime: &[u8]) -> Bytes {
    constructor(&[], runtime)
}

/// Init code that runs `prefix`, then deploys `runtime`, which it copies from its own tail.
pub(crate) fn constructor(prefix: &[u8], runtime: &[u8]) -> Bytes {
    let len = u8::try_from(runtime.len()).expect("a short runtime");
    let tail = u16::try_from(prefix.len() + 11).expect("a short prefix").to_be_bytes();
    let mut code = prefix.to_vec();
    code.extend_from_slice(&[0x60, len, 0x61, tail[0], tail[1], PUSH0, CODECOPY]);
    code.extend_from_slice(&[0x60, len, PUSH0, RETURN]);
    code.extend_from_slice(runtime);
    code.into()
}

/// A runtime of `len` bytes that stops.
pub(crate) fn runtime(len: usize) -> Vec<u8> {
    vec![0x00; len]
}

/// A database holding the system contracts, the relayer's balance and `balance` on the signer of
/// `deployment`.
pub(crate) fn db_for(deployment: &Deployment, balance: U256) -> MemoryDatabase {
    let db = system_db();
    if balance.is_zero() {
        return db;
    }
    db.account_balance(deployment.signer, balance)
}

/// Runs the `keylessDeploy` transaction `data` from the relayer over `db` at `gas_limit`, under
/// `limits`.
pub(crate) fn run_with(
    db: MemoryDatabase,
    data: Bytes,
    gas_limit: u64,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    run_nth(db, data, gas_limit, limits, 0)
}

/// [`run_with`], as the relayer's transaction at `nonce`.
pub(crate) fn run_nth(
    db: MemoryDatabase,
    data: Bytes,
    gas_limit: u64,
    limits: EvmTxRuntimeLimits,
    nonce: u64,
) -> MegaTransactionOutcome {
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    tx.0.base.nonce = nonce;
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(tx)
        .expect("the transaction is valid")
}

/// Runs a `keylessDeploy` of `deployment` over `db` at `gas_limit`, with no runtime limit.
pub(crate) fn deploy(
    db: MemoryDatabase,
    deployment: &Deployment,
    gas_limit: u64,
) -> MegaTransactionOutcome {
    run_with(db, deployment.call_data(LARGE_OVERRIDE), gas_limit, EvmTxRuntimeLimits::no_limits())
}

/// What a `keylessDeploy` call that succeeded returned.
pub(crate) fn returned(outcome: &MegaTransactionOutcome) -> IKeylessDeploy::keylessDeployReturn {
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("the call did not succeed: {:?}", outcome.result);
    };
    IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
        .expect("a keylessDeploy return")
}

/// The error a deployment that ran and failed reports in `errorData`.
pub(crate) fn failure(outcome: &MegaTransactionOutcome) -> KeylessDeployError {
    let ret = returned(outcome);
    assert_eq!(ret.deployedAddress, Address::ZERO, "a failed deployment deploys nothing");
    decode_error_result(&ret.errorData).expect("errorData is a keyless deploy error")
}

/// The error a `keylessDeploy` call reverted with.
pub(crate) fn refusal(outcome: &MegaTransactionOutcome) -> KeylessDeployError {
    let ExecutionResult::Revert { output, .. } = &outcome.result else {
        panic!("the call did not revert: {:?}", outcome.result);
    };
    decode_error_result(output).expect("the revert data is a keyless deploy error")
}

/// The nonce `address` ends the transaction with; zero when the transaction did not touch it.
pub(crate) fn nonce(outcome: &MegaTransactionOutcome, address: Address) -> u64 {
    outcome.state.get(&address).map_or(0, |account| account.info.nonce)
}

/// The code hash `address` ends the transaction with, if the transaction touched it.
pub(crate) fn code_hash(outcome: &MegaTransactionOutcome, address: Address) -> Option<B256> {
    outcome.state.get(&address).map(|account| account.info.code_hash)
}

/// An account with no code: the target of the reference transactions.
pub(crate) const NOBODY: Address = address!("0x0000000000000000000000000000000000c0ffee");

/// The same calldata sent to [`NOBODY`] at the same gas limit: a transaction that pays the
/// intrinsic gas a `keylessDeploy` transaction carrying `data` pays, and nothing else.
pub(crate) fn reference(data: Bytes, gas_limit: u64) -> MegaTransactionOutcome {
    let mut tx = call_tx(NOBODY, data, U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    MegaEvm::new(context(system_db())).execute_transaction(tx).expect("the transaction is valid")
}

/// What `outcome` spent beyond `reference`: `[total, regular, state, history, history bytes]`.
pub(crate) fn beyond(
    outcome: &MegaTransactionOutcome,
    reference: &MegaTransactionOutcome,
) -> [u64; 5] {
    let total = |o: &MegaTransactionOutcome| o.result.gas().total_gas_spent();
    [
        total(outcome) - total(reference),
        outcome.gas.regular - reference.gas.regular,
        outcome.gas.state - reference.gas.state,
        outcome.gas.history - reference.gas.history,
        outcome.gas.history_bytes - reference.gas.history_bytes,
    ]
}

/// The schedule's entry `id`.
pub(crate) fn entry(id: GasId) -> u64 {
    satin_gas_params().get(id)
}

/// The regular gas the `CREATE` opcode charges its frame for init code of `len` bytes, which a
/// `keylessDeploy` call pays on top of its overhead: the schedule's `create` entry and EIP-3860's
/// cost per word.
pub(crate) fn create_regular(len: usize) -> u64 {
    satin_gas_params().create_cost() + satin_gas_params().initcode_cost(len)
}

/// The history gas of one write record, `WRITE_RECORD_SIZE` bytes, by hand.
pub(crate) fn record() -> u64 {
    history(mega_evm::WRITE_RECORD_SIZE)
}

/// The history gas of `bytes` bytes, by hand.
pub(crate) fn history(bytes: u64) -> u64 {
    crate::common::history(bytes)
}

/// A Satin context over `db` reading `envs`.
pub(crate) fn salt_run(
    db: MemoryDatabase,
    envs: TestExternalEnvs<String>,
    deployment: &Deployment,
    gas_limit: u64,
) -> MegaTransactionOutcome {
    let context = MegaContext::<_, TestExternalEnvs<String>>::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs { salt_env: envs.clone(), oracle_env: envs },
    )
    .with_block(block())
    .with_chain(mega_evm::test_utils::zero_fee_l1_block_info());
    let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, deployment.call_data(LARGE_OVERRIDE), U256::ZERO);
    tx.0.base.gas_limit = gas_limit;
    MegaEvm::new(context).execute_transaction(tx).expect("the transaction is valid")
}

/// The bucket `account`'s own state lives in.
pub(crate) fn bucket(account: Address) -> BucketId {
    <TestExternalEnvs<String> as SaltEnv>::bucket_id_for_account(account)
}

/// `envs` with `account`'s bucket at multiplier `m`.
pub(crate) fn crowded(
    envs: TestExternalEnvs<String>,
    account: Address,
    m: u64,
) -> TestExternalEnvs<String> {
    envs.with_bucket_capacity(bucket(account), MIN_BUCKET_SIZE as u64 * m)
}
