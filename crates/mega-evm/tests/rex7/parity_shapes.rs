//! REX6 ↔ REX7 bit-for-bit parity on the transaction shapes the settlement suite does not reach.
//!
//! The precision invariant is that a transaction which stays inside every per-tx limit is
//! indistinguishable under the two accounting models. `checkpoint_settlement` establishes that for
//! bytecode shapes reached from a plain call; the shapes here are the ones that enter or leave the
//! interpreter through a different door:
//!
//! - **EIP-7702 authorizations** — accounted in validate / pre-execution, before any frame exists,
//!   and able to re-derive the beneficiary detention cap from usage the checkpoint model settles
//!   differently;
//! - **`KeylessDeploy`** — a system contract intercepted at depth 0, whose sandbox runs a whole
//!   nested transaction and merges its usage back;
//! - **system-originated transactions** — exempt from per-tx metering, which also switches the
//!   clamp off entirely, so an exempt transaction must run to completion under a limit that would
//!   stop a user transaction;
//! - **the REX5 storage-call stipend** — a per-frame allowance drawn only at the storage-gas
//!   surcharge sites, which are exactly the checkpoints;
//! - **oracle hints** — metered from inside `frame_init`, one step past the CALL checkpoint.
//!
//! Every case asserts the full outcome tuple: execution result, compute gas, all four dimensions,
//! receipt `gas_used` and the detained compute-gas limit.

use std::vec::Vec;

use crate::common::{
    assert_outcomes_identical, default_envs, transact_tx, Outcome, CALLEE, CALLER, CONTRACT,
    DEFAULT_TX_GAS_LIMIT, EMPTY_TARGET, ONE_ETH,
};
use alloy_eips::eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, IOracle, MegaSpecId, TestExternalEnvs,
    KEYLESS_DEPLOY_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2,
};
use revm::{
    bytecode::opcode::{CALL, LOG1, POP, SLOAD, STOP, TIMESTAMP},
    context::{tx::TxEnvBuilder, TxEnv},
};

/// The protocol's own system caller (EIP-4788 / EIP-2935 pre-block calls). A transaction from this
/// address is system-originated, and REX6+ exempts it from per-tx metering.
const PROTOCOL_SYSTEM_CALLER: Address = address!("fffffffffffffffffffffffffffffffffffffffe");

/// The address authorizations in this file delegate to.
const DELEGATE: Address = address!("0000000000000000000000000000000000330001");
/// An authority that already exists in state.
const EXISTING_AUTHORITY: Address = address!("0000000000000000000000000000000000330002");
/// An authority that does not exist yet, so applying its authorization grows state.
const NEW_AUTHORITY: Address = address!("0000000000000000000000000000000000330003");

const KEYLESS_RELAYER: Address = address!("0000000000000000000000000000000000330004");

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// `pairs` PUSH1/POP pairs — plain opcodes that record nothing of their own.
fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// A mixed body: plain opcodes around one of every checkpoint family that does not need operands
/// from the caller — a storage read, a storage write, a log, and a volatile opcode.
fn mixed_checkpoint_body() -> Bytes {
    let builder = plain_filler(BytecodeBuilder::default(), 10)
        .append(TIMESTAMP)
        .append(POP)
        .push_u256(U256::from(3))
        .append(SLOAD)
        .append(POP)
        .sstore(U256::from(1), U256::from(0x11));
    let builder = plain_filler(builder, 10)
        .mstore(0, [0x22u8; 32])
        .push_number(0xabcu64) // topic0
        .push_number(32u64) // len
        .push_number(0u64) // offset
        .append(LOG1);
    plain_filler(builder, 10).append(STOP).build()
}

