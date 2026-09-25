//! The Oracle's hint path: which calls reach `OracleEnv::on_hint`, what they cost the
//! transaction's data size, and which do not reach it at all.

use core::convert::Infallible;

use alloy_primitives::{Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::{IOracle, ORACLE_CONTRACT_ADDRESS},
    test_utils::{zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, ExternalEnvs, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaLimitExceeded, MegaSpecId, MegaTransaction, MegaTransactionOutcome,
    RecordedHint, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, STATICCALL},
    context::result::ResultAndState,
};

use crate::common::{block, call_tx, calls_with, run, system_db, with_contract, CALLER, CONTRACT};

/// A topic and payload the tests send.
const TOPIC: B256 = B256::repeat_byte(0x7a);

/// The hint the two data-size cases send, so the limit they set is the same number.
const METERED_HINT: &[u8] = b"a hint the transaction is metered for";

/// A top-level `sendHint`: the body counts the calldata, and the hint counts it again.
fn top_level_hint(payload: u64) -> u64 {
    mega_evm::TX_BODY_SIZE + payload + payload
}

/// A nested hint. The transaction's own calldata is empty, so only the body and the hint count.
fn nested_hint(payload: u64) -> u64 {
    mega_evm::TX_BODY_SIZE + payload
}

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
    let (outcome, hints) = run_with_oracle_under(db, tx, EvmTxRuntimeLimits::no_limits());
    (outcome.result_and_state, hints, outcome.usage)
}

/// [`run_with_oracle`] under `limits`, returning the whole outcome: the stop a crossed limit
/// latched is what the outcome reports, not the output alone.
fn run_with_oracle_under(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> (MegaTransactionOutcome, Vec<RecordedHint>) {
    let envs = TestExternalEnvs::<Infallible>::new();
    let ctx = MegaContext::new_with_external_envs(
        db,
        MegaSpecId::SATIN,
        ExternalEnvs::from(envs.clone()),
    )
    .with_block(block())
    .with_chain(zero_fee_l1_block_info())
    .with_tx_runtime_limits(limits);
    let outcome = MegaEvm::new(ctx).execute_transaction(tx).expect("the transaction is valid");
    (outcome, envs.recorded_hints())
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
    assert_eq!(usage.data_size, top_level_hint(data.len() as u64));

    // The same call with bytes the ABI decoder drops pays for them too.
    let padded: Vec<u8> = data.iter().copied().chain([0_u8; 64]).collect();
    let (_, hints, padded_usage) =
        run_with_oracle(system_db(), call_tx(ORACLE_CONTRACT_ADDRESS, &padded, U256::ZERO));
    assert_eq!(hints.len(), 1, "the trailing bytes do not stop the decoding");
    assert_eq!(padded_usage.data_size, top_level_hint(padded.len() as u64));
}

/// A payload that does not decode is paid for and forwards nothing.
#[test]
fn test_a_malformed_payload_is_paid_for_and_not_forwarded() {
    let data: Vec<u8> =
        IOracle::sendHintCall::SELECTOR.iter().copied().chain([0xff_u8; 8]).collect();
    let (_, hints, usage) =
        run_with_oracle(system_db(), call_tx(ORACLE_CONTRACT_ADDRESS, &data, U256::ZERO));
    assert!(hints.is_empty(), "a payload that does not decode carries no hint");
    assert_eq!(usage.data_size, top_level_hint(data.len() as u64));
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
    assert_eq!(
        usage.data_size,
        nested_hint(0),
        "nothing was materialised for a hint that is not forwarded"
    );
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
    assert_eq!(usage.data_size, nested_hint(0));
}

/// A call forwarded one gas does forward its hint, and the frame it was sent from then runs out
/// of gas: positive gas is what admits a hint, not a promise that the bytecode can run. The hint
/// is a synchronous side effect, so the service holds it whatever the frame does next.
#[test]
fn test_a_call_with_one_gas_forwards_the_hint_it_cannot_deliver() {
    use revm::bytecode::opcode::{MSTORE, RETURN};

    let data = send_hint(b"one gas");
    let code = BytecodeBuilder::default()
        .mstore(0x0, &data)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_u64) // gas: enough to admit the hint, not to run the bytecode
        .append(CALL)
        // memory[0..32] = the call's status, and return it
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();
    let (result, hints, usage) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));

    assert!(result.result.is_success(), "the caller survives the failed call");
    let outcome = result.result.output().cloned().unwrap_or_default();
    let (status, _) = crate::common::split_outcome(&outcome);
    assert!(!status, "one gas does not run the contract's dispatcher");
    assert_eq!(hints.len(), 1, "the hint reached the service before the frame ran out of gas");
    assert_eq!(usage.data_size, nested_hint(data.len() as u64), "and its bytes stay counted");
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
        assert_eq!(usage.data_size, nested_hint(0));
    }
}

