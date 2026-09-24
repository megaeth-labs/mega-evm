//! `MegaAccessControl` steering gas detention's switch: a frame that calls
//! `disableVolatileDataAccess()` has every volatile read of its own and of the frames below it
//! refused with `VolatileDataAccessDisabled`, until it switches access back on or returns; a frame
//! below cannot switch back on what a frame above switched off; `isVolatileDataAccessDisabled()`
//! answers for the caller.
//!
//! The legacy engine's rows, on Satin's switch. The beneficiary is the block's, `Address::ZERO` in
//! these tests.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::{
        IMegaAccessControl, VolatileDataAccessType, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE,
        NON_ZERO_TRANSFER_REVERT_DATA, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    volatile_data_access_disabled_revert_data, ExternalEnvs, MegaContext, MegaEvm, MegaSpecId,
    MegaTransaction, MegaTransactionOutcome, TestExternalEnvs, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::*,
    context::{ContextTr, TxEnv},
    interpreter::{CallInputs, CallOutcome, InterpreterTypes},
    Inspector,
};

use crate::common::{block, system_db, CALLER, GAS_LIMIT};

const PARENT: Address = address!("0x0000000000000000000000000000000000200001");
const CHILD: Address = address!("0x0000000000000000000000000000000000200002");
const GRANDCHILD: Address = address!("0x0000000000000000000000000000000000200003");
const SIBLING: Address = address!("0x0000000000000000000000000000000000200004");

/// The block's beneficiary in these tests.
const BENEFICIARY: Address = Address::ZERO;

const DISABLE: [u8; 4] = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
const ENABLE: [u8; 4] = IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR;
const IS_DISABLED: [u8; 4] = IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR;
const DISABLED_BY_PARENT: [u8; 4] = IMegaAccessControl::DisabledByParent::SELECTOR;
const NOT_INTERCEPTED: [u8; 4] = IMegaAccessControl::NotIntercepted::SELECTOR;

/* ---------- running a transaction ---------- */

/// What a transaction did, and what gas detention made of its reads.
struct Run {
    outcome: MegaTransactionOutcome,
    accessed: VolatileDataAccess,
    limit: Option<u64>,
}

impl Run {
    /// The output of the transaction, which must have succeeded.
    fn output(&self) -> Bytes {
        assert!(self.outcome.result.is_success(), "{:?}", self.outcome.result);
        self.outcome.result.output().cloned().unwrap_or_default()
    }

    /// The output as a word.
    fn word(&self) -> U256 {
        U256::from_be_slice(&self.output())
    }

    /// The call status the log at `index` carries ([`log_status`]).
    fn logged_status(&self, index: usize) -> bool {
        let logs = self.outcome.result.logs();
        assert!(logs.len() > index, "{} logs, wanted {}", logs.len(), index + 1);
        U256::from_be_slice(&logs[index].data.data) == U256::ONE
    }
}

/// A transaction from [`CALLER`] to `to` with `data` and `value`, carrying `gas_limit`.
fn tx(to: Address, data: &[u8], value: U256, gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data: Bytes::copy_from_slice(data),
        value,
        gas_limit,
        ..Default::default()
    }))
}

/// Runs `tx` over `db`, with an oracle service that holds slot 0 of the Oracle.
fn execute(db: MemoryDatabase, tx: MegaTransaction) -> Run {
    let envs = TestExternalEnvs::<core::convert::Infallible>::new()
        .with_oracle_storage(U256::ZERO, U256::from(0x1234));
    let ctx = MegaContext::new_with_external_envs(db, MegaSpecId::SATIN, ExternalEnvs::from(envs))
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());
    let mut evm = MegaEvm::new(ctx);
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let detention = evm.ctx().detention();
    Run { outcome, accessed: detention.accessed(), limit: detention.compute_limit() }
}

/// Runs a call to [`PARENT`] with the contracts `code` holds deployed.
fn run(code: &[(Address, Bytes)]) -> Run {
    let db = code
        .iter()
        .fold(system_db(), |db, (address, code)| db.account_code(*address, code.clone()));
    execute(db, tx(PARENT, &[], U256::ZERO, GAS_LIMIT))
}

/* ---------- code ---------- */

