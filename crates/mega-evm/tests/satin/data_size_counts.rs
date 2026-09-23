//! What the data-size limit counts: the bytes and write records each kind of transaction keeps.
//!
//! Every transaction keeps its body: the envelope, the five records of the writes its inclusion
//! makes, its calldata, its access list and its authorizations. What its execution keeps comes on
//! top: a 40-byte record per account or storage write, a log's 32 bytes plus 32 per topic plus its
//! data, and the code a creation deploys. A frame that fails keeps nothing, except a creator's
//! nonce record once the nonce was bumped.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitUsage, MegaEvm, MegaTransaction, AUTHORIZATION_SIZE, LOG_BASE_SIZE,
    LOG_TOPIC_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, DELEGATECALL, GAS, INVALID, LOG0, LOG1, POP, PUSH0, RETURN, SELFDESTRUCT,
        SLOAD, SSTORE, STATICCALL, STOP,
    },
    context::result::ExecutionResult,
};

use crate::common::{call, call_with_data, context, create, execute};

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const CALLEE: Address = address!("0000000000000000000000000000000000400001");
const LIBRARY: Address = address!("0000000000000000000000000000000000400002");
const EXISTING: Address = address!("0000000000000000000000000000000000400003");

const GAS_LIMIT: u64 = 20_000_000;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)))
}

/// What a transaction keeps: its body with `extra` body bytes, `bytes` more from its execution,
/// and `records` write records among those.
const fn kept(extra: u64, bytes: u64, records: u64) -> LimitUsage {
    LimitUsage { data_size: TX_BODY_SIZE + extra + bytes, write_records: records }
}

/// `records` write records and nothing else.
const fn records(records: u64) -> LimitUsage {
    kept(0, records * WRITE_RECORD_SIZE, records)
}

/// Init code that returns `size` zero bytes as the deployed contract.
fn constructor_returning(size: u64) -> Bytes {
    BytecodeBuilder::default().push_number(size).push_number(0_u8).append(RETURN).build()
}

/// A contract that `CREATE`s `init` and stops.
fn factory(init: &Bytes) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, init)
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(CREATE)
        .append(POP)
        .stop()
        .build()
}

/// A call from the running frame to `target` with `value` and all its gas, by `opcode`.
fn call_to(opcode: u8, target: Address, value: u8) -> BytecodeBuilder {
    let builder = BytecodeBuilder::default().append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
    let builder = if opcode == CALL { builder.push_number(value) } else { builder };
    builder.push_address(target).append(GAS).append(opcode).append(POP)
}

/// `LOG0` of `len` zero bytes.
fn log0(builder: BytecodeBuilder, len: u64) -> BytecodeBuilder {
    builder.push_number(len).push_number(0_u64).append(LOG0)
}

/// A type-4 call from [`CALLER`] to [`CALLEE`] carrying `data`, an access list of one address
/// with one key, and an authorization that delegates the sender's own account to [`LIBRARY`].
fn call_with_body(data: Bytes) -> MegaTransaction {
    use revm::{
        context::{transaction::TransactionType, TxEnv},
        context_interface::{
            either::Either,
            transaction::{
                AccessList, AccessListItem, Authorization, RecoveredAuthority,
                RecoveredAuthorization,
            },
        },
    };
    // The sender's nonce is bumped before its authorizations apply, so its own authorization
    // names the nonce after the bump.
    let authorization = Either::Right(RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::ZERO, address: LIBRARY, nonce: 1 },
        RecoveredAuthority::Valid(CALLER),
    ));
    alloy_op_evm::OpTx(mega_evm::test_utils::op_transaction(TxEnv {
        tx_type: TransactionType::Eip7702 as u8,
        caller: CALLER,
        kind: alloy_primitives::TxKind::Call(CALLEE),
        data,
        gas_limit: GAS_LIMIT,
        gas_priority_fee: Some(0),
        access_list: AccessList(vec![AccessListItem {
            address: CALLER,
            storage_keys: vec![B256::ZERO],
        }]),
        authorization_list: vec![authorization],
        ..Default::default()
    }))
}