/// A selector the Oracle's interceptor does not know is not intercepted: the call runs the
/// contract's bytecode, which reverts with empty data on a selector it does not declare — the
/// Oracle carries no fallback of its own.
#[test]
fn test_an_unknown_selector_runs_the_bytecode() {
    let (result, hints, usage) = run_with_oracle(
        system_db(),
        call_tx(ORACLE_CONTRACT_ADDRESS, [0xde, 0xad, 0xbe, 0xef], U256::ZERO),
    );
    assert!(!result.result.is_success(), "the Oracle has no such method");
    assert_eq!(
        result.result.output().cloned().unwrap_or_default(),
        Bytes::new(),
        "the contract has no code to answer a selector it does not declare",
    );
    assert!(hints.is_empty());
    assert_eq!(usage.data_size, mega_evm::TX_BODY_SIZE + 4);
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

/// A hint cannot be un-sent, so the bytes it cost stay counted even when the frame that sent it
/// reverts and takes its writes back.
#[test]
fn test_a_reverting_frame_does_not_take_the_hint_bytes_back() {
    use revm::bytecode::opcode::{POP, PUSH0, REVERT};

    let data = send_hint(b"a hint that was sent");
    let code = BytecodeBuilder::default()
        .mstore(0x0, &data)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(200_000_u64)
        .append(CALL)
        .append(POP)
        .append_many([PUSH0, PUSH0, REVERT])
        .build();

    let (result, hints, usage) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));

    assert!(!result.result.is_success(), "the frame that sent the hint reverted");
    assert_eq!(hints.len(), 1, "the hint reached the service before the revert");
    assert_eq!(
        usage.data_size,
        nested_hint(data.len() as u64),
        "the bytes stay counted: a revert cannot take the hint back",
    );
}

/// A payload that does not fit in what is left of the transaction's data-size limit is not
/// forwarded, and the crossing stops the transaction: the hint's bytes are counted before the
/// payload is decoded, so the limit is crossed before the frame the call would have started.
///
/// The bytes stay counted — the crossing is what the transaction is stopped for, and the usage
/// is what shows it — and the transaction settles as a revert carrying the stop's own data.
#[test]
fn test_a_hint_that_crosses_the_data_size_limit_is_not_forwarded() {
    let data = send_hint(METERED_HINT);
    // The body already counts the calldata. The hint counts it again, and that second copy is
    // the byte that crosses.
    let counted = top_level_hint(data.len() as u64);
    let limit = counted - 1;
    let (outcome, hints) = run_with_oracle_under(
        system_db(),
        call_tx(ORACLE_CONTRACT_ADDRESS, data, U256::ZERO),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
    );

    assert!(hints.is_empty(), "the hint the transaction cannot pay for is not forwarded");
    assert_eq!(
        outcome.usage.data_size, counted,
        "the whole payload is counted: the count is what crossed the limit",
    );
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit,
            used: counted,
            frame_local: false,
        }),
    );
    assert_eq!(
        outcome.result.output().cloned().unwrap_or_default(),
        Bytes::from(MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit }.abi_encode()),
        "the transaction settles as the stop's revert",
    );
}