/// Appends a `CALL` of `selector` on the access control contract, discarding its status.
fn steer(code: BytecodeBuilder, selector: [u8; 4]) -> BytecodeBuilder {
    code.mstore(0x0, selector)
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(4_u8) // argsSize
        .push_number(0_u8) // argsOffset
        .push_number(0_u8) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u32)
        .append_many([CALL, POP])
}

fn disable(code: BytecodeBuilder) -> BytecodeBuilder {
    steer(code, DISABLE)
}

fn enable(code: BytecodeBuilder) -> BytecodeBuilder {
    steer(code, ENABLE)
}

/// Appends a query of `isVolatileDataAccessDisabled()` and returns its answer.
fn query_and_return(code: BytecodeBuilder) -> BytecodeBuilder {
    code.mstore(0x0, IS_DISABLED)
        .push_number(32_u8) // retSize
        .push_number(0x20_u8) // retOffset
        .push_number(4_u8) // argsSize
        .push_number(0_u8) // argsOffset
        .push_number(0_u8) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u32)
        .append_many([CALL, POP])
        .push_number(32_u8)
        .push_number(0x20_u8)
        .append(RETURN)
}

/// Appends a call of `target` through `scheme` with `gas`, leaving its status on the stack.
fn call(code: BytecodeBuilder, scheme: u8, target: Address, gas: u64) -> BytecodeBuilder {
    let code = code.push_number(0_u8).push_number(0_u8).push_number(0_u8).push_number(0_u8);
    let code = if scheme == CALL || scheme == CALLCODE { code.push_number(0_u8) } else { code };
    code.push_address(target).push_number(gas).append(scheme)
}

/// Appends a `LOG0` of the status on the stack.
fn log_status(code: BytecodeBuilder) -> BytecodeBuilder {
    code.append_many([PUSH0, MSTORE]).push_number(32_u8).append_many([PUSH0, LOG0])
}

/// Appends a return of the word on the stack.
fn return_word(code: BytecodeBuilder) -> Bytes {
    code.append_many([PUSH0, MSTORE]).push_number(32_u8).append_many([PUSH0, RETURN]).build()
}

/// Appends a call of `target`, then returns what it returned or reverted with.
fn call_and_return_data(code: BytecodeBuilder, target: Address) -> Bytes {
    call(code, CALL, target, 50_000_000)
        .append(POP)
        .append_many([RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY, RETURNDATASIZE, PUSH0, RETURN])
        .build()
}

/// Code that runs `opcode` once and stops; `BLOCKHASH` and `BLOBHASH` are given an operand.
fn reads(opcode: u8) -> Bytes {
    let code = BytecodeBuilder::default();
    let code = if matches!(opcode, BLOCKHASH | BLOBHASH) { code.append(PUSH0) } else { code };
    code.append_many([opcode, POP, STOP]).build()
}

/// Code that runs `opcode` on the account at `target` and stops.
fn reads_account(opcode: u8, target: Address) -> Bytes {
    BytecodeBuilder::default().push_address(target).append_many([opcode, POP, STOP]).build()
}

/// The access type a `VolatileDataAccessDisabled` revert names.
fn refused_type(data: &[u8]) -> VolatileDataAccessType {
    IMegaAccessControl::VolatileDataAccessDisabled::abi_decode(data)
        .expect("a VolatileDataAccessDisabled revert")
        .accessType
}

/// The parent that switches access off, calls [`CHILD`] and logs its status.
fn disables_then_calls_child(scheme: u8) -> Bytes {
    log_status(call(disable(BytecodeBuilder::default()), scheme, CHILD, 50_000_000)).stop().build()
}

/* ---------- 1. disableVolatileDataAccess() ---------- */

/// A child that reads `TIMESTAMP` after its caller switched access off reverts.
#[test]
fn test_inner_call_timestamp_reverts() {
    let run = run(&[(PARENT, disables_then_calls_child(CALL)), (CHILD, reads(TIMESTAMP))]);
    assert!(run.outcome.result.is_success());
    assert!(!run.logged_status(0), "the child's read was refused");
}