/// Runs `tx` under both specs against a freshly built database and asserts the two are
/// indistinguishable. Returns `(REX6, REX7)` for any case-specific assertions on top.
fn assert_parity(
    label: &str,
    build_db: impl Fn() -> MemoryDatabase,
    build_tx: impl Fn() -> TxEnv,
    limits: impl Fn(MegaSpecId) -> EvmTxRuntimeLimits,
) -> (Outcome, Outcome) {
    let envs6 = default_envs();
    let r6 =
        transact_tx(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6), build_tx(), &envs6);
    let envs7 = default_envs();
    let r7 =
        transact_tx(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7), build_tx(), &envs7);
    assert_outcomes_identical(label, &r6, &r7);
    (r6, r7)
}

fn recovered_auth(authority: Address, nonce: u64) -> RecoveredAuthorization {
    RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: DELEGATE, nonce },
        RecoveredAuthority::Valid(authority),
    )
}

/// EIP-7702 authorization accounting happens in `validate` / pre-execution — before the first
/// frame, and therefore before any checkpoint exists. It also charges dynamic SALT account-creation
/// gas into the transaction's intrinsic gas, which the first frame's settlement window then has to
/// open on top of. One net-new authority and one existing one exercise both arms of
/// `on_rex6_eip7702_authority_applied`.
#[test]
fn test_eip7702_authorization_accounting_matches_per_opcode() {
    let code = mixed_checkpoint_body();
    let build_db = || {
        base_db(code.clone())
            .account_balance(EXISTING_AUTHORITY, U256::from(1u64))
            .account_code(DELEGATE, BytecodeBuilder::default().append(STOP).build())
    };
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .chain_id(Some(1))
            .authorization_list_recovered(Vec::from([
                recovered_auth(EXISTING_AUTHORITY, 0),
                recovered_auth(NEW_AUTHORITY, 0),
            ]))
            .build_fill()
    };

    let (_, r7) =
        assert_parity("EIP-7702 authorizations", build_db, build_tx, EvmTxRuntimeLimits::from_spec);
    assert!(r7.is_success(), "the authorized transaction must succeed: {:?}", r7.result);
    assert!(
        r7.state_growth > 0,
        "the net-new authority must register state growth; growth={}",
        r7.state_growth
    );
}

/// The same shape with the authorizations applied under an engaged detention cap. The cap is
/// re-derived from settled usage when an applied authority is the block beneficiary, so this also
/// checks that a cap installed outside any frame lands on the same number under both models.
#[test]
fn test_eip7702_authorization_under_detention_matches_per_opcode() {
    let code = mixed_checkpoint_body();
    let build_db = || {
        base_db(code.clone())
            .account_balance(EXISTING_AUTHORITY, U256::from(1u64))
            .account_code(DELEGATE, BytecodeBuilder::default().append(STOP).build())
    };
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .chain_id(Some(1))
            .authorization_list_recovered(Vec::from([recovered_auth(NEW_AUTHORITY, 0)]))
            .build_fill()
    };
    let limits = |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = 1_000_000;
        limits
    };

    let (_, r7) = assert_parity("EIP-7702 under detention", build_db, build_tx, limits);
    assert!(r7.is_success(), "the authorized transaction must succeed: {:?}", r7.result);
}

/// Builds a deterministic pre-EIP-155 keyless deployment transaction.
fn keyless_tx_bytes(init_code: Bytes) -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: 200_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(buf)
}

