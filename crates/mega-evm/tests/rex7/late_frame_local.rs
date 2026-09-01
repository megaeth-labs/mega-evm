//! REX7: a frame-local exceed that only becomes visible once the frame has been merged.
//!
//! A per-frame budget is the frame's usage weighed against its *caller's* budget after the merge,
//! so a frame can overrun one with nothing having noticed while it ran. The frame return is where
//! that is first detectable, and through REX6 it is detected one step too late to act on: the
//! merge has already happened on the frame's original classification, and so has the journal
//! decision. What the caller is handed is a revert over usage that was kept and state that was
//! committed.
//!
//! REX7 asks the question before the pop and writes the answer onto the frame's result first, so
//! all three follow one classification — the caller is told the frame reverted, the pop discards
//! the frame's usage the way it discards any reverting frame's, and the journal rolls the frame's
//! state back.
//!
//! # The construction
//!
//! Natural traffic produces no instance of this: a child frame is pushed with 98% of its caller's
//! remaining budget, so merging a child that stayed inside its own budget cannot push the caller
//! past its own. The one charge that breaks that arithmetic is REX6's creator nonce bump, which is
//! charged to the *caller's* lane after the child's budget has already been computed from the
//! caller's remaining. It costs one account-info write, so it can only tip the balance when 2% of
//! the caller's remaining data-size budget is under 40 bytes — a caller with under two kilobytes
//! left. The transaction below puts one there with an explicit runtime limit, and has it CREATE a
//! contract whose deployed code fills the child's budget almost exactly.
//!
//! Frame budgets, with `tx_data_size_limit` at 1171 and 150 bytes of intrinsic usage:
//!
//! | frame            | budget | usage                                          |
//! | ---------------- | ------ | ---------------------------------------------- |
//! | `CONTRACT`       |  1 021 | 0 while the call is out                        |
//! | `CALLEE`         |  1 000 | 40 — the creator nonce bump, charged after push |
//! | the constructor  |    980 | 970 = 40 account + 32 log + 898 deployed code   |
//!
//! 40 + 970 = 1 010 > 1 000: the constructor overran `CALLEE`'s budget, and nothing could have
//! seen it before the merge.

use crate::common::{transact, Outcome, CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{CALL, CREATE, ISZERO, LOG0, MSTORE, PUSH0, RETURN, SSTORE, STOP};

/// `CALLEE` stores 1 here when the CREATE it ran came back as a failure.
const CREATE_FAILED_SLOT: u64 = 0x11;
/// `CONTRACT` stores the CALL's own success flag here.
const CALL_RESULT_SLOT: u64 = 0x12;

/// Deployed code length, chosen so the constructor's frame lands just under its own budget and
/// just over its caller's — see the table in the module docs.
const DEPLOYED_CODE_LEN: u16 = 898;
/// Leaves `CONTRACT`'s frame 1 021 bytes of data-size budget, after 150 bytes of intrinsic usage.
const TX_DATA_SIZE_LIMIT: u64 = 1171;

/// Ample: the deposit alone is nearly 180 000 gas, and `MegaETH` charges storage gas on top.
const FORWARDED_GAS: u64 = 50_000_000;

/// Emits a log and returns [`DEPLOYED_CODE_LEN`] zero bytes of runtime code.
///
/// The log is what makes this a statement about receipts: it is emitted by a frame that runs to a
/// successful exit and is then failed by the merge.
fn constructor_code() -> Vec<u8> {
    let mut code = vec![PUSH0, PUSH0, LOG0, 0x61];
    code.extend_from_slice(&DEPLOYED_CODE_LEN.to_be_bytes());
    code.extend_from_slice(&[PUSH0, RETURN]);
    code
}

/// Runs the CREATE, records whether it failed, and emits a log of its own.
fn callee_code() -> Bytes {
    let constructor = constructor_code();
    let size = constructor.len();
    // `MSTORE` writes the pushed word right-aligned, so the constructor sits at the tail of the
    // first memory word.
    let offset = 32 - size;
    BytecodeBuilder::default()
        .push_bytes(&constructor)
        .append(PUSH0)
        .append(MSTORE)
        .push_number(size as u64)
        .push_number(offset as u64)
        .append(PUSH0)
        .append(CREATE)
        .append(ISZERO)
        .push_number(CREATE_FAILED_SLOT)
        .append(SSTORE)
        .append_many([PUSH0, PUSH0, LOG0])
        .append(STOP)
        .build()
}

/// Calls [`CALLEE`] and records whether that call survived.
fn contract_code() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // retSize
        .append(PUSH0) // retOffset
        .append(PUSH0) // argsSize
        .append(PUSH0) // argsOffset
        .append(PUSH0) // value
        .push_address(CALLEE)
        .push_number(FORWARDED_GAS)
        .append(CALL)
        .push_number(CALL_RESULT_SLOT)
        .append(SSTORE)
        .append(STOP)
        .build()
}

/// The address the CREATE would deploy to: `CALLEE`'s first creation.
fn created_address() -> Address {
    CALLEE.create(0)
}

