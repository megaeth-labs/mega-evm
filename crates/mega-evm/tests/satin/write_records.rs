//! The observe-stage-commit layer and the write records it counts.
//!
//! A write record is one account or storage write, 40 bytes of data size. The sites: the first
//! change of a slot in the transaction (taken back when the slot is written back), a value
//! transfer's sender and recipient, a creation's creator nonce and created account, a
//! `SELFDESTRUCT` that moves value to another account, an applied EIP-7702 authority, and the
//! transaction's own value recipient or created account. The sender's own account is part of the
//! transaction body and is not a record. Records are deduplicated per frame and discarded with the
//! frame that fails.

use alloy_primitives::{address, Address, Bytes, Log, LogData, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LimitUsage, MegaContext, MegaEvm, StagedRecord, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLDATASIZE, CREATE, GAS, INVALID, JUMPDEST, JUMPI, LOG2, POP, PUSH0, PUSH1,
        SELFDESTRUCT, SSTORE, STOP,
    },
    interpreter::{interpreter::EthInterpreter, interpreter_types::Jumps, Interpreter},
    Database, Inspector,
};

use crate::common::{call, call_with_data, context, create, run};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");
const CONTRACT2: Address = address!("0000000000000000000000000000000000100003");
const GAS_LIMIT: u64 = 10_000_000;

const fn records(n: u64) -> LimitUsage {
    LimitUsage { data_size: n * WRITE_RECORD_SIZE, write_records: n }
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
}

/// Appends `CALL(gas=GAS, target, value, args=[], ret=[])`, leaving the success flag.
fn append_value_call(builder: BytecodeBuilder, target: Address, value: u8) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(value)
        .push_address(target)
        .append(GAS)
        .append(CALL)
}

/// Appends a `CREATE` with value 1 and empty init code.
fn append_value_create(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder.append_many([PUSH0, PUSH0]).append(PUSH1).append(1u8).append(CREATE)
}

/* ---------- value transfers and creations, deduplicated per frame ---------- */

/// Two value transfers from one frame record the sender once and each recipient.
#[test]
fn test_two_value_calls_record_the_sender_once() {
    let code = append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP);
    let code = append_value_call(code, CONTRACT2, 1).append(POP).append(STOP).build();
    let (result, usage) =
        run(funded().account_code(CALLEE, code), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(3), "CALLEE once, CONTRACT, CONTRACT2");
}

/// A creation and a value transfer from one frame record the frame's account once.
#[test]
fn test_create_then_call_record_the_frame_account_once() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP);
    let code = append_value_call(code, CONTRACT, 1).append(POP).append(STOP).build();
    let (result, usage) =
        run(funded().account_code(CALLEE, code), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(3), "CALLEE once, the created account, CONTRACT");
}

/// Two creations from one frame record the creator once and each created account.
#[test]
fn test_two_creates_record_the_creator_once() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP);
    let code = append_value_create(code).append(POP).append(STOP).build();
    let (result, usage) =
        run(funded().account_code(CALLEE, code), call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(3), "CALLEE once, two created accounts");
}

/// A value transfer into a child that fails is discarded with the child, sender included.
#[test]
fn test_failed_child_discards_its_transfer_records() {
    let code =
        append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP).append(STOP).build();
    let db =
        funded().account_code(CALLEE, code).account_code(CONTRACT, Bytes::from_static(&[INVALID]));
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "the outer call succeeds although the child halts");
    assert_eq!(usage, LimitUsage::ZERO);
}

/// After a failed transfer the sender can be recorded again by the next one.
#[test]
fn test_failed_first_child_lets_the_next_transfer_record_the_sender() {
    let code = append_value_call(BytecodeBuilder::default(), CONTRACT, 1).append(POP);
    let code = append_value_call(code, CONTRACT2, 1).append(POP).append(STOP).build();
    let db =
        funded().account_code(CALLEE, code).account_code(CONTRACT, Bytes::from_static(&[INVALID]));
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(2), "CALLEE and CONTRACT2, from the second transfer");
}

const OUTER_CREATOR: Address = address!("000000000000000000000000000000000000C0FF");

/// `PUSH5 <PUSH1 0 PUSH1 0 REVERT>; PUSH1 0; MSTORE; (PUSH1 5; PUSH1 27; PUSH1 0; CREATE; POP) x
/// n`: `n` creations whose init code reverts.
fn reverting_creations(n: usize) -> Bytes {
    let mut code = vec![0x64, 0x60, 0x00, 0x60, 0x00, 0xFD, 0x60, 0x00, 0x52];
    for _ in 0..n {
        code.extend_from_slice(&[0x60, 0x05, 0x60, 0x1b, 0x60, 0x00, 0xf0, 0x50]);
    }
    code.push(0x00);
    code.into()
}

