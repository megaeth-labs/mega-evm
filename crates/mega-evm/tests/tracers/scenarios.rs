//! The ten Satin scenarios the tracer goldens pin.
//!
//! Each scenario is a standalone test so a mismatch names the case. Relationship checks that
//! would pin a tracer bug as if it were required behaviour are written as assertions; if one
//! fails, the golden still records the current output and the failure is a suspected issue for
//! the report, not an engine change.

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::{
        keyless::{
            decode_error_result, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
            KEYLESS_DEPLOY_CODE,
        },
        ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaTransaction,
};
use revm::{
    bytecode::opcode::{
        CALL, CODECOPY, GAS as GAS_OP, JUMP, JUMPDEST, POP, PUSH0, RETURN, REVERT, TIMESTAMP,
    },
    context::{result::ExecutionResult, TxEnv},
};

use crate::harness::{
    assert_call_gas_matches_receipt, assert_keyless_steps,
    assert_keyless_struct_logs_miss_the_creation, assert_limit_stop_outputs,
    assert_prestate_covers_touched, assert_root_gas_is_the_gas_limit, at_spec_prices, call_tx,
    create_tx, pin_tracer_views, Traced,
};

const CALLER: Address = address!("0x0000000000000000000000000000000000400000");
const PAYEE: Address = address!("0x0000000000000000000000000000000000400001");
const CONTRACT: Address = address!("0x0000000000000000000000000000000000400002");
const CHILD: Address = address!("0x0000000000000000000000000000000000400003");

/// Room for the small programs these scenarios run, below the execution cap.
const TX_GAS: u64 = 5_000_000;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
}

fn pin(name: &str, traced: &Traced) {
    if at_spec_prices() {
        pin_tracer_views(name, traced);
    }
    assert_call_gas_matches_receipt(traced);
    assert_prestate_covers_touched(traced);
    assert_root_gas_is_the_gas_limit(traced);
}