/// The frame that switched access off is restricted itself.
#[test]
fn test_caller_frame_is_restricted() {
    let parent = disable(BytecodeBuilder::default()).append_many([TIMESTAMP, POP, STOP]).build();
    let run = run(&[(PARENT, parent)]);
    assert!(!run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(
        run.outcome.result.output().cloned().unwrap_or_default(),
        volatile_data_access_disabled_revert_data(VolatileDataAccess::TIMESTAMP),
    );
}

/// A child that reads nothing volatile is not restricted.
#[test]
fn test_inner_call_without_volatile_access_succeeds() {
    let child = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(2_u8)
        .append_many([ADD, POP, STOP])
        .build();
    let run = run(&[(PARENT, disables_then_calls_child(CALL)), (CHILD, child)]);
    assert!(run.logged_status(0));
}

/* ---------- 2. which opcodes are refused ---------- */

/// Every block-environment opcode is refused in a child, `SLOTNUM` included; `BLOBHASH` is not,
/// because it reads the transaction's own blob hashes, which Satin does not count as volatile.
#[test]
fn test_volatile_opcodes_all_revert_in_inner_call() {
    for (opcode, refused) in [
        (TIMESTAMP, true),
        (NUMBER, true),
        (COINBASE, true),
        (DIFFICULTY, true),
        (GASLIMIT, true),
        (BASEFEE, true),
        (BLOCKHASH, true),
        (BLOBBASEFEE, true),
        (SLOTNUM, true),
        (BLOBHASH, false),
    ] {
        let run = run(&[(PARENT, disables_then_calls_child(CALL)), (CHILD, reads(opcode))]);
        assert!(run.outcome.result.is_success(), "{opcode:#04x}");
        assert_eq!(!run.logged_status(0), refused, "{opcode:#04x}");
    }
}

/// The refusal reverts with `VolatileDataAccessDisabled(uint8)` naming what the opcode reads.
#[test]
fn test_revert_data_contains_error_with_access_type() {
    let parent = call_and_return_data(disable(BytecodeBuilder::default()), CHILD);
    let run = run(&[(PARENT, parent), (CHILD, reads(TIMESTAMP))]);
    let output = run.output();
    assert_eq!(output[..4], IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR);
    assert_eq!(refused_type(&output), VolatileDataAccessType::Timestamp);
}

/* ---------- 3. nested calls and call schemes ---------- */

/// The restriction reaches every frame below the one that switched it off: a grandchild's read is
/// refused, and the child, which read nothing, returns normally.
#[test]
fn test_nested_call_volatile_access_reverts() {
    let child =
        log_status(call(BytecodeBuilder::default(), CALL, GRANDCHILD, 40_000_000)).stop().build();
    let run = run(&[
        (PARENT, disables_then_calls_child(CALL)),
        (CHILD, child),
        (GRANDCHILD, reads(TIMESTAMP)),
    ]);
    assert!(run.outcome.result.is_success());
    assert!(!run.logged_status(0), "the grandchild was refused");
    assert!(run.logged_status(1), "the child returned");
}

/// A child reached with `STATICCALL` or `DELEGATECALL` is restricted too: the switch is by frame,
/// not by scheme.
#[test]
fn test_staticcall_also_restricted() {
    let run = run(&[(PARENT, disables_then_calls_child(STATICCALL)), (CHILD, reads(TIMESTAMP))]);
    assert!(!run.logged_status(0));
}

/// See [`test_staticcall_also_restricted`].
#[test]
fn test_delegatecall_also_restricted() {
    let run = run(&[(PARENT, disables_then_calls_child(DELEGATECALL)), (CHILD, reads(TIMESTAMP))]);
    assert!(!run.logged_status(0));
}

/* ---------- 4. the beneficiary's account ---------- */

/// An account opcode is refused on the beneficiary only: `BALANCE` and `EXTCODESIZE` of another
/// account are not volatile reads.
#[test]
fn test_balance_non_beneficiary_not_restricted() {
    let run =
        run(&[(PARENT, disables_then_calls_child(CALL)), (CHILD, reads_account(BALANCE, CHILD))]);
    assert!(run.logged_status(0));
}

/// See [`test_balance_non_beneficiary_not_restricted`].
#[test]
fn test_extcodesize_non_beneficiary_not_restricted() {
    let run = run(&[
        (PARENT, disables_then_calls_child(CALL)),
        (CHILD, reads_account(EXTCODESIZE, CHILD)),
    ]);
    assert!(run.logged_status(0));
}

/// `BALANCE` of the beneficiary is refused, naming the beneficiary.
#[test]
fn test_balance_beneficiary_restricted() {
    let parent = call_and_return_data(disable(BytecodeBuilder::default()), CHILD);
    let run = run(&[(PARENT, parent), (CHILD, reads_account(BALANCE, BENEFICIARY))]);
    assert_eq!(refused_type(&run.output()), VolatileDataAccessType::Beneficiary);
}

/// A call of the beneficiary is refused through each of the four schemes, naming the beneficiary.
#[test]
fn test_call_beneficiary_restricted() {
    for scheme in [CALL, STATICCALL, DELEGATECALL, CALLCODE] {
        let child = call(BytecodeBuilder::default(), scheme, BENEFICIARY, 100_000).stop().build();
        let parent = call_and_return_data(disable(BytecodeBuilder::default()), CHILD);
        let run = run(&[(PARENT, parent), (CHILD, child)]);
        assert_eq!(
            refused_type(&run.output()),
            VolatileDataAccessType::Beneficiary,
            "{scheme:#04x}"
        );
    }
}

/// A call of another account is not restricted.
#[test]
fn test_call_non_beneficiary_not_restricted() {
    let child = call(BytecodeBuilder::default(), CALL, GRANDCHILD, 100_000).stop().build();
    let run = run(&[
        (PARENT, disables_then_calls_child(CALL)),
        (CHILD, child),
        (GRANDCHILD, BytecodeBuilder::default().stop().build()),
    ]);
    assert!(run.logged_status(0));
}

/// Without the switch, calling the beneficiary is an ordinary call.
#[test]
fn test_call_beneficiary_not_restricted_without_disable() {
    let child = call(BytecodeBuilder::default(), CALL, BENEFICIARY, 100_000).stop().build();
    let parent =
        log_status(call(BytecodeBuilder::default(), CALL, CHILD, 50_000_000)).stop().build();
    let run = run(&[(PARENT, parent), (CHILD, child)]);
    assert!(run.logged_status(0));
}

/// `SELFDESTRUCT` to the beneficiary is refused, naming the beneficiary; to another account it is
/// not, and without the switch neither is.
#[test]
fn test_selfdestruct_beneficiary_restricted() {
    let child = reads_account(SELFDESTRUCT, BENEFICIARY);
    let parent = call_and_return_data(disable(BytecodeBuilder::default()), CHILD);
    let run = run(&[(PARENT, parent), (CHILD, child)]);
    assert_eq!(refused_type(&run.output()), VolatileDataAccessType::Beneficiary);
}

/// See [`test_selfdestruct_beneficiary_restricted`].
#[test]
fn test_selfdestruct_non_beneficiary_not_restricted() {
    let run = run(&[
        (PARENT, disables_then_calls_child(CALL)),
        (CHILD, reads_account(SELFDESTRUCT, GRANDCHILD)),
    ]);
    assert!(run.logged_status(0));
}

/// See [`test_selfdestruct_beneficiary_restricted`].
#[test]
fn test_selfdestruct_beneficiary_not_restricted_without_disable() {
    let parent =
        log_status(call(BytecodeBuilder::default(), CALL, CHILD, 50_000_000)).stop().build();
    let run = run(&[(PARENT, parent), (CHILD, reads_account(SELFDESTRUCT, BENEFICIARY))]);
    assert!(run.logged_status(0));
}

/* ---------- 5. a refused read reads nothing ---------- */

/// A refused read records nothing and sets no compute limit: the block environment, the
/// beneficiary's balance, a call of the beneficiary, a `SELFDESTRUCT` to it, and the Oracle's
/// storage.
#[test]
fn test_blocked_volatile_access_does_not_set_bitmap() {
    let oracle_read = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    for (name, target, child) in [
        ("TIMESTAMP", CHILD, reads(TIMESTAMP)),
        ("BALANCE", CHILD, reads_account(BALANCE, BENEFICIARY)),
        (
            "CALL",
            CHILD,
            call(BytecodeBuilder::default(), CALL, BENEFICIARY, 100_000).stop().build(),
        ),
        ("SELFDESTRUCT", CHILD, reads_account(SELFDESTRUCT, BENEFICIARY)),
        ("SLOAD", ORACLE_CONTRACT_ADDRESS, oracle_read),
    ] {
        let parent =
            log_status(call(disable(BytecodeBuilder::default()), CALL, target, 50_000_000))
                .stop()
                .build();
        let run = run(&[(PARENT, parent), (target, child)]);
        assert!(run.outcome.result.is_success(), "{name}");
        assert!(!run.logged_status(0), "{name}: refused");
        assert_eq!(run.accessed, VolatileDataAccess::empty(), "{name}");
        assert_eq!(run.limit, None, "{name}");
    }
}

/// The Oracle's storage is refused in the Oracle's own frame below a frame that switched access
/// off, naming the Oracle.
#[test]
fn test_oracle_sload_reverts_when_volatile_access_disabled() {
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    let parent = call_and_return_data(disable(BytecodeBuilder::default()), ORACLE_CONTRACT_ADDRESS);
    let run = run(&[(PARENT, parent), (ORACLE_CONTRACT_ADDRESS, oracle)]);
    assert_eq!(refused_type(&run.output()), VolatileDataAccessType::Oracle);
}

/// A creation whose init code reads volatile data fails below a frame that switched access off.
#[test]
fn test_create_reverts_when_volatile_access_disabled() {
    let init_code =
        BytecodeBuilder::default().append_many([TIMESTAMP, POP, PUSH0, PUSH0, RETURN]).build();
    let parent = disable(BytecodeBuilder::default())
        .mstore(0x40, &init_code)
        .push_number(init_code.len() as u64)
        .push_number(0x40_u8)
        .push_number(0_u8)
        .append(CREATE);
    let run = run(&[(PARENT, return_word(parent))]);
    assert_eq!(run.word(), U256::ZERO, "the creation failed");
}

/// A parent that read volatile data before switching access off still has its child refused:
/// what the transaction read already does not lift the switch.
#[test]
fn test_parent_accesses_volatile_then_child_restricted() {
    let parent = call_and_return_data(
        disable(BytecodeBuilder::default().append_many([TIMESTAMP, POP])),
        CHILD,
    );
    let run = run(&[(PARENT, parent), (CHILD, reads(TIMESTAMP))]);
    assert_eq!(refused_type(&run.output()), VolatileDataAccessType::Timestamp);
    assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP, "the parent's own read counts");
}