/// The same payload at exactly the transaction's limit is forwarded: the limit is crossed only
/// above it.
#[test]
fn test_a_hint_at_the_data_size_limit_is_forwarded() {
    let data = send_hint(METERED_HINT);
    let (outcome, hints) = run_with_oracle_under(
        system_db(),
        call_tx(ORACLE_CONTRACT_ADDRESS, data.clone(), U256::ZERO),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(top_level_hint(data.len() as u64)),
    );

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(hints.len(), 1);
    assert_eq!(outcome.usage.data_size, top_level_hint(data.len() as u64));
    assert_eq!(outcome.limit_exceeded, None);
}

/* ---------- the legacy engine's hint rows ---------- */

/// A second contract, calling the first.
const INNER: alloy_primitives::Address =
    alloy_primitives::address!("0x0000000000000000000000000000000000300002");

/// Appends a `CALL` of the Oracle with `data` and `gas`, the calldata written at 0x100, leaving
/// the call's status on the stack.
fn call_oracle(code: BytecodeBuilder, data: &[u8], gas: u64) -> BytecodeBuilder {
    code.mstore(0x100, data)
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0x100_u16) // argsOffset
        .push_number(0_u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(gas)
        .append(CALL)
}

/// Appends a return of the status on the stack.
fn return_status(code: BytecodeBuilder) -> Bytes {
    use revm::bytecode::opcode::{MSTORE, PUSH0, RETURN};
    code.append_many([PUSH0, MSTORE]).push_number(32_u8).append_many([PUSH0, RETURN]).build()
}

/// Appends a `CALL` of `MegaAccessControl` with `selector`, discarding its status.
fn steer(code: BytecodeBuilder, selector: [u8; 4]) -> BytecodeBuilder {
    use mega_evm::system::ACCESS_CONTROL_ADDRESS;
    use revm::bytecode::opcode::POP;
    code.mstore(0x0, selector)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(4_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u32)
        .append_many([CALL, POP])
}

const DISABLE: [u8; 4] =
    mega_evm::system::IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
const ENABLE: [u8; 4] =
    mega_evm::system::IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR;

/// A contract's hint reaches the service with the contract as its sender, and the call runs the
/// Oracle's `sendHint`, which succeeds.
#[test]
fn test_a_contracts_hint_reaches_the_service_from_the_contract() {
    let data = send_hint(&[0xde, 0xad, 0xbe, 0xef]);
    let code = return_status(call_oracle(BytecodeBuilder::default(), &data, 1_000_000));
    let (result, hints, _) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));

    let outcome = result.result.output().cloned().unwrap_or_default();
    assert_eq!(U256::from_be_slice(&outcome), U256::ONE, "sendHint succeeded");
    assert_eq!(
        hints,
        vec![RecordedHint {
            from: CONTRACT,
            topic: TOPIC,
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef])
        }],
    );
}

/// The Oracle's code at another address is not the Oracle: its `sendHint` runs and forwards
/// nothing.
#[test]
fn test_the_oracles_code_elsewhere_forwards_no_hint() {
    use mega_evm::system::ORACLE_CONTRACT_CODE;

    let elsewhere = alloy_primitives::address!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    let db = system_db().account_code(elsewhere, ORACLE_CONTRACT_CODE);
    let (result, hints, _) =
        run_with_oracle(db, call_tx(elsewhere, send_hint(b"hint"), U256::ZERO));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert!(hints.is_empty());
}

/// Two hints reach the service in the order they were sent.
#[test]
fn test_hints_reach_the_service_in_the_order_they_were_sent() {
    use revm::bytecode::opcode::POP;

    let first = IOracle::sendHintCall {
        topic: B256::repeat_byte(0x11),
        data: Bytes::from_static(&[0xaa, 0xbb]),
    };
    let second = IOracle::sendHintCall {
        topic: B256::repeat_byte(0x22),
        data: Bytes::from_static(&[0xcc, 0xdd]),
    };
    let code = call_oracle(BytecodeBuilder::default(), &first.abi_encode(), 1_000_000).append(POP);
    let code = call_oracle(code, &second.abi_encode(), 1_000_000).append(POP).stop().build();
    let (_, hints, _) = run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    assert_eq!(
        hints,
        vec![
            RecordedHint { from: CONTRACT, topic: first.topic, data: first.data },
            RecordedHint { from: CONTRACT, topic: second.topic, data: second.data },
        ],
    );
}