/// A reverted creation keeps the creator's nonce record: the nonce bump survives the revert.
#[test]
fn test_reverted_nested_create_keeps_the_creator_nonce_record() {
    let db = funded().account_code(OUTER_CREATOR, reverting_creations(1));
    let (result, usage) = run(db, call(CALLER, OUTER_CREATOR, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(1), "the creator's nonce; the created account is gone");
}

/// Two reverted creations record the creator once: the first record survives, so the second
/// finds the creator recorded.
#[test]
fn test_reverted_then_retried_create_records_the_creator_once() {
    let db = funded().account_code(OUTER_CREATOR, reverting_creations(2));
    let (result, usage) = run(db, call(CALLER, OUTER_CREATOR, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(1));
}

/// A creation the creator cannot fund never starts (the opcode checks the endowment before it
/// builds a frame), so it records nothing.
#[test]
fn test_creation_without_funds_records_nothing() {
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .push_number(1_000_000_000_000_u64)
        .append(CREATE)
        .append(POP)
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(CALLEE, code);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, LimitUsage::ZERO);
    assert_eq!(result.state[&CALLEE].info.nonce, 0, "the nonce was never bumped");
}

const A: Address = address!("00000000000000000000000000000000A0000001");
const B: Address = address!("00000000000000000000000000000000B0000002");
const SELF_CALLER: Address = address!("000000000000000000000000000000005E1F0004");

fn value_delta(to: Address) -> LimitUsage {
    let db = || MemoryDatabase::default().account_balance(A, U256::from(1_000_000));
    let (with_value, usage_value) = run(db(), call(A, to, U256::from(1), GAS_LIMIT));
    let (zero_value, usage_zero) = run(db(), call(A, to, U256::ZERO, GAS_LIMIT));
    assert!(with_value.result.is_success() && zero_value.result.is_success());
    usage_value.saturating_sub(usage_zero)
}

/// A top-level transfer to the sender itself writes no account the body does not already count.
#[test]
fn test_top_level_self_transfer_no_double_count() {
    assert_eq!(value_delta(A), LimitUsage::ZERO);
}

/// A top-level transfer to another account records the recipient.
#[test]
fn test_top_level_transfer_counts_the_recipient() {
    assert_eq!(value_delta(B), records(1));
}

/// `CALLDATASIZE; JUMPI` to a body that calls `targets` with `value`, so an inner call with empty
/// calldata stops at once.
fn self_calling_code(targets: &[Address], value: u64) -> Bytes {
    let mut code = BytecodeBuilder::default()
        .append(CALLDATASIZE)
        .push_number(5_u8)
        .append(JUMPI)
        .append(STOP)
        .append(JUMPDEST);
    for target in targets {
        code = code
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_number(value)
            .push_address(*target)
            .push_number(100_000_u64)
            .append(CALL)
            .append(POP);
    }
    code.stop().build()
}

fn nested_delta(targets: &[Address]) -> LimitUsage {
    let usage = |value| {
        let db = MemoryDatabase::default()
            .account_balance(A, U256::from(1_000_000))
            .account_balance(SELF_CALLER, U256::from(1_000_000))
            .account_code(SELF_CALLER, self_calling_code(targets, value));
        let (result, usage) =
            run(db, call_with_data(A, SELF_CALLER, Bytes::from_static(&[1]), GAS_LIMIT));
        assert!(result.result.is_success());
        usage
    };
    usage(1).saturating_sub(usage(0))
}

/// A nested transfer to the frame's own account is one account write.
#[test]
fn test_nested_self_call_with_value_counts_once() {
    assert_eq!(nested_delta(&[SELF_CALLER]), records(1));
}

/// After a transfer to another account recorded the frame's account, a transfer to itself adds
/// nothing.
#[test]
fn test_nested_self_transfer_after_recorded_sender_counts_nothing() {
    assert_eq!(nested_delta(&[B, SELF_CALLER]), records(2));
}

/* ---------- storage, logs, selfdestruct ---------- */

/// A slot's first change is a record; writing it back to its original value takes it back.
#[test]
fn test_sstore_first_change_records_and_write_back_refunds() {
    type Writes = &'static [(u64, u64)];
    let cases: [(Writes, u64, LimitUsage); 4] = [
        (&[(0, 1)], 0, records(1)),
        (&[(0, 1), (0, 2)], 0, records(1)),
        (&[(0, 1), (0, 0)], 0, LimitUsage::ZERO),
        (&[(0, 0)], 5, records(1)),
    ];
    for (writes, original, expected) in cases {
        let mut code = BytecodeBuilder::default();
        for (slot, value) in writes {
            code = code.sstore(U256::from(*slot), U256::from(*value));
        }
        let db = MemoryDatabase::default()
            .account_code(CALLEE, code.stop().build())
            .account_storage(CALLEE, U256::ZERO, U256::from(original));
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
        assert!(result.result.is_success());
        assert_eq!(usage, expected, "writes {writes:?} over original {original}");
    }
}

/// A write-back in a child refunds the record its caller made, and the refund dies with a
/// failing child.
#[test]
fn test_write_back_in_a_child_refunds_the_caller_record() {
    let child = BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).stop().build();
    let failing_child =
        BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).append(INVALID).build();
    for (code, expected) in [(child, LimitUsage::ZERO), (failing_child, records(1))] {
        // CALLEE writes slot 0, then DELEGATECALLs CONTRACT, which writes it back in CALLEE's
        // storage.
        let parent = BytecodeBuilder::default()
            .sstore(U256::ZERO, U256::from(1))
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(CONTRACT)
            .append(GAS)
            .append(revm::bytecode::opcode::DELEGATECALL)
            .append(POP)
            .stop()
            .build();
        let db =
            MemoryDatabase::default().account_code(CALLEE, parent).account_code(CONTRACT, code);
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
        assert!(result.result.is_success());
        assert_eq!(usage, expected);
    }
}