fn run(spec: MegaSpecId, data_size_limit: u64) -> Outcome {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, contract_code())
        .account_code(CALLEE, callee_code());
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(data_size_limit))
}

fn deployed_code_len(outcome: &Outcome) -> usize {
    outcome
        .state
        .get(&created_address())
        .and_then(|account| account.info.code.as_ref())
        .map(|code| code.original_bytes().len())
        .unwrap_or(0)
}

/// With room to spare, the same transaction deploys: this is the fixture's own control, and it
/// pins the usage the budgets in the module docs are derived from.
#[test]
fn test_the_same_transaction_deploys_when_the_caller_has_room() {
    let outcome = run(MegaSpecId::REX7, u64::MAX);

    assert!(outcome.is_success(), "control: {:?}", outcome.result);
    assert_eq!(
        outcome.storage_value(CALLEE, U256::from(CREATE_FAILED_SLOT)),
        U256::ZERO,
        "control: the CREATE must succeed when nothing is binding",
    );
    assert_eq!(
        deployed_code_len(&outcome),
        usize::from(DEPLOYED_CODE_LEN),
        "control: the constructor's code must be deployed",
    );
    assert_eq!(
        outcome.data_size, 1232,
        "control: 150 intrinsic + 40 nonce bump + 970 constructor + 32 CALLEE log \
         + 40 CALLEE store"
    );
    assert_eq!(outcome.result.logs().len(), 2, "control: both logs must reach the receipt");
}

/// The whole of the settlement, on one transaction: the frame reverts, its usage is discarded, its
/// state is rolled back, and its caller carries on.
#[test]
fn test_a_late_frame_local_exceed_reverts_the_frame_and_discards_its_usage() {
    let outcome = run(MegaSpecId::REX7, TX_DATA_SIZE_LIMIT);

    assert!(outcome.is_success(), "the transaction itself must survive: {:?}", outcome.result);
    assert_eq!(
        outcome.storage_value(CALLEE, U256::from(CREATE_FAILED_SLOT)),
        U256::from(1),
        "the constructor's frame must come back to its caller as a failure",
    );
    assert_eq!(
        outcome.storage_value(CONTRACT, U256::from(CALL_RESULT_SLOT)),
        U256::from(1),
        "and its caller must be free to carry on — the exceed was the constructor's",
    );
    assert_eq!(
        deployed_code_len(&outcome),
        0,
        "a frame that reports a revert has reverted: no code may be deposited",
    );
    assert_eq!(
        outcome.state.get(&CALLEE).map(|account| account.info.nonce),
        Some(1),
        "the creator's nonce bump survives the child's revert, which is why it is charged to the \
         creator's own lane and why this shape exists at all",
    );
    assert_eq!(
        outcome.data_size, 302,
        "150 intrinsic + 40 nonce bump + 32 CALLEE log + 40 CALLEE store + 40 CONTRACT store: \
         the reverted frame's 970 bytes are discarded, not merged",
    );
}

/// The receipt of a *successful* transaction carries no log from the frame the merge failed.
///
/// `strip_logs_if_not_success` cannot be what removed it: that function returns a `Success` result
/// untouched. What removed it is the journal decision, which now follows the frame's final result
/// — the same rollback that left no deployed code behind. This is the pin that the strip is a
/// no-op under REX7 rather than the thing holding the receipt together.
#[test]
fn test_a_reverted_frames_log_never_reaches_a_successful_receipt() {
    let outcome = run(MegaSpecId::REX7, TX_DATA_SIZE_LIMIT);

    assert!(outcome.is_success(), "the strip does nothing to a success: {:?}", outcome.result);
    let logs = outcome.result.logs();
    assert_eq!(logs.len(), 1, "exactly one log survives: {logs:?}");
    assert_eq!(logs[0].address, CALLEE, "and it is the one the surviving frame emitted");
}

/// Frozen specs keep the split, so the same transaction ends differently there.
///
/// REX6 merges the constructor's usage on its original classification, then rewrites the result;
/// the caller resumes over its own budget and is failed on the spot. The caller's call fails, its
/// store never runs, and its log never reaches the receipt — none of which happens under REX7.
#[test]
fn test_frozen_specs_fail_the_caller_instead() {
    let outcome = run(MegaSpecId::REX6, TX_DATA_SIZE_LIMIT);

    assert!(outcome.is_success(), "the top-level frame still returns: {:?}", outcome.result);
    assert_eq!(
        outcome.storage_value(CONTRACT, U256::from(CALL_RESULT_SLOT)),
        U256::ZERO,
        "REX6: the caller is failed by its child's exceed, not just told about it",
    );
    assert_eq!(
        outcome.storage_value(CALLEE, U256::from(CREATE_FAILED_SLOT)),
        U256::ZERO,
        "REX6: the caller never gets to record the failure",
    );
    assert_eq!(outcome.result.logs().len(), 0, "REX6: nothing survives to the receipt");
    assert_eq!(outcome.data_size, 150, "REX6: the whole call frame's usage is discarded with it");
}
