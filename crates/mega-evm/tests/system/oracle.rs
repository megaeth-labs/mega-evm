//! The Oracle's hint path: which calls reach `OracleEnv::on_hint`, what they cost the
//! transaction's data size, and which do not reach it at all.

use core::convert::Infallible;

use alloy_evm::Evm;
use alloy_primitives::{Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, ORACLE_CONTRACT_ADDRESS},
    test_utils::{zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    ExternalEnvs, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    RecordedHint, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, STATICCALL},
    context::result::ResultAndState,
};

use crate::common::{block, call_tx, calls_with, run, system_db, with_contract, CALLER, CONTRACT};

/// A topic and payload the tests send.
const TOPIC: B256 = B256::repeat_byte(0x7a);

/// The calldata of `sendHint(TOPIC, data)`.
fn send_hint(data: &[u8]) -> Bytes {
    Bytes::from(
        IOracle::sendHintCall { topic: TOPIC, data: Bytes::copy_from_slice(data) }.abi_encode(),
    )
}

/// Runs `tx` against an oracle environment that records the hints it receives, and returns what
/// the transaction produced, the hints and what the common execution layer counted.
fn run_with_oracle(
    db: MemoryDatabase,
    tx: MegaTransaction,
) -> (ResultAndState<MegaHaltReason>, Vec<RecordedHint>, LimitUsage) {
    let envs = TestExternalEnvs::<Infallible>::new();
    let ctx = MegaContext::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs::from(envs.clone()),
    )
    .with_block(block())
    .with_chain(zero_fee_l1_block_info());
    let mut evm = MegaEvm::new(ctx);
    let result = evm.transact_raw(tx).expect("the transaction is valid");
    let usage = evm.ctx().additional_limit().usage();
    (result, envs.recorded_hints(), usage)
}

/// A `sendHint` transaction reaches the oracle service with the caller, the topic and the
/// payload, and the call itself succeeds through the contract's own bytecode.
#[test]
fn test_send_hint_reaches_the_oracle_service() {
    let payload = b"a hint".to_vec();
    let (result, hints, _) = run_with_oracle(
        system_db(),
        call_tx(ORACLE_CONTRACT_ADDRESS, send_hint(&payload), U256::ZERO),
    );

    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(
        hints,
        vec![RecordedHint { from: CALLER, topic: TOPIC, data: Bytes::from(payload) }],
    );
}

/// The bytes the node materialised for the hint are counted on the transaction's data size,
/// trailing bytes the decoder ignores included.
#[test]
fn test_the_hint_payload_is_counted_on_the_transaction() {
    let data = send_hint(b"a hint");
    let (_, hints, usage) =
        run_with_oracle(system_db(), call_tx(ORACLE_CONTRACT_ADDRESS, data.clone(), U256::ZERO));
    assert_eq!(hints.len(), 1);
    assert_eq!(usage.data_size, data.len() as u64);

    // The same call with bytes the ABI decoder drops pays for them too.
    let padded: Vec<u8> = data.iter().copied().chain([0_u8; 64]).collect();
    let (_, hints, padded_usage) =
        run_with_oracle(system_db(), call_tx(ORACLE_CONTRACT_ADDRESS, &padded, U256::ZERO));
    assert_eq!(hints.len(), 1, "the trailing bytes do not stop the decoding");
    assert_eq!(padded_usage.data_size, padded.len() as u64);
}

/// A payload that does not decode is paid for and forwards nothing.
#[test]
fn test_a_malformed_payload_is_paid_for_and_not_forwarded() {
    let data: Vec<u8> =
        IOracle::sendHintCall::SELECTOR.iter().copied().chain([0xff_u8; 8]).collect();
    let (_, hints, usage) =
        run_with_oracle(system_db(), call_tx(ORACLE_CONTRACT_ADDRESS, &data, U256::ZERO));
    assert!(hints.is_empty(), "a payload that does not decode carries no hint");
    assert_eq!(usage.data_size, data.len() as u64);
}

/// A `STATICCALL` forwards the hint: `sendHint` is a view method.
#[test]
fn test_a_static_call_forwards_the_hint() {
    let data = send_hint(b"static");
    let code = calls_with(STATICCALL, ORACLE_CONTRACT_ADDRESS, &data, 0);
    let (result, hints, _) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    assert!(result.result.is_success());
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].from, CONTRACT, "the frame that called is the sender of the hint");
}

/// A call that carries value forwards nothing: `sendHint` is not payable, so the call reverts
/// in the bytecode and the hint would be one the caller never sent.
#[test]
fn test_a_value_bearing_call_forwards_nothing() {
    let data = send_hint(b"paid");
    let code = calls_with(CALL, ORACLE_CONTRACT_ADDRESS, &data, 1);
    let (result, hints, usage) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));

    let outcome = result.result.output().cloned().unwrap_or_default();
    let (status, _) = crate::common::split_outcome(&outcome);
    assert!(!status, "the bytecode refuses the value");
    assert!(hints.is_empty());
    assert_eq!(usage.data_size, 0, "nothing was materialised for a hint that is not forwarded");
}

/// A call forwarded no gas forwards nothing: it cannot run the contract either, so its hint
/// would be a free message to the service.
#[test]
fn test_a_call_without_gas_forwards_nothing() {
    use revm::bytecode::opcode::{POP, STOP};

    let data = send_hint(b"free");
    let code = BytecodeBuilder::default()
        .mstore(0x0, &data)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(0_u64) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let (result, hints, usage) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));

    assert!(result.result.is_success(), "the caller survives the failed call");
    assert!(hints.is_empty());
    assert_eq!(usage.data_size, 0);
}

/// `CALLCODE` and `DELEGATECALL` never reach the interceptor, so they forward nothing.
#[test]
fn test_callcode_and_delegatecall_forward_nothing() {
    let data = send_hint(b"scheme");
    for scheme in [CALLCODE, DELEGATECALL] {
        let code = calls_with(scheme, ORACLE_CONTRACT_ADDRESS, &data, 0);
        let (_, hints, usage) =
            run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
        assert!(hints.is_empty(), "scheme {scheme:#x} must not reach the interceptor");
        assert_eq!(usage.data_size, 0);
    }
}

/// A selector the Oracle's interceptor does not know is not intercepted: the call runs the
/// contract's bytecode, which reverts on an unknown selector.
#[test]
fn test_an_unknown_selector_runs_the_bytecode() {
    let (result, hints, usage) = run_with_oracle(
        system_db(),
        call_tx(ORACLE_CONTRACT_ADDRESS, [0xde, 0xad, 0xbe, 0xef], U256::ZERO),
    );
    assert!(!result.result.is_success(), "the Oracle has no such method");
    assert!(hints.is_empty());
    assert_eq!(usage.data_size, 0);
}

/// Reading the Oracle's storage is not intercepted: `getSlot` runs the contract's code.
#[test]
fn test_reading_a_slot_runs_the_bytecode() {
    let db = system_db().account_storage(ORACLE_CONTRACT_ADDRESS, U256::from(3), U256::from(42));
    let data = IOracle::getSlotCall { slot: U256::from(3) }.abi_encode();
    let result = run(db, call_tx(ORACLE_CONTRACT_ADDRESS, &data, U256::ZERO));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(
        IOracle::getSlotCall::abi_decode_returns(result.result.output().unwrap()).unwrap(),
        B256::from(U256::from(42)),
    );
}