/// A log counts 32 bytes for its address, 32 per topic and its data; no write record.
#[test]
fn test_log_counts_its_bytes() {
    let code = BytecodeBuilder::default()
        .push_number(0x22_u8)
        .push_number(0x11_u8)
        .push_number(10_u8)
        .append(PUSH0)
        .append(LOG2)
        .stop()
        .build();
    let (result, usage) = run(
        MemoryDatabase::default().account_code(CALLEE, code),
        call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT),
    );
    assert!(result.result.is_success());
    assert_eq!(result.result.logs().len(), 1);
    assert_eq!(usage, LimitUsage { data_size: 32 + 2 * 32 + 10, write_records: 0 });
}

/// `SELFDESTRUCT` records the beneficiary only when value moves to another account than the
/// destructed one and the sender, whose account the transaction body counts.
#[test]
fn test_selfdestruct_records_the_beneficiary_when_value_moves() {
    let destruct_to = |beneficiary: Address| {
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build()
    };
    let cases = [
        (U256::from(5), CONTRACT, records(1)),
        (U256::from(5), CALLEE, LimitUsage::ZERO),
        (U256::from(5), CALLER, LimitUsage::ZERO),
        (U256::ZERO, CONTRACT, LimitUsage::ZERO),
    ];
    for (balance, beneficiary, expected) in cases {
        let db = MemoryDatabase::default()
            .account_code(CALLEE, destruct_to(beneficiary))
            .account_balance(CALLEE, balance);
        let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
        assert!(result.result.is_success());
        assert_eq!(usage, expected, "balance {balance}, beneficiary {beneficiary}");
    }
}

/// A top-level creation records the created account, and nothing when it fails.
#[test]
fn test_top_level_create_records_the_created_account() {
    let (result, usage) =
        run(MemoryDatabase::default(), create(CALLER, Bytes::from_static(&[STOP]), GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(usage, records(1));

    let (result, usage) =
        run(MemoryDatabase::default(), create(CALLER, Bytes::from_static(&[INVALID]), GAS_LIMIT));
    assert!(!result.result.is_success());
    assert_eq!(usage, LimitUsage::ZERO);
}

/* ---------- the commit happens after the opcode ---------- */

/// Records, after each opcode, what the layer holds.
#[derive(Default)]
struct LayerProbe {
    /// `(opcode, staged record left, usage)` after each opcode.
    steps: Vec<(u8, bool, LimitUsage)>,
    opcode: u8,
    /// Stages a log through the Host before every `SSTORE`, as a stale record.
    stage_before_sstore: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for LayerProbe {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut MegaContext<DB>) {
        self.opcode = interp.bytecode.opcode();
        if self.stage_before_sstore && self.opcode == SSTORE {
            let log = Log {
                address: CALLEE,
                data: LogData::new(vec![B256::ZERO], Bytes::new()).unwrap(),
            };
            revm::context_interface::Host::log(context, log);
            assert!(matches!(
                context.additional_limit().staged_record(),
                Some(StagedRecord::Log { .. })
            ));
        }
    }

    fn step_end(
        &mut self,
        _interp: &mut Interpreter<EthInterpreter>,
        context: &mut MegaContext<DB>,
    ) {
        let layer = context.additional_limit();
        self.steps.push((self.opcode, layer.staged_record().is_some(), layer.usage()));
    }
}

fn probe(
    db: MemoryDatabase,
    gas_limit: u64,
    stage_before_sstore: bool,
) -> Vec<(u8, bool, LimitUsage)> {
    let mut evm = MegaEvm::new(context(db))
        .with_inspector(LayerProbe { stage_before_sstore, ..Default::default() });
    alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, gas_limit)).unwrap();
    evm.inspector().steps.clone()
}