/* ---------- 6. the switch's scope ---------- */

/// A sibling called after the frame that switched access off returned is not restricted, and
/// neither is its child.
#[test]
fn test_sibling_call_not_restricted() {
    let c1 = disable(BytecodeBuilder::default()).stop().build();
    let c2 = return_word(call(BytecodeBuilder::default(), CALL, GRANDCHILD, 40_000_000));
    let parent = call(BytecodeBuilder::default(), CALL, CHILD, 50_000_000)
        .append(POP)
        .push_number(32_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(0_u8) // argsSize
        .push_number(0_u8) // argsOffset
        .push_number(0_u8) // value
        .push_address(SIBLING)
        .push_number(50_000_000_u32)
        .append_many([CALL, POP])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN])
        .build();
    let run = run(&[(PARENT, parent), (CHILD, c1), (SIBLING, c2), (GRANDCHILD, reads(TIMESTAMP))]);
    assert_eq!(run.word(), U256::ONE, "the sibling's child read the timestamp");
}

/// A switch set by a child that then reverted goes with the child: a sibling reads.
#[test]
fn test_disable_in_reverted_child_does_not_affect_sibling() {
    let child = disable(BytecodeBuilder::default()).revert().build();
    let parent = log_status(call(BytecodeBuilder::default(), CALL, CHILD, 50_000_000));
    let parent = return_word(call(parent, CALL, SIBLING, 50_000_000));
    let run = run(&[(PARENT, parent), (CHILD, child), (SIBLING, reads(TIMESTAMP))]);
    assert!(!run.logged_status(0), "the child reverted");
    assert_eq!(run.word(), U256::ONE, "the sibling read");
}