/// A transaction that sends value with its `sendHint` forwards nothing: the method is not payable,
/// so the transaction reverts in the bytecode, and its hint would be one it never sent.
#[test]
fn test_a_value_bearing_hint_transaction_forwards_nothing() {
    let (result, hints, usage) = run_with_oracle(
        system_db(),
        call_tx(ORACLE_CONTRACT_ADDRESS, send_hint(b"paid"), U256::ONE),
    );
    assert!(!result.result.is_success(), "{:?}", result.result);
    assert!(hints.is_empty());
    assert_eq!(
        usage.data_size,
        mega_evm::TX_BODY_SIZE + send_hint(b"paid").len() as u64,
        "the calldata alone: no hint was counted",
    );
}

/// A hint sent with gas is forwarded, its payload whole, and the calldata of the call is counted
/// on the transaction.
#[test]
fn test_a_hint_with_gas_forwards_and_is_counted() {
    let data = send_hint(&[0_u8; 128]);
    let code = return_status(call_oracle(BytecodeBuilder::default(), &data, 100_000));
    let (_, hints, usage) = run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].data.len(), 128);
    assert_eq!(usage.data_size, nested_hint(data.len() as u64));
}

/// A call of the Oracle with a selector that is not `sendHint` is not a hint, whatever it
/// carries: nothing is forwarded or counted.
#[test]
fn test_an_unknown_selector_to_the_oracle_counts_no_hint() {
    let data: Vec<u8> = [0xde, 0xad, 0xbe, 0xef].into_iter().chain([0_u8; 256]).collect();
    let code = return_status(call_oracle(BytecodeBuilder::default(), &data, 1_000_000));
    let (_, hints, usage) = run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    assert!(hints.is_empty());
    assert_eq!(usage.data_size, nested_hint(0));
}

/// A malformed `sendHint` payload, and one followed by bytes the decoder drops, are counted
/// whole: the first forwards nothing, the second forwards its envelope. Under a limit that holds
/// half of the payload, both are stopped before anything is forwarded.
#[test]
fn test_a_malformed_or_padded_hint_is_counted_whole() {
    let malformed: Vec<u8> = IOracle::sendHintCall::SELECTOR
        .iter()
        .copied()
        .chain(core::iter::repeat_n(0xab, 1024))
        .collect();
    let padded: Vec<u8> =
        send_hint(&[]).iter().copied().chain(core::iter::repeat_n(0xcd, 4096)).collect();
    for (data, forwarded) in [(malformed, 0), (padded, 1)] {
        let code = return_status(call_oracle(BytecodeBuilder::default(), &data, 1_000_000));
        let total = data.len() as u64;

        let (outcome, hints) = run_with_oracle_under(
            with_contract(code.clone()),
            call_tx(CONTRACT, [], U256::ZERO),
            EvmTxRuntimeLimits::no_limits(),
        );
        assert!(outcome.result.is_success());
        assert_eq!(hints.len(), forwarded);
        assert!(hints.iter().all(|hint| hint.data.is_empty()), "the padding is dropped");
        assert_eq!(outcome.usage.data_size, nested_hint(total), "the whole calldata is counted");

        let limit = nested_hint(total / 2);
        let (outcome, hints) = run_with_oracle_under(
            with_contract(code),
            call_tx(CONTRACT, [], U256::ZERO),
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
        );
        assert!(hints.is_empty(), "a hint past the limit is not forwarded");
        assert_eq!(
            outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit,
                used: nested_hint(total),
                frame_local: false,
            }),
        );
    }
}

/// Hints add up on the transaction: under a limit that holds one and a half, the first is
/// forwarded and the second stops the transaction.
#[test]
fn test_consecutive_hints_add_up_on_the_transaction() {
    use revm::bytecode::opcode::POP;

    let data = send_hint(&[0_u8; 256]);
    let len = data.len() as u64;
    let code = call_oracle(BytecodeBuilder::default(), &data, 1_000_000).append(POP);
    let code = return_status(call_oracle(code, &data, 1_000_000));
    let limit = nested_hint(len + len / 2);
    let (outcome, hints) = run_with_oracle_under(
        with_contract(code),
        call_tx(CONTRACT, [], U256::ZERO),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
    );
    assert_eq!(hints.len(), 1, "the first hint was forwarded, the second was not");
    assert!(matches!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, .. })
    ));
}