fn after_sstore(steps: &[(u8, bool, LimitUsage)]) -> (bool, LimitUsage) {
    let (_, staged, usage) = steps.iter().find(|(op, ..)| *op == SSTORE).copied().unwrap();
    (staged, usage)
}

/// A completed `SSTORE` commits its record: nothing is left staged and the record is counted by
/// the time the opcode ends.
#[test]
fn test_completed_sstore_commits_its_record() {
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let steps = probe(MemoryDatabase::default().account_code(CALLEE, code), GAS_LIMIT, false);
    let (staged, usage) = after_sstore(&steps);
    assert!(!staged);
    assert_eq!(usage, records(1));
}

/// An `SSTORE` that runs out of gas after its Host call wrote the slot discards the staged
/// record: the Host observed the write, the failure takes it back, and nothing is counted.
#[test]
fn test_sstore_out_of_gas_after_the_write_discards_the_record() {
    // CALLEE calls CONTRACT with 5,000 gas: past the EIP-2200 stipend check, so the Host call
    // writes the slot, but short of the 22,100 a cold first write charges after it.
    let child = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT)
        .push_number(5_000_u64)
        .append(CALL)
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(CALLEE, parent).account_code(CONTRACT, child);
    let steps = probe(db, GAS_LIMIT, false);
    let (staged, usage) = after_sstore(&steps);
    assert!(!staged, "the record is discarded, not left for the next opcode");
    assert_eq!(usage, LimitUsage::ZERO, "a failed opcode's write is not counted");
}

/// A record staged outside an opcode's wrapper is discarded when the next wrapper starts, so it
/// is never committed for that opcode.
#[test]
fn test_wrapper_discards_a_stale_record_on_entry() {
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    let steps = probe(MemoryDatabase::default().account_code(CALLEE, code), GAS_LIMIT, true);
    let (staged, usage) = after_sstore(&steps);
    assert!(!staged);
    assert_eq!(usage, records(1), "the SSTORE's record, and not the stale log's bytes");
}

/* ---------- applied EIP-7702 authorities ---------- */

const DELEGATE: Address = address!("00000000000000000000000000000000DE1E0001");
const AUTHORITY_1: Address = address!("00000000000000000000000000000000A0717001");
const AUTHORITY_2: Address = address!("00000000000000000000000000000000A0717002");

/// A type-4 transaction from `CALLER` to `to` with value `value`, carrying authorizations
/// `(authority, nonce)` that delegate to `DELEGATE`.
fn authorizing_call(
    to: Address,
    value: u64,
    authorizations: &[(Address, u64)],
) -> mega_evm::MegaTransaction {
    use revm::{
        context::{transaction::TransactionType, TxEnv},
        context_interface::{
            either::Either,
            transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
        },
    };
    let authorization_list = authorizations
        .iter()
        .map(|(authority, nonce)| {
            Either::Right(RecoveredAuthorization::new_unchecked(
                Authorization { chain_id: U256::ZERO, address: DELEGATE, nonce: *nonce },
                RecoveredAuthority::Valid(*authority),
            ))
        })
        .collect();
    alloy_op_evm::OpTx(mega_evm::test_utils::op_transaction(TxEnv {
        tx_type: TransactionType::Eip7702 as u8,
        caller: CALLER,
        kind: alloy_primitives::TxKind::Call(to),
        value: U256::from(value),
        gas_limit: GAS_LIMIT,
        gas_priority_fee: Some(0),
        authorization_list,
        ..Default::default()
    }))
}