/* ---------- 7. enableVolatileDataAccess() ---------- */

/// The frame that switched access off can switch it back on, and its child then reads.
#[test]
fn test_enable_after_disable_succeeds() {
    let parent = enable(disable(BytecodeBuilder::default()));
    let run = run(&[
        (PARENT, return_word(call(parent, CALL, CHILD, 50_000_000))),
        (CHILD, reads(TIMESTAMP)),
    ]);
    assert_eq!(run.word(), U256::ONE);
}

/// A child cannot switch back on what its caller switched off: its call reverts with
/// `DisabledByParent()`.
#[test]
fn test_enable_by_child_reverts_when_parent_disabled() {
    let child = enable(BytecodeBuilder::default())
        .append_many([RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY, RETURNDATASIZE, PUSH0, RETURN])
        .build();
    let parent = call_and_return_data(disable(BytecodeBuilder::default()), CHILD);
    let run = run(&[(PARENT, parent), (CHILD, child)]);
    assert_eq!(run.output(), Bytes::from_static(&DISABLED_BY_PARENT));
}

/// Switching on what is not off succeeds and changes nothing.
#[test]
fn test_enable_when_not_disabled_is_noop() {
    let run = run(&[(PARENT, enable(BytecodeBuilder::default()).append(TIMESTAMP).stop().build())]);
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP);
}