/// `KeylessDeploy` is intercepted at depth 0, so the interception happens before any frame — and
/// therefore before any checkpoint — has been created. Its sandbox then runs a whole nested
/// transaction under the same spec, with its own tracker, and merges the usage back.
///
/// Two accounting models have to agree across all of that: the sandbox's own checkpoint settlement,
/// the merge, and the outer transaction's view of it.
#[test]
fn test_keyless_deploy_sandbox_accounting_matches_per_opcode() {
    // Initcode that runs some plain opcodes and a storage write, then deploys a small runtime.
    let runtime = BytecodeBuilder::default().append(STOP).build_vec();
    let init_code = plain_filler(BytecodeBuilder::default(), 10)
        .sstore(U256::from(7), U256::from(0x99))
        .return_with_data(&runtime)
        .build();
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes(init_code),
        gasLimitOverride: U256::from(1_000_000u64),
    }
    .abi_encode();

    let build_db =
        || MemoryDatabase::default().account_balance(KEYLESS_RELAYER, U256::from(10 * ONE_ETH));
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(KEYLESS_RELAYER)
            .call(KEYLESS_DEPLOY_ADDRESS)
            .gas_limit(30_000_000)
            .chain_id(Some(1))
            .data(Bytes::from(call_data.clone()))
            .build_fill()
    };

    let (_, r7) =
        assert_parity("keyless deploy sandbox", build_db, build_tx, EvmTxRuntimeLimits::from_spec);
    assert!(r7.is_success(), "the keyless deployment must succeed: {:?}", r7.result);
    let returns = IKeylessDeploy::keylessDeployCall::abi_decode_returns(
        r7.result.output().expect("the interceptor must return data"),
    )
    .expect("the output must decode as keylessDeployReturn");
    assert!(
        !returns.deployedAddress.is_zero(),
        "the sandbox must report a deployed address; errorData={}",
        returns.errorData
    );
}

/// The same keyless deployment under a detention cap engaged by the sandboxed code, so the sandbox
/// runs with a clamp of its own.
#[test]
fn test_keyless_deploy_sandbox_under_detention_matches_per_opcode() {
    let runtime = BytecodeBuilder::default().append(STOP).build_vec();
    let init_code = plain_filler(BytecodeBuilder::default(), 5)
        .append(TIMESTAMP)
        .append(POP)
        .return_with_data(&runtime)
        .build();
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes(init_code),
        gasLimitOverride: U256::from(1_000_000u64),
    }
    .abi_encode();

    let build_db =
        || MemoryDatabase::default().account_balance(KEYLESS_RELAYER, U256::from(10 * ONE_ETH));
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(KEYLESS_RELAYER)
            .call(KEYLESS_DEPLOY_ADDRESS)
            .gas_limit(30_000_000)
            .chain_id(Some(1))
            .data(Bytes::from(call_data.clone()))
            .build_fill()
    };
    let limits = |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = 500_000;
        limits
    };

    let (_, r7) = assert_parity("keyless deploy under detention", build_db, build_tx, limits);
    assert!(r7.is_success(), "the keyless deployment must succeed: {:?}", r7.result);
}

/// A system-originated transaction is exempt from per-tx metering, and the exemption also switches
/// the clamp off: `checkpoint_clamp_amount` refuses to hide anything once the tracker is not in the
/// `WithinLimit` state.
///
/// The compute limit here is far below what the transaction spends, so a user transaction running
/// the same code would be stopped. The exempt one must run to completion under both models — and
/// still report the same compute usage, since the recording continues while only the halt decision
/// is suppressed.
#[test]
fn test_system_originated_transaction_is_unclamped_under_both_models() {
    let code = {
        let mut code = Vec::new();
        code.extend_from_slice(&[0x61, 0x03, 0xe8]); // PUSH2 1000
        let target = code.len() as u8;
        code.extend_from_slice(&[0x5b, 0x60, 0x01, 0x90, 0x03, 0x80, 0x60, target, 0x57, 0x00]);
        Bytes::from(code)
    };
    let build_db = || {
        MemoryDatabase::default()
            .account_code(CONTRACT, code.clone())
            .account_balance(PROTOCOL_SYSTEM_CALLER, U256::from(ONE_ETH))
    };
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(PROTOCOL_SYSTEM_CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .gas_price(0)
            .build_fill()
    };
    // Well under the loop's cost: binding for a user transaction, ignored for this one.
    let limits = |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(25_000);

    let (_, r7) = assert_parity("system-originated exemption", build_db, build_tx, limits);
    assert!(r7.is_success(), "an exempt transaction must not be stopped: {:?}", r7.result);
    assert!(
        r7.compute_gas > 25_000,
        "the exempt transaction must have spent past the limit it ignores; compute={}",
        r7.compute_gas
    );
}