/// Ordinary ETH transfer. The 7708 transfer log is what `call_with_log` pins.
#[test]
fn test_eth_transfer_emits_7708_log() {
    let traced = Traced::run(
        funded(),
        call_tx(CALLER, PAYEE, Bytes::new(), U256::from(1), TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    pin("eth_transfer", &traced);
}

/// A call that writes a fresh storage slot, so the receipt carries state gas.
#[test]
fn test_sstore_new_slot_charges_state_gas() {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build();
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    assert!(traced.outcome.gas.state > 0, "a fresh slot costs state gas");
    pin("sstore_new_slot", &traced);
}

/// A top-level CREATE that deploys empty runtime code.
#[test]
fn test_create_deploys_empty_runtime() {
    let init = Bytes::from_static(&[PUSH0, PUSH0, RETURN]);
    let traced =
        Traced::run(funded(), create_tx(CALLER, init, TX_GAS), EvmTxRuntimeLimits::no_limits());
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    pin("create", &traced);
}

/// Nested call whose inner frame reverts.
#[test]
fn test_nested_call_inner_reverts() {
    let child = Bytes::from_static(&[PUSH0, PUSH0, REVERT]);
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .append(GAS_OP)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let traced = Traced::run(
        funded().account_code(CONTRACT, parent).account_code(CHILD, child),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(
        traced.outcome.result.is_success(),
        "the outer frame succeeds: {:?}",
        traced.outcome.result
    );
    pin("nested_inner_revert", &traced);
}

fn system_db() -> MemoryDatabase {
    funded()
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE)
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
}

/// A pre-EIP-155 signed creation, as Nick's Method makes one.
struct Deployment {
    tx: Bytes,
    /// The address the signature recovers to.
    signer: Address,
}

impl Deployment {
    fn signed(value: U256, init_code: Bytes) -> Self {
        let tx = mega_evm::alloy_consensus::TxLegacy {
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
        let signed = mega_evm::alloy_consensus::Signed::new_unchecked(
            tx,
            Signature::new(word, word, false),
            B256::ZERO,
        );
        let mut encoded = Vec::new();
        signed.rlp_encode(&mut encoded);
        let signer = signed.recover_signer().expect("Nick's-Method signature recovers");
        Self { tx: encoded.into(), signer }
    }

    fn call_data(&self) -> Bytes {
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: self.tx.clone(),
            gasLimitOverride: U256::from(10_000_000_000_u64),
        }
        .abi_encode()
        .into()
    }
}

/// The runtime the creation scenarios deploy: it returns nothing.
fn runtime() -> Bytes {
    BytecodeBuilder::default().return_empty().build()
}

/// Init code that copies [`runtime`] from its own tail and deposits it.
fn deploying_runtime() -> Bytes {
    let runtime = runtime();
    let len = u8::try_from(runtime.len()).expect("a short runtime");
    let prefix = BytecodeBuilder::default()
        .push_number(len)
        .push_number(0_u16)
        .append(PUSH0)
        .append(CODECOPY)
        .push_number(len)
        .append(PUSH0)
        .append(RETURN);
    let tail = u16::try_from(prefix.len()).expect("a short prefix");
    BytecodeBuilder::default()
        .push_number(len)
        .push_number(tail)
        .append(PUSH0)
        .append(CODECOPY)
        .push_number(len)
        .append(PUSH0)
        .append(RETURN)
        .append_many(runtime)
        .build()
}

fn keyless_tx(data: Bytes) -> MegaTransaction {
    call_tx(CALLER, KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO, TX_GAS)
}

/// A `keylessDeploy` that deploys: the answer names the signer's first creation address and no
/// error, and that address holds the runtime afterwards.
#[test]
fn test_keyless_deploy_succeeds() {
    let deployment = Deployment::signed(U256::ZERO, deploying_runtime());
    let traced = Traced::run(
        system_db(),
        keyless_tx(deployment.call_data()),
        EvmTxRuntimeLimits::no_limits(),
    );
    let ExecutionResult::Success { output, .. } = &traced.outcome.result else {
        panic!("the keyless call succeeds: {:?}", traced.outcome.result);
    };
    let answer = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
        .expect("a keylessDeploy answer");
    let deployed = deployment.signer.create(0);
    assert_ne!(deployed, Address::ZERO);
    assert_eq!(answer.deployedAddress, deployed, "the answer names the deploy address");
    assert!(answer.errorData.is_empty(), "a deployment that deployed reports no error");
    let code =
        traced.outcome.state[&deployed].info.code.as_ref().expect("the code is in the state");
    assert_eq!(code.original_bytes(), runtime(), "the deploy address holds the runtime");
    // Below the execution cap the transaction has no reservoir, so every charge the creation
    // made, its code deposit's state and history gas included, drew its regular gas: the
    // creation frame's spent gas, as the tracer measured it, is what it spent from both pools.
    let creation = &traced.inspector.traces().nodes()[1].trace;
    assert_eq!(answer.gasUsed, creation.gas_used, "gasUsed is the creation frame's spend");
    pin("keyless_success", &traced);
    assert_keyless_steps(&traced, true);
    assert_keyless_struct_logs_miss_the_creation(&traced);
}

/// A `keylessDeploy` the rules refuse: value with an unfunded signer. No creation starts.
#[test]
fn test_keyless_deploy_refused() {
    let deployment = Deployment::signed(U256::from(1), deploying_runtime());
    let traced = Traced::run(
        system_db(),
        keyless_tx(deployment.call_data()),
        EvmTxRuntimeLimits::no_limits(),
    );
    let ExecutionResult::Revert { output, .. } = &traced.outcome.result else {
        panic!("the keyless call reverts: {:?}", traced.outcome.result);
    };
    assert_eq!(
        decode_error_result(output),
        Some(KeylessDeployError::InsufficientBalance),
        "the signer cannot fund the carried value"
    );
    pin("keyless_refused", &traced);
    assert_keyless_steps(&traced, false);
}

/// A transaction-level data-size limit crossed by the body: revert-class stop.
#[test]
fn test_data_size_limit_stop() {
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100);
    let traced = Traced::run(
        funded().account_code(CONTRACT, BytecodeBuilder::default().stop().build()),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        limits,
    );
    assert!(traced.outcome.limit_exceeded.is_some(), "{:?}", traced.outcome.result);
    pin("data_size_stop", &traced);
    assert_limit_stop_outputs(&traced);
}

/// Reads `TIMESTAMP` then burns compute past a tiny detention cap.
#[test]
fn test_timestamp_detention_stop() {
    // TIMESTAMP, POP, then an infinite JUMPDEST/PUSH/JUMP loop. The detention cap is small so
    // the golden stays short.
    let code = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .append(JUMPDEST)
        .push_number(2_u64)
        .append(JUMP)
        .build();
    let limits = EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(80);
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        limits,
    );
    assert!(traced.outcome.limit_exceeded.is_some(), "{:?}", traced.outcome.result);
    pin("timestamp_detention", &traced);
    assert_limit_stop_outputs(&traced);
}

/// Gas limit above the 200M execution cap, so a fresh slot is paid from the reservoir first.
#[test]
fn test_reservoir_pays_state_gas() {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build();
    let gas_limit = TX_GAS_LIMIT_CAP + 50_000_000;
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, gas_limit),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    assert!(traced.outcome.gas.state > 0);
    pin("reservoir_sstore", &traced);
}

/// A call to the identity precompile.
#[test]
fn test_identity_precompile() {
    let precompile = Address::with_last_byte(4);
    let traced = Traced::run(
        funded(),
        call_tx(
            CALLER,
            precompile,
            Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            U256::ZERO,
            TX_GAS,
        ),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    pin("identity_precompile", &traced);
}

/// An OP deposit that calls a contract which stops.
#[test]
fn test_deposit_transaction() {
    let mut tx = op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit: TX_GAS,
        ..Default::default()
    });
    tx.deposit.source_hash = B256::repeat_byte(0x42);
    let traced = Traced::run(
        funded().account_code(CONTRACT, BytecodeBuilder::default().stop().build()),
        mega_evm::alloy_op_evm::OpTx(tx),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    pin("deposit", &traced);
}