/// A `STATICCALL` steers the switch as a `CALL` does: the switch is not state.
#[test]
fn test_staticcall_disable_volatile_data_access_is_intercepted() {
    let static_steer = |code: BytecodeBuilder, selector: [u8; 4]| {
        code.mstore(0x0, selector)
            .append_many([PUSH0, PUSH0])
            .push_number(4_u8)
            .append(PUSH0)
            .push_address(ACCESS_CONTROL_ADDRESS)
            .push_number(100_000_u32)
            .append(STATICCALL)
    };
    let parent = log_status(static_steer(BytecodeBuilder::default(), DISABLE));
    let parent = log_status(call(parent, CALL, CHILD, 50_000_000)).stop().build();
    let run1 = run(&[(PARENT, parent), (CHILD, reads(TIMESTAMP))]);
    assert!(run1.logged_status(0), "the static call succeeded");
    assert!(!run1.logged_status(1), "and switched access off");

    let parent = log_status(static_steer(disable(BytecodeBuilder::default()), ENABLE));
    let parent = log_status(call(parent, CALL, CHILD, 50_000_000)).stop().build();
    let run2 = run(&[(PARENT, parent), (CHILD, reads(TIMESTAMP))]);
    assert!(run2.logged_status(0), "the static call succeeded");
    assert!(run2.logged_status(1), "and switched access back on");
}

/* ---------- 8. isVolatileDataAccessDisabled() ---------- */

/// The query answers `false` where nothing switched access off, `true` in the frame that did, and
/// `true` in a child of it.
#[test]
fn test_query_returns_false_when_not_disabled() {
    let run = run(&[(PARENT, query_and_return(BytecodeBuilder::default()).build())]);
    assert_eq!(run.word(), U256::ZERO);
}

/// See [`test_query_returns_false_when_not_disabled`].
#[test]
fn test_query_returns_true_for_disabling_frame() {
    let run = run(&[(PARENT, query_and_return(disable(BytecodeBuilder::default())).build())]);
    assert_eq!(run.word(), U256::ONE);
}

/// See [`test_query_returns_false_when_not_disabled`].
#[test]
fn test_query_returns_true_when_parent_disabled() {
    let parent = call_and_return_data(disable(BytecodeBuilder::default()), CHILD);
    let run =
        run(&[(PARENT, parent), (CHILD, query_and_return(BytecodeBuilder::default()).build())]);
    assert_eq!(run.word(), U256::ONE);
}