/// Each kind of transaction keeps its body and exactly the bytes and records its execution
/// makes.
#[test]
fn test_what_each_transaction_keeps() {
    let with_code = |code: Bytes| funded().account_code(CALLEE, code);
    let init = constructor_returning(10);
    let reverting_init = BytecodeBuilder::default().revert().build();
    let one_topic_and_ten_bytes = BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(10_u64)
        .push_number(0_u64)
        .append(LOG1)
        .stop()
        .build();
    let thirty_two_bytes_one_topic = BytecodeBuilder::default()
        .mstore(0, [0x11_u8; 32])
        .push_number(0xabc_u64)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(LOG1)
        .stop()
        .build();
    let access_list = 20 + 32;

    let cases: Vec<(&str, MemoryDatabase, MegaTransaction, LimitUsage)> = vec![
        (
            "a call to an existing account without code",
            funded().account_balance(CALLEE, U256::from(1)),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 0, 0),
        ),
        (
            "a call to an account that does not exist",
            funded(),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 0, 0),
        ),
        (
            "calldata, an access list and the sender's own authorization",
            funded(),
            call_with_body(Bytes::from_static(&[1, 2, 3, 4])),
            kept(4 + access_list + AUTHORIZATION_SIZE, 0, 0),
        ),
        (
            "a value transfer to an existing account",
            funded().account_balance(CALLEE, U256::from(100)),
            call(CALLER, CALLEE, U256::from(1), GAS_LIMIT),
            records(1),
        ),
        (
            "a value transfer to an account that does not exist",
            funded(),
            call(CALLER, CALLEE, U256::from(1), GAS_LIMIT),
            records(1),
        ),
        (
            "a creation deploying 10 bytes",
            funded(),
            create(CALLER, init.clone(), GAS_LIMIT),
            kept(init.len() as u64, WRITE_RECORD_SIZE + 10, 1),
        ),
        (
            "a factory creating a contract of 10 bytes",
            with_code(factory(&init)),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 2 * WRITE_RECORD_SIZE + 10, 2),
        ),
        (
            "a storage write",
            with_code(BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build()),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            records(1),
        ),
        (
            "a storage read",
            with_code(BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build()),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 0, 0),
        ),
        (
            "a log with one topic and 10 bytes of data",
            with_code(one_topic_and_ten_bytes),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, LOG_BASE_SIZE + LOG_TOPIC_SIZE + 10, 0),
        ),
        (
            "an empty LOG0",
            with_code(log0(BytecodeBuilder::default(), 0).stop().build()),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, LOG_BASE_SIZE, 0),
        ),
        (
            "a LOG1 of 32 bytes",
            with_code(thirty_two_bytes_one_topic),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, LOG_BASE_SIZE + LOG_TOPIC_SIZE + 32, 0),
        ),
        (
            "two empty LOG0s",
            with_code(log0(log0(BytecodeBuilder::default(), 0), 0).stop().build()),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 2 * LOG_BASE_SIZE, 0),
        ),
        (
            "a DELEGATECALL",
            with_code(call_to(DELEGATECALL, LIBRARY, 0).stop().build())
                .account_code(LIBRARY, BytecodeBuilder::default().stop().build()),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 0, 0),
        ),
        (
            "a value transfer the recipient passes on: its account is recorded once",
            with_code(call_to(CALL, LIBRARY, 1).stop().build()),
            call(CALLER, CALLEE, U256::from(100), GAS_LIMIT),
            records(2),
        ),
        (
            "a child that writes a slot and halts",
            with_code(call_to(CALL, LIBRARY, 0).stop().build()).account_code(
                LIBRARY,
                BytecodeBuilder::default()
                    .append_many([PUSH0, PUSH0, SLOAD, SSTORE, INVALID])
                    .build(),
            ),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 0, 0),
        ),
        (
            "a creation whose constructor reverts: the creator's bumped nonce stays",
            with_code(factory(&reverting_init)),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            records(1),
        ),
        (
            "a SELFDESTRUCT that moves value to an existing account",
            with_code(
                BytecodeBuilder::default().push_address(EXISTING).append(SELFDESTRUCT).build(),
            )
            .account_balance(CALLEE, U256::from(1_000))
            .account_balance(EXISTING, U256::from(1)),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            records(1),
        ),
        (
            "a SELFDESTRUCT to itself, which moves nothing",
            with_code(BytecodeBuilder::default().push_address(CALLEE).append(SELFDESTRUCT).build())
                .account_balance(CALLEE, U256::from(1_000)),
            call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
            kept(0, 0, 0),
        ),
    ];

    for (name, db, tx, expected) in cases {
        let outcome = execute(db, tx);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.usage, expected, "{name}");
    }
}