/// A contract's hint whose payload crosses the transaction's data-size limit is not forwarded, and
/// the crossing stops the transaction with a revert carrying the stop, not a halt: the contract
/// never runs on to return its call's status, and the transaction is billed what ran rather than
/// its gas limit.
#[test]
fn test_data_size_overflow_blocks_forwarding_and_stops_the_transaction() {
    const LIMIT: u64 = 2_048;
    let data = send_hint(&[0_u8; 4_096]);
    let code = return_status(call_oracle(BytecodeBuilder::default(), &data, 1_000_000));
    let (outcome, hints) = run_with_oracle_under(
        with_contract(code),
        call_tx(CONTRACT, [], U256::ZERO),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(LIMIT),
    );

    assert!(hints.is_empty(), "the hint the transaction cannot pay for is not forwarded");
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: LIMIT,
        used: nested_hint(data.len() as u64),
        frame_local: false,
    };
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert!(!outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(outcome.result.output(), Some(&stop.revert_data()), "the stop, not the status");
    assert!(
        outcome.gas.gas_used < crate::common::GAS_LIMIT / 10,
        "the stop burns nothing: {}",
        outcome.gas.gas_used
    );
}

/// A frame that switched its volatile-data access off sends no hint: `sendHint` still runs and
/// succeeds, and nothing reaches the service.
#[test]
fn test_a_frame_that_switched_access_off_sends_no_hint() {
    let data = send_hint(&[0_u8; 64]);
    let code =
        return_status(call_oracle(steer(BytecodeBuilder::default(), DISABLE), &data, 100_000));
    let (result, hints, usage) =
        run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    let outcome = result.result.output().cloned().unwrap_or_default();
    assert_eq!(U256::from_be_slice(&outcome), U256::ONE, "the call ran and succeeded");
    assert!(hints.is_empty());
    assert_eq!(usage.data_size, nested_hint(0), "a withheld hint is not counted");
}

/// A frame whose access is on sends its hint, and so does one that switched it off and back on.
#[test]
fn test_a_frame_with_access_on_sends_its_hint() {
    let data = send_hint(&[0_u8; 64]);
    for code in
        [BytecodeBuilder::default(), steer(steer(BytecodeBuilder::default(), DISABLE), ENABLE)]
    {
        let code = return_status(call_oracle(code, &data, 100_000));
        let (_, hints, usage) =
            run_with_oracle(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
        assert_eq!(hints.len(), 1);
        assert_eq!(usage.data_size, nested_hint(data.len() as u64));
    }
}

/// A child of a frame that switched access off sends no hint, although it switched nothing off
/// itself.
#[test]
fn test_a_child_of_a_frame_that_switched_access_off_sends_no_hint() {
    let data = send_hint(&[0_u8; 64]);
    let inner = return_status(call_oracle(BytecodeBuilder::default(), &data, 100_000));
    let outer = steer(BytecodeBuilder::default(), DISABLE)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(INNER)
        .push_number(10_000_000_u32)
        .append(CALL);
    let outer = return_status(outer);
    let db = with_contract(outer).account_code(INNER, inner);
    let (result, hints, _) = run_with_oracle(db, call_tx(CONTRACT, [], U256::ZERO));
    let outcome = result.result.output().cloned().unwrap_or_default();
    assert_eq!(U256::from_be_slice(&outcome), U256::ONE, "the child ran");
    assert!(hints.is_empty());
}

/// A withheld hint is not counted, so a payload far past the transaction's data-size limit does
/// not stop a transaction that switched access off first.
#[test]
fn test_a_withheld_hint_is_not_counted() {
    let data = send_hint(&[0_u8; 4096]);
    let code =
        return_status(call_oracle(steer(BytecodeBuilder::default(), DISABLE), &data, 1_000_000));
    let (outcome, hints) = run_with_oracle_under(
        with_contract(code),
        call_tx(CONTRACT, [], U256::ZERO),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(nested_hint(2_048)),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None);
    assert!(hints.is_empty());
}