/// The answer at four depths, with the switch set at the second: `false` above it, before it is
/// set and after the frame that set it returned; `true` in that frame and in the two below it.
#[test]
fn test_the_query_answers_for_the_caller_at_every_depth() {
    // Each frame queries and keeps its answer in memory at 0x80.. by its own slot, calls the next,
    // and returns all the answers it knows of.
    let query_into = |code: BytecodeBuilder, slot: u64| {
        code.mstore(0x0, IS_DISABLED)
            .push_number(32_u8)
            .push_number(0x80 + 0x20 * slot)
            .push_number(4_u8)
            .push_number(0_u8)
            .push_number(0_u8)
            .push_address(ACCESS_CONTROL_ADDRESS)
            .push_number(100_000_u32)
            .append_many([CALL, POP])
    };
    // Calls `target`, copying what it returns to 0x80 + 0x20 * `from`.
    let call_into = |code: BytecodeBuilder, target: Address, from: u64, words: u64| {
        code.push_number(0x20 * words)
            .push_number(0x80 + 0x20 * from)
            .push_number(0_u8)
            .push_number(0_u8)
            .push_number(0_u8)
            .push_address(target)
            .push_number(10_000_000_u32)
            .append_many([CALL, POP])
    };
    let returning = |code: BytecodeBuilder, from: u64, words: u64| {
        code.push_number(0x20 * words).push_number(0x80 + 0x20 * from).append(RETURN).build()
    };
    // Depth 3: queries.
    let grandchild_child = returning(query_into(BytecodeBuilder::default(), 3), 3, 1);
    // Depth 2: queries, calls depth 3.
    let grandchild =
        returning(call_into(query_into(BytecodeBuilder::default(), 2), SIBLING, 3, 1), 2, 2);
    // Depth 1: queries, switches off, queries, calls depth 2.
    let child = query_into(BytecodeBuilder::default(), 0);
    let child = call_into(query_into(disable(child), 1), GRANDCHILD, 2, 2);
    let child = returning(child, 0, 4);
    // Depth 0: queries before and after the child.
    let parent = query_into(BytecodeBuilder::default(), 5);
    let parent = query_into(call_into(parent, CHILD, 0, 4), 4);
    let parent = returning(parent, 0, 6);

    let run = run(&[
        (PARENT, parent),
        (CHILD, child),
        (GRANDCHILD, grandchild),
        (SIBLING, grandchild_child),
    ]);
    let output = run.output();
    let answers: Vec<bool> =
        output.chunks(32).map(|word| U256::from_be_slice(word) == U256::ONE).collect();
    assert_eq!(
        answers,
        vec![
            false, // depth 1, before it switches access off
            true,  // depth 1, after
            true,  // depth 2
            true,  // depth 3
            false, // depth 0, after depth 1 returned
            false, // depth 0, before calling depth 1
        ]
    );
}

/* ---------- 9. a transaction calling the contract directly ---------- */

/// A transaction that calls the contract directly is answered: the switches succeed, the query
/// answers `false`, a value-bearing call is refused with `NonZeroTransfer()`, and a selector the
/// contract does not intercept reverts in its bytecode with `NotIntercepted()`.
#[test]
fn test_direct_tx_disable_volatile_data_access() {
    for selector in [DISABLE, ENABLE] {
        let run =
            execute(system_db(), tx(ACCESS_CONTROL_ADDRESS, &selector, U256::ZERO, GAS_LIMIT));
        assert!(run.output().is_empty());
    }
}

/// See [`test_direct_tx_disable_volatile_data_access`].
#[test]
fn test_direct_tx_disable_volatile_data_access_with_value_reverts() {
    let run = execute(system_db(), tx(ACCESS_CONTROL_ADDRESS, &DISABLE, U256::ONE, GAS_LIMIT));
    assert!(!run.outcome.result.is_success());
    assert_eq!(
        run.outcome.result.output().cloned().unwrap_or_default(),
        Bytes::from_static(&NON_ZERO_TRANSFER_REVERT_DATA)
    );
}

/// See [`test_direct_tx_disable_volatile_data_access`].
#[test]
fn test_direct_tx_is_volatile_data_access_disabled() {
    let run = execute(system_db(), tx(ACCESS_CONTROL_ADDRESS, &IS_DISABLED, U256::ZERO, GAS_LIMIT));
    assert_eq!(run.word(), U256::ZERO);
}

/// See [`test_direct_tx_disable_volatile_data_access`].
#[test]
fn test_direct_tx_unknown_selector_falls_through_and_reverts_not_intercepted() {
    let run = execute(
        system_db(),
        tx(ACCESS_CONTROL_ADDRESS, &[0xde, 0xad, 0xbe, 0xef], U256::ZERO, GAS_LIMIT),
    );
    assert!(!run.outcome.result.is_success());
    assert_eq!(
        run.outcome.result.output().cloned().unwrap_or_default(),
        Bytes::from_static(&NOT_INTERCEPTED)
    );
}