/// The REX5 storage-call stipend is a per-frame allowance drawn only at `MegaETH`'s storage-gas
/// surcharge sites — which are exactly the checkpoints. A value-transferring internal CALL into a
/// callee that logs and writes storage draws on it at three of them.
///
/// Under checkpoint accounting the same sites do the drawing, but the compute they record is now a
/// segment delta rather than a per-opcode capture, so the subtraction of the drawn storage gas has
/// to land on the same number.
#[test]
fn test_storage_call_stipend_allowance_matches_per_opcode() {
    // The callee is reached by a value-transferring CALL with no gas of its own beyond the stipend
    // revm adds, so its storage work is paid for out of the allowance.
    let callee = BytecodeBuilder::default()
        .mstore(0, [0x33u8; 32])
        .push_number(0xdefu64) // topic0
        .push_number(32u64) // len
        .push_number(0u64) // offset
        .append(LOG1)
        .append(STOP)
        .build();
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(1u64) // value — arms the stipend
        .push_address(CALLEE)
        .push_number(0u64) // gas — only the stipend is available
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .build_fill()
    };

    let (_, r7) =
        assert_parity("storage-call stipend", build_db, build_tx, EvmTxRuntimeLimits::from_spec);
    assert!(r7.is_success(), "the stipend-funded call must succeed: {:?}", r7.result);
    assert_eq!(
        r7.result.logs().len(),
        1,
        "the callee's log must have been emitted out of the stipend allowance; logs={:?}",
        r7.result.logs()
    );
}

/// The stipend's other arm: a value transfer to an account that does not exist yet, so the
/// new-account materialisation surcharge is what draws on the allowance.
#[test]
fn test_storage_call_stipend_new_account_matches_per_opcode() {
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(1u64) // value
        .push_address(EMPTY_TARGET)
        .push_number(0u64) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .build_fill()
    };

    let (_, r7) = assert_parity(
        "storage-call stipend / new account",
        || base_db(code.clone()),
        build_tx,
        EvmTxRuntimeLimits::from_spec,
    );
    assert!(r7.is_success(), "the value transfer must succeed: {:?}", r7.result);
}

/// The oracle-hint site on its success arm: the payload is metered into the data-size lane from
/// inside `frame_init`, then forwarded to the backend, then the inner Oracle frame runs.
///
/// Under checkpoint accounting the caller's segment was settled at the CALL checkpoint one step
/// earlier, so both the metering and the forwarding observe the same state they observe under
/// per-opcode accounting — and the hint that reaches the backend has to be identical.
#[test]
fn test_oracle_hint_forwarding_matches_per_opcode() {
    let payload = Bytes::from(vec![0xa5u8; 96]);
    let topic = B256::repeat_byte(0x5a);
    let calldata = IOracle::sendHintCall { topic, data: payload.clone() }.abi_encode();
    let len = calldata.len() as u64;
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .mstore(0, &calldata)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(len) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_db =
        || base_db(code.clone()).account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);
    let tx = || {
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(DEFAULT_TX_GAS_LIMIT)
            .build_fill()
    };

    let envs6 = TestExternalEnvs::new();
    let r6 = transact_tx(
        MegaSpecId::REX6,
        build_db(),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6),
        tx(),
        &envs6,
    );
    let envs7 = TestExternalEnvs::new();
    let r7 = transact_tx(
        MegaSpecId::REX7,
        build_db(),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
        tx(),
        &envs7,
    );

    let hints6 = envs6.recorded_hints();
    let hints7 = envs7.recorded_hints();
    assert_eq!(hints7.len(), 1, "the hint must have reached the backend; got {hints7:?}");
    assert_eq!(hints6, hints7, "the forwarded hint must be identical under both models");
    assert_eq!(hints7[0].data, payload, "the payload must survive intact");
    assert_outcomes_identical("oracle hint forwarding", &r6, &r7);
}