/// Each distinct applied authority is one record, however many of its authorizations applied;
/// an authorization that does not apply (a wrong nonce) records nothing.
#[test]
fn test_applied_authorities_record_once_each() {
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
    let (result, usage) = run(
        db(),
        authorizing_call(CONTRACT, 0, &[(AUTHORITY_1, 0), (AUTHORITY_2, 0), (AUTHORITY_1, 1)]),
    );
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(usage, records(2));

    let (result, usage) = run(db(), authorizing_call(CONTRACT, 0, &[(AUTHORITY_1, 7)]));
    assert!(result.result.is_success());
    assert_eq!(usage, LimitUsage::ZERO, "a wrong nonce applies nothing");

    let (result, usage) = run(db(), authorizing_call(CONTRACT, 0, &[(CALLER, 1)]));
    assert!(result.result.is_success());
    assert_eq!(usage, LimitUsage::ZERO, "the sender's own account is part of the body");
}

/// A value transfer to an applied authority records the authority once, not again as the
/// recipient.
#[test]
fn test_value_to_an_applied_authority_records_it_once() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
    let (result, usage) = run(db, authorizing_call(AUTHORITY_1, 1, &[(AUTHORITY_1, 0)]));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(usage, records(1));
}

/// A creation a frame budget stops before it runs bumps no nonce, so it leaves no creator
/// record behind: the caller keeps only what it wrote itself.
#[test]
fn test_creation_stopped_at_init_leaves_no_creator_record() {
    // CALLEE creates (two records: created account and creator), which a 60-byte budget stops,
    // then writes a slot.
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0])
        .append(CREATE)
        .append(POP)
        .sstore(U256::from(1), U256::from(1))
        .stop()
        .build();
    let db = MemoryDatabase::default().account_code(CALLEE, code);
    let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(60);
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
            .unwrap();
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&CALLEE].info.nonce, 0, "the creation never started");
    assert_eq!(evm.ctx().additional_limit().usage(), records(1), "only CALLEE's slot");
}

/// A frame running as the transaction's sender (a delegated sender called back) records no
/// write to the sender's account: the transaction body counts it.
#[test]
fn test_frames_running_as_the_sender_do_not_record_it() {
    // CALLER is delegated to CONTRACT, whose code sends value to CONTRACT2. CALLEE calls CALLER.
    let relay = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CALLER)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let sender_code = append_value_call(BytecodeBuilder::default(), CONTRACT2, 1)
        .append(POP)
        .append(STOP)
        .build();
    let mut db = funded().account_code(CALLEE, relay).account_code(CONTRACT, sender_code);
    let delegation = revm::state::Bytecode::new_eip7702(CONTRACT);
    let account = db.load_account(CALLER).unwrap();
    account.info.code_hash = delegation.hash_slow();
    account.info.code = Some(delegation);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(result.state[&CONTRACT2].info.balance, U256::from(1), "the transfer happened");
    assert_eq!(usage, records(1), "CONTRACT2 only; the sender is part of the body");
}

/// Raises the endowment of every creation past what the creator holds.
struct OverfundCreations;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for OverfundCreations {
    fn create(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut revm::interpreter::CreateInputs,
    ) -> Option<revm::interpreter::CreateOutcome> {
        inputs.set_value(U256::MAX);
        None
    }
}

/// A creation that fails before bumping the creator's nonce (here, an endowment an inspector
/// raised past the creator's balance) takes its creator record back with it.
#[test]
fn test_creation_failing_before_the_nonce_bump_records_nothing() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP).append(STOP).build();
    let mut evm = MegaEvm::new(context(funded().account_code(CALLEE, code)))
        .with_inspector(OverfundCreations);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT))
            .unwrap();
    assert!(result.result.is_success());
    assert_eq!(result.state[&CALLEE].info.nonce, 0, "the nonce was never bumped");
    assert_eq!(evm.ctx().additional_limit().usage(), LimitUsage::ZERO);
}

/// A creation that collides with an existing account fails after bumping the creator's nonce, so
/// the creator record stays and the created account's goes.
#[test]
fn test_colliding_creation_keeps_the_creator_record() {
    let code = append_value_create(BytecodeBuilder::default()).append(POP).append(STOP).build();
    let db = funded().account_code(CALLEE, code).account_nonce(CALLEE.create(0), 1);
    let (result, usage) = run(db, call(CALLER, CALLEE, U256::ZERO, GAS_LIMIT));
    assert!(result.result.is_success());
    assert_eq!(result.state[&CALLEE].info.nonce, 1, "the collision comes after the bump");
    assert_eq!(usage, records(1), "the creator's nonce only");
}