/* ---------- 10. schemes that do not reach the interceptor ---------- */

/// `DELEGATECALL` and `CALLCODE` of `disableVolatileDataAccess()` run the contract's bytecode,
/// which reverts with `NotIntercepted()`, and switch nothing off: the child reads.
#[test]
fn test_delegatecall_to_access_control_not_intercepted() {
    for scheme in [DELEGATECALL, CALLCODE] {
        let parent = BytecodeBuilder::default().mstore(0x0, DISABLE);
        let parent = parent.append_many([PUSH0, PUSH0]).push_number(4_u8).append(PUSH0);
        let parent = if scheme == CALLCODE { parent.append(PUSH0) } else { parent };
        let parent =
            parent.push_address(ACCESS_CONTROL_ADDRESS).push_number(100_000_u32).append(scheme);
        let parent = return_word(call(log_status(parent), CALL, CHILD, 50_000_000));
        let run = run(&[
            (PARENT, parent),
            (CHILD, reads(TIMESTAMP)),
            (ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE),
        ]);
        assert!(!run.logged_status(0), "{scheme:#04x}: the bytecode reverted");
        assert_eq!(run.word(), U256::ONE, "{scheme:#04x}: and the child read");
    }
}

/* ---------- 11. an inspector sees the answered call ---------- */

/// Records the targets of every call an inspector is shown, as it starts and as it ends.
#[derive(Default)]
struct Calls {
    starts: Vec<Address>,
    ends: Vec<Address>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for Calls {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.starts.push(inputs.target_address);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.ends.push(inputs.target_address);
    }
}

/// An inspector sees the call the interceptor answered start and end like any other.
#[test]
fn test_inspector_sees_system_contract_call() {
    let child = BytecodeBuilder::default().append_many([PUSH0, POP, STOP]).build();
    let db = system_db()
        .account_code(PARENT, disables_then_calls_child(CALL))
        .account_code(CHILD, child);
    let ctx = MegaContext::new(db, MegaSpecId::SATIN)
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());
    let mut evm = MegaEvm::new(ctx).with_inspector(Calls::default());
    let outcome = evm.execute_transaction(tx(PARENT, &[], U256::ZERO, GAS_LIMIT)).unwrap();
    assert!(outcome.result.is_success());
    assert_eq!(evm.inspector().starts, vec![PARENT, ACCESS_CONTROL_ADDRESS, CHILD]);
    assert_eq!(evm.inspector().ends, vec![ACCESS_CONTROL_ADDRESS, CHILD, PARENT]);
}

/* ---------- 12. below and above the execution cap ---------- */

/// The switch behaves the same whether the transaction carries a state-gas reservoir or not, and
/// the transaction costs the same: the answers carry the reservoir back untouched.
#[test]
fn test_the_switch_is_the_same_below_and_above_the_execution_cap() {
    let parent = log_status(call(disable(BytecodeBuilder::default()), CALL, CHILD, 50_000_000));
    let parent = log_status(call(enable(parent), CALL, CHILD, 50_000_000)).stop().build();
    let at = |gas_limit: u64| {
        let db =
            system_db().account_code(PARENT, parent.clone()).account_code(CHILD, reads(TIMESTAMP));
        execute(db, tx(PARENT, &[], U256::ZERO, gas_limit))
    };
    let (below, above) = (at(GAS_LIMIT), at(TX_GAS_LIMIT_CAP + 100_000_000));
    for run in [&below, &above] {
        assert!(run.outcome.result.is_success());
        assert!(!run.logged_status(0), "refused while off");
        assert!(run.logged_status(1), "read once on again");
    }
    assert_eq!(below.outcome.result.tx_gas_used(), above.outcome.result.tx_gas_used());
    let (below, above) = (below.outcome.gas, above.outcome.gas);
    assert_eq!(
        (below.regular, below.state, below.history),
        (above.regular, above.state, above.history),
        "every ledger is the same",
    );
    assert_eq!(below.reservoir_remaining, 0, "there is no reservoir below the cap");
    assert_eq!(
        above.reservoir_remaining,
        100_000_000 - above.history,
        "above it, the reservoir paid the history and the answers carried the rest back",
    );
}