/// A child that writes a slot, writes it back and reverts runs under a caller that already wrote
/// one. The child's write and its write-back both go with its revert, so neither reaches the
/// caller: the caller keeps its own write and that write's record, no more and no less.
#[test]
fn test_a_reverted_childs_write_and_write_back_leave_its_callers_write() {
    const CALLER_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);
    let parent = BytecodeBuilder::default()
        .sstore(CALLER_SLOT, U256::from(1))
        .append_many(call_to(CALL, LIBRARY, 0).build_vec())
        .stop()
        .build();
    let child = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .sstore(U256::ZERO, U256::ZERO)
        .revert()
        .build();
    let db = funded().account_code(CALLEE, parent).account_code(LIBRARY, child);
    let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage, records(1));
    assert_eq!(
        outcome.state[&CALLEE].storage.get(&CALLER_SLOT).map(|slot| slot.present_value),
        Some(U256::from(1)),
        "the caller's write survives the child's revert",
    );
    assert!(
        outcome.state.get(&LIBRARY).is_none_or(|library| {
            library.storage.get(&U256::ZERO).is_none_or(|slot| !slot.is_changed())
        }),
        "the child's write went with its revert",
    );
}

/// The sender's own authorization in [`call_with_body`] applies: its bytes are the body's, and
/// the account it writes is the sender's, which the body already records.
#[test]
fn test_the_senders_own_authorization_applies_and_records_nothing() {
    let outcome = execute(funded(), call_with_body(Bytes::new()));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let code = outcome.state[&CALLER].info.code.clone().expect("the sender is delegated");
    assert_eq!(code.eip7702_address(), Some(LIBRARY));
    assert_eq!(outcome.usage, kept(20 + 32 + AUTHORIZATION_SIZE, 0, 0));
}

/// A `SELFDESTRUCT` in a static frame halts before it moves anything, so the frame keeps what a
/// frame that stops keeps: nothing.
#[test]
fn test_a_selfdestruct_in_a_static_frame_keeps_nothing() {
    let run = |child: Bytes| {
        let db = funded()
            .account_code(CALLEE, call_to(STATICCALL, LIBRARY, 0).stop().build())
            .account_code(LIBRARY, child)
            .account_balance(LIBRARY, U256::from(1_000));
        execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
    };
    let selfdestruct =
        run(BytecodeBuilder::default().push_address(EXISTING).append(SELFDESTRUCT).build());
    let stop = run(BytecodeBuilder::default().stop().build());
    assert!(selfdestruct.result.is_success() && stop.result.is_success());
    assert_eq!(selfdestruct.usage, stop.usage);
    assert_eq!(selfdestruct.usage, kept(0, 0, 0));
    assert!(selfdestruct.state.get(&EXISTING).is_none_or(|a| a.info.balance.is_zero()));
}

/// A transaction whose own frame reverts keeps its body: the body is the transaction's, not the
/// frame's.
#[test]
fn test_a_top_level_revert_keeps_the_body() {
    let db = funded().account_code(CALLEE, BytecodeBuilder::default().revert().build());
    let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(matches!(outcome.result, ExecutionResult::Revert { .. }), "{:?}", outcome.result);
    assert_eq!(outcome.usage, kept(0, 0, 0));
    assert_eq!(outcome.limit_exceeded, None);
}

/// A transaction limit equal to what the transaction keeps does not stop it, however much of it
/// is calldata.
#[test]
fn test_a_limit_equal_to_what_is_kept_does_not_stop() {
    let db = || funded().account_code(CALLEE, BytecodeBuilder::default().stop().build());
    for calldata in [0_usize, 500] {
        let limit = TX_BODY_SIZE + calldata as u64;
        let outcome = MegaEvm::new(context(db()).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
        ))
        .execute_transaction(call_with_data(
            CALLER,
            CALLEE,
            Bytes::from(vec![0xab; calldata]),
            GAS_LIMIT,
        ))
        .unwrap();
        assert!(outcome.result.is_success(), "{calldata} bytes: {:?}", outcome.result);
        assert_eq!(outcome.usage.data_size, limit);
        assert_eq!(outcome.limit_exceeded, None);
    }
}

/// Every transaction counts from zero: what one EVM ran before does not add to the next.
///
/// The EVM does not commit what it runs, so the same transaction runs twice on the same state.
#[test]
fn test_each_transaction_counts_from_zero() {
    let db = funded().account_code(
        CALLEE,
        BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
    );
    let mut evm = MegaEvm::new(context(db));
    for run in 0..2 {
        let result = evm.transact_raw(call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT)).unwrap();
        assert!(result.result.is_success(), "{:?}", result.result);
        assert_eq!(evm.ctx().additional_limit().usage(), records(1), "run {run}");
    }
}
