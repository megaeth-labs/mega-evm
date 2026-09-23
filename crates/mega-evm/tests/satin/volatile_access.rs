//! Volatile reads the legacy engine's tests pinned, on Satin's gas detention: what records a
//! read, which read sets the limit, where the Oracle's storage is read and where it is not, who
//! is exempt, and how a refused read keeps the failures an opcode has on its own.
//!
//! The legacy engine's switch was steered through `MegaAccessControl`; here the tests switch it
//! off with `MegaContext::with_volatile_access_disabled_from`, as if the frame at that depth had
//! called `disableVolatileDataAccess()` first.

use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::ORACLE_ACCESS_COMPUTE_GAS,
    system::{
        IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{op_transaction, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    volatile_data_access_disabled_revert_data, EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaEvm,
    VolatileDataAccess,
};
use revm::{bytecode::opcode::*, context::TxEnv, interpreter::InstructionResult, state::Bytecode};

use crate::detention::{
    assert_stopped, call, context, execute, intrinsic, on_beneficiary, op, run_on, spin, tx,
    with_delegation, work, Calls, BELOW, BENEFICIARY, CALLER, CAP, CHILD, CONTRACT, DELEGATOR,
    TIERS,
};

/// A second contract, never the beneficiary.
const OTHER: Address = address!("0000000000000000000000000000000000d00004");

/// Rounds of [`work`] that spend more compute than the cap: 21,756,000.
const PAST_THE_CAP: u32 = 7_000;

/* ---------- what records a read ---------- */

/// Every block-environment opcode records its own kind; the transaction's and the chain's own
/// fields, and a contract's own balance, record nothing.
#[test]
fn test_the_block_environment_opcodes_record_their_kind_and_nothing_else_does() {
    let reads: [(u8, VolatileDataAccess); 9] = [
        (NUMBER, VolatileDataAccess::BLOCK_NUMBER),
        (TIMESTAMP, VolatileDataAccess::TIMESTAMP),
        (COINBASE, VolatileDataAccess::COINBASE),
        (DIFFICULTY, VolatileDataAccess::PREV_RANDAO),
        (GASLIMIT, VolatileDataAccess::GAS_LIMIT),
        (BASEFEE, VolatileDataAccess::BASE_FEE),
        (BLOBBASEFEE, VolatileDataAccess::BLOB_BASE_FEE),
        (SLOTNUM, VolatileDataAccess::SLOT_NUM),
        (PUSH0, VolatileDataAccess::empty()),
    ];
    for (opcode, access) in reads {
        let code = op(BytecodeBuilder::default(), opcode).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, BELOW),
        );
        assert!(run.outcome.result.is_success());
        assert_eq!(run.accessed, access, "{opcode:#04x}");
        assert_eq!(run.accessed.count_block_env_accessed(), usize::from(!access.is_empty()));
    }
    for opcode in [
        revm::bytecode::opcode::CALLER,
        ORIGIN,
        GASPRICE,
        CHAINID,
        ADDRESS,
        CALLVALUE,
        SELFBALANCE,
        GAS,
    ] {
        let code = op(BytecodeBuilder::default(), opcode).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, BELOW),
        );
        assert!(run.outcome.result.is_success());
        assert_eq!(run.accessed, VolatileDataAccess::empty(), "{opcode:#04x}");
        assert_eq!(run.limit, None, "{opcode:#04x}");
    }
}

/// A transaction that reads several kinds records all of them, and the first read's limit, the
/// lowest, is the one that holds.
#[test]
fn test_several_reads_record_every_kind_and_the_first_limit_holds() {
    for gas_limit in TIERS {
        let code = op(BytecodeBuilder::default(), NUMBER);
        let code = op(work(code, 300), TIMESTAMP);
        let code = op(work(code, 300), BASEFEE);
        let code = work(code, 300).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success());
        assert_eq!(
            run.accessed,
            VolatileDataAccess::BLOCK_NUMBER |
                VolatileDataAccess::TIMESTAMP |
                VolatileDataAccess::BASE_FEE
        );
        assert_eq!(run.accessed.count_block_env_accessed(), 3);
        assert_eq!(run.limit, Some(2 + CAP), "NUMBER, the first read, set the limit");
    }
}

/// Detention belongs to one transaction: the next one on the same EVM starts with nothing read
/// and no limit, and computes past the cap its predecessor was held to.
#[test]
fn test_detention_starts_afresh_for_every_transaction() {
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    let reader = op(call(BytecodeBuilder::default(), CALL, ORACLE_CONTRACT_ADDRESS), TIMESTAMP);
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, reader.stop().build())
        .account_code(ORACLE_CONTRACT_ADDRESS, oracle)
        .account_code(OTHER, work(BytecodeBuilder::default(), PAST_THE_CAP).stop().build());
    let mut evm = MegaEvm::new(context(db));

    let first = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(first.outcome.result.is_success());
    assert_eq!(first.accessed, VolatileDataAccess::ORACLE | VolatileDataAccess::TIMESTAMP);
    assert!(first.limit.is_some());

    let second = run_on(&mut evm, tx(CALLER, OTHER, BELOW));
    assert!(second.outcome.result.is_success(), "{:?}", second.outcome.result);
    assert_eq!(second.accessed, VolatileDataAccess::empty());
    assert_eq!(second.limit, None);
    assert!(second.outcome.gas.regular > CAP);
}

/// A read that stays within the cap costs what it ran: the gas detention withheld goes back
/// when the transaction ends, below and above the execution cap.
#[test]
fn test_a_read_within_the_cap_bills_what_ran() {
    for gas_limit in TIERS {
        for opcode in [TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE] {
            // The read, a word stored, a short loop, and the word returned.
            let code = BytecodeBuilder::default().append(opcode).push_number(0_u8).append(MSTORE);
            let code = work(code, 10).push_number(32_u8).push_number(0_u8).append(RETURN).build();
            let run = execute(
                MemoryDatabase::default().account_code(CONTRACT, code),
                tx(CALLER, CONTRACT, gas_limit),
            );
            assert!(run.outcome.result.is_success(), "{opcode:#04x}");
            assert!(run.limit.is_some(), "{opcode:#04x}");
            let used = run.outcome.result.gas().tx_gas_used();
            assert!(used < 100_000, "{opcode:#04x}: {used}, not the gas it was given");
        }
        let code = BytecodeBuilder::default().push_number(299_u16).append(BLOCKHASH).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success());
        assert!(run.outcome.result.gas().tx_gas_used() < 100_000);
    }
}

/* ---------- which read sets the limit ---------- */

/// A read two frames down caps every caller: the middle frame and the transaction's own frame
/// resume on what the limit leaves them, and a transaction that stays within it bills what ran.
#[test]
fn test_a_read_two_frames_down_caps_every_caller() {
    let nested = on_beneficiary(BytecodeBuilder::default(), BALANCE).stop().build();
    let middle = work(call(BytecodeBuilder::default(), CALL, DELEGATOR), 10).stop().build();
    for gas_limit in TIERS {
        let db = |top: Bytes| {
            MemoryDatabase::default()
                .account_code(CONTRACT, top)
                .account_code(CHILD, middle.clone())
                .account_code(DELEGATOR, nested.clone())
        };
        let run = execute(
            db(spin(call(BytecodeBuilder::default(), CALL, CHILD))),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let limit = assert_stopped(&run, intrinsic(gas_limit));
        assert!(limit < CAP + 100_000, "the read was the third frame's first opcode: {limit}");
        assert_eq!(run.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);

        let run = execute(
            db(call(BytecodeBuilder::default(), CALL, CHILD).stop().build()),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success());
        assert!(run.outcome.result.gas().tx_gas_used() < 200_000);
    }
}

/// A caller that already read keeps its own limit through a child that reads again.
#[test]
fn test_a_childs_second_read_leaves_the_callers_limit() {
    let child = op(BytecodeBuilder::default(), NUMBER).stop().build();
    let parent = work(op(BytecodeBuilder::default(), TIMESTAMP), 300);
    let parent = call(parent, CALL, CHILD).stop().build();
    for gas_limit in TIERS {
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(run.outcome.result.is_success());
        assert_eq!(run.limit, Some(2 + CAP), "TIMESTAMP, the caller's first opcode, set it");
        assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP | VolatileDataAccess::BLOCK_NUMBER);
    }
}

/// A read in a frame that reverts still happened: the limit it set outlives the frame, and the
/// caller that resumes is held to it.
#[test]
fn test_a_read_in_a_frame_that_reverts_still_caps() {
    let beneficiary = BytecodeBuilder::default().revert().build();
    let child = call(BytecodeBuilder::default(), CALL, BENEFICIARY).revert().build();
    let parent = spin(call(BytecodeBuilder::default(), CALL, CHILD));
    for gas_limit in TIERS {
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone())
            .account_code(BENEFICIARY, beneficiary.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        let limit = assert_stopped(&run, intrinsic(gas_limit));
        assert!(limit < CAP + 100_000, "{limit}");
    }
}

/// A frame that reads and then crosses another transaction-level limit is stopped by that
/// limit: detention holds compute back and leaves the data size to its own limit.
#[test]
fn test_detention_leaves_a_data_size_stop_to_the_data_size_limit() {
    let code = call(BytecodeBuilder::default(), CALL, BENEFICIARY)
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .stop()
        .build();
    // Room above the body for one write record, not two.
    let data_limit = mega_evm::TX_BODY_SIZE + 60;
    for gas_limit in TIERS {
        let db = MemoryDatabase::default().account_code(CONTRACT, code.clone());
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(data_limit),
        ));
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
        assert!(run.limit.is_some(), "the call read the beneficiary");
        assert!(
            matches!(
                run.outcome.limit_exceeded,
                Some(LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, .. })
            ),
            "{:?}",
            run.outcome.limit_exceeded
        );
    }
}

/* ---------- the Oracle ---------- */

/// A frame reading the Oracle's slot 0, then spinning.
fn oracle_reads_then_spins() -> Bytes {
    spin(BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]))
}

/// Calling the Oracle is not reading its storage: without a load of a slot, nothing is detained,
/// and the caller computes past the cap.
#[test]
fn test_calling_the_oracle_without_reading_its_storage_detains_nothing() {
    let oracle = BytecodeBuilder::default().stop().build();
    let code = work(call(BytecodeBuilder::default(), CALL, ORACLE_CONTRACT_ADDRESS), PAST_THE_CAP);
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code.stop().build())
        .account_code(ORACLE_CONTRACT_ADDRESS, oracle);
    let run = execute(db, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(run.limit, None);
    assert!(run.outcome.gas.regular > CAP);
}

/// The Oracle's own code is held to the cap once it read its storage, whether the transaction
/// calls it directly or through a frame, and whether the call is a `CALL` or a `STATICCALL`.
#[test]
fn test_the_oracle_reading_its_own_storage_is_held_to_the_cap() {
    assert_eq!(ORACLE_ACCESS_COMPUTE_GAS, CAP);
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let db = || {
            MemoryDatabase::default()
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle_reads_then_spins())
        };

        let run = execute(db(), tx(CALLER, ORACLE_CONTRACT_ADDRESS, gas_limit));
        assert_eq!(
            assert_stopped(&run, intrinsic),
            CAP + 2 + 2_100,
            "the direct call: PUSH0 and a cold SLOAD"
        );
        assert_eq!(run.accessed, VolatileDataAccess::ORACLE);

        for scheme in [CALL, STATICCALL] {
            let code =
                call(BytecodeBuilder::default(), scheme, ORACLE_CONTRACT_ADDRESS).stop().build();
            let run = execute(db().account_code(CONTRACT, code), tx(CALLER, CONTRACT, gas_limit));
            assert_stopped(&run, intrinsic);
            assert_eq!(run.accessed, VolatileDataAccess::ORACLE, "{scheme:#04x}");
        }
    }
}

/// `DELEGATECALL` and `CALLCODE` run the Oracle's code on the caller's storage: what they read is
/// the caller's, not the Oracle's, and nothing is detained.
#[test]
fn test_the_oracles_code_on_another_accounts_storage_reads_nothing_volatile() {
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    for scheme in [DELEGATECALL, CALLCODE] {
        let code =
            work(call(BytecodeBuilder::default(), scheme, ORACLE_CONTRACT_ADDRESS), PAST_THE_CAP);
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, code.stop().build())
            .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone());
        let run = execute(db, tx(CALLER, CONTRACT, BELOW));
        assert!(run.outcome.result.is_success(), "{scheme:#04x}: {:?}", run.outcome.result);
        assert_eq!(run.limit, None, "{scheme:#04x}");
    }
}

/// A hint goes to the node's oracle service and reads nothing: `sendHint` detains nothing.
#[test]
fn test_a_hint_detains_nothing() {
    let hint =
        IOracle::sendHintCall { topic: B256::ZERO, data: Bytes::from_static(b"hint") }.abi_encode();
    let code = BytecodeBuilder::default()
        .mstore(0, &hint)
        .append_many([PUSH0, PUSH0])
        .push_number(hint.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL, POP]);
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, work(code, PAST_THE_CAP).stop().build())
        .account_code(ORACLE_CONTRACT_ADDRESS, BytecodeBuilder::default().stop().build());
    let run = execute(db, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(run.limit, None);
}

/// The system address maintains the Oracle: its transaction reads the Oracle's storage and is not
/// detained. The same code run for anyone else is.
#[test]
fn test_the_system_address_is_not_detained() {
    let oracle = work(BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]), PAST_THE_CAP)
        .stop()
        .build();
    let db = || MemoryDatabase::default().account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone());

    let system = op_transaction(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        gas_limit: BELOW,
        chain_id: Some(1),
        ..Default::default()
    });
    let run = execute(db(), alloy_op_evm::OpTx(system));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(run.accessed, VolatileDataAccess::empty());
    assert_eq!(run.limit, None);
    assert!(run.outcome.gas.regular > CAP);

    let run = execute(db(), tx(CALLER, ORACLE_CONTRACT_ADDRESS, BELOW));
    assert!(
        matches!(
            run.outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, .. })
        ),
        "{:?}",
        run.outcome.result
    );
}

/* ---------- the beneficiary's own frame ---------- */

/// The beneficiary's own frame reads its balance with `SELFBALANCE` and destroys itself with
/// `SELFDESTRUCT`; a transaction that reaches it is detained from its start. Another contract's
/// `SELFBALANCE` reads nothing volatile.
#[test]
fn test_the_beneficiarys_own_frame_is_detained() {
    for gas_limit in TIERS {
        let code = spin(op(BytecodeBuilder::default(), SELFBALANCE));
        let run = execute(
            MemoryDatabase::default().account_code(BENEFICIARY, code),
            tx(CALLER, BENEFICIARY, gas_limit),
        );
        assert_eq!(assert_stopped(&run, intrinsic(gas_limit)), CAP);

        let code = BytecodeBuilder::default().push_address(OTHER).append(SELFDESTRUCT).build();
        let db = MemoryDatabase::default()
            .account_code(BENEFICIARY, code)
            .account_balance(BENEFICIARY, U256::from(7));
        let run = execute(db, tx(CALLER, BENEFICIARY, gas_limit));
        assert!(run.outcome.result.is_success());
        assert_eq!(run.limit, Some(CAP));
        assert_eq!(run.outcome.state[&OTHER].info.balance, U256::from(7));

        let code = work(op(BytecodeBuilder::default(), SELFBALANCE), PAST_THE_CAP).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success());
        assert_eq!(run.limit, None);
    }
}

/// A `SELFDESTRUCT` to the beneficiary moves the balance and detains the transaction.
#[test]
fn test_a_selfdestruct_to_the_beneficiary_moves_the_balance_and_detains() {
    let child = on_beneficiary(BytecodeBuilder::default(), SELFDESTRUCT).build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, CHILD).stop().build())
        .account_code(CHILD, child)
        .account_balance(CHILD, U256::from(9));
    let run = execute(db, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(run.outcome.state[&BENEFICIARY].info.balance, U256::from(9));
    assert_eq!(run.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);
    assert!(run.limit.is_some());
}

/* ---------- refused reads ---------- */

/// Runs `CONTRACT`, which calls `CHILD` with everything it has, with volatile-data access off
/// from the child down, and returns what the child's frame ended with.
fn refused_child(db: MemoryDatabase) -> (InstructionResult, Bytes) {
    let db =
        db.account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, CHILD).stop().build());
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
        .with_inspector(Calls::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "the caller resumes: {:?}", run.outcome.result);
    let child =
        evm.inspector().calls.iter().find(|call| call.target == CHILD).expect("the child ran");
    (child.result, child.output.clone())
}

fn refused(access: VolatileDataAccess) -> (InstructionResult, Bytes) {
    (InstructionResult::Revert, volatile_data_access_disabled_revert_data(access))
}

/// The refusal follows what the Host loads, whichever way the call reaches it: the beneficiary
/// behind an EIP-7702 delegation, the beneficiary with a delegation of its own, and the
/// beneficiary's own frame destroying itself are refused; a call that loads no volatile data
/// proceeds.
#[test]
fn test_a_refusal_follows_what_the_host_loads() {
    let beneficiary = refused(VolatileDataAccess::BENEFICIARY_BALANCE);

    // A delegator whose delegate is the beneficiary.
    let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
        .account_code(CHILD, call(BytecodeBuilder::default(), CALL, DELEGATOR).stop().build());
    assert_eq!(refused_child(db), beneficiary, "the delegate");

    // The beneficiary, delegating elsewhere.
    let db = with_delegation(MemoryDatabase::default(), BENEFICIARY, OTHER)
        .account_code(CHILD, call(BytecodeBuilder::default(), CALL, BENEFICIARY).stop().build());
    assert_eq!(refused_child(db), beneficiary, "the beneficiary itself");

    // Another account.
    let db = MemoryDatabase::default()
        .account_code(OTHER, BytecodeBuilder::default().stop().build())
        .account_code(CHILD, call(BytecodeBuilder::default(), CALL, OTHER).stop().build());
    assert_eq!(refused_child(db), (InstructionResult::Stop, Bytes::new()), "no volatile load");
}

/// The beneficiary's own frame destroying itself reads and clears the beneficiary's balance:
/// with access off from that frame, the destruction is refused.
#[test]
fn test_the_beneficiary_destroying_itself_is_refused() {
    let code = BytecodeBuilder::default().push_address(OTHER).append(SELFDESTRUCT).build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, BENEFICIARY).stop().build())
        .account_code(BENEFICIARY, code)
        .account_balance(BENEFICIARY, U256::from(7));
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
        .with_inspector(Calls::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success());
    let frame = &evm.inspector().calls[0];
    assert_eq!(frame.target, BENEFICIARY);
    assert_eq!(
        (frame.result, frame.output.clone()),
        refused(VolatileDataAccess::BENEFICIARY_BALANCE)
    );
    assert!(run.outcome.state.get(&OTHER).is_none_or(|account| account.info.balance.is_zero()));
}

/// An opcode that fails before it loads anything keeps its own failure: a `SELFDESTRUCT` in the
/// beneficiary's own frame or a `CALL` to the beneficiary short of operands halts with a stack
/// underflow, not a refusal.
#[test]
fn test_a_stack_underflow_is_not_a_refusal() {
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, BENEFICIARY).stop().build())
        .account_code(BENEFICIARY, BytecodeBuilder::default().append(SELFDESTRUCT).build());
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
        .with_inspector(Calls::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success());
    let frame = &evm.inspector().calls[0];
    assert_eq!((frame.target, frame.result), (BENEFICIARY, InstructionResult::StackUnderflow));

    let partial_call = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .push_number(100_000_u32)
        .append(CALL)
        .build();
    let db = MemoryDatabase::default().account_code(CHILD, partial_call);
    assert_eq!(refused_child(db).0, InstructionResult::StackUnderflow, "CALL");
}

/// A database failure on an account an opcode loads surfaces as an error of the transaction, with
/// access switched off or not; a `CALL` short of operands still fails on its stack first, even
/// when its target's code would fail to load.
#[test]
fn test_a_database_failure_surfaces_and_a_stack_underflow_comes_first() {
    let selfdestruct = BytecodeBuilder::default().push_address(OTHER).append(SELFDESTRUCT).build();
    let calls_other = call(BytecodeBuilder::default(), CALL, OTHER).stop().build();
    for (child, disabled) in [(selfdestruct, false), (calls_other, true)] {
        let inner = MemoryDatabase::default()
            .account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, CHILD).stop().build())
            .account_code(CHILD, child)
            .account_balance(OTHER, U256::from(1));
        let mut db = ErrorInjectingDatabase::new(inner);
        db.fail_on_account = Some(OTHER);
        let mut ctx = context(db);
        if disabled {
            ctx = ctx.with_volatile_access_disabled_from(1);
        }
        let result = MegaEvm::new(ctx).execute_transaction(tx(CALLER, CONTRACT, BELOW));
        assert!(result.is_err(), "disabled {disabled}: {result:?}");
    }

    // The target's account, or its code, fails to load; the partial CALL never gets that far.
    let code_hash = Bytecode::new_eip7702(BENEFICIARY).hash_slow();
    let partial_call = BytecodeBuilder::default()
        .push_address(OTHER)
        .push_number(100_000_u32)
        .append(CALL)
        .build();
    for fails_on_code in [false, true] {
        let inner = MemoryDatabase::default()
            .account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, CHILD).stop().build())
            .account_code(CHILD, partial_call.clone())
            .account_lazy_code(OTHER, code_hash);
        let mut db = ErrorInjectingDatabase::new(inner);
        if fails_on_code {
            db.fail_on_code_by_hash = Some(code_hash);
        } else {
            db.fail_on_account = Some(OTHER);
        }
        let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
            .with_inspector(Calls::default());
        let outcome =
            evm.execute_transaction(tx(CALLER, CONTRACT, BELOW)).expect("no database error");
        assert!(outcome.result.is_success());
        let child = evm.inspector().calls.iter().find(|call| call.target == CHILD).unwrap();
        assert_eq!(child.result, InstructionResult::StackUnderflow, "code {fails_on_code}");
    }
}

/// Calling `MegaAccessControl` costs the caller its call's overhead alone: the answer carries the
/// forwarded gas back untouched.
#[test]
fn test_a_call_to_the_access_control_contract_costs_its_overhead() {
    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let code = BytecodeBuilder::default()
        .mstore(0, selector)
        .append(GAS)
        .append_many([PUSH0, PUSH0])
        .push_number(4_u8)
        .append_many([PUSH0, PUSH0])
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u32)
        .append_many([CALL, POP, GAS, SWAP1, SUB])
        .push_number(0_u8)
        .append(MSTORE)
        .push_number(32_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build();
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, BELOW),
    );
    assert!(run.outcome.result.is_success());
    let consumed = U256::from_be_slice(run.outcome.result.output().unwrap());
    // The pushes (2 + 2 + 3 + 2 + 2 + 3 + 3), the cold call (2,600), POP and GAS.
    assert_eq!(consumed, U256::from(17 + 2_600 + 2 + 2));
}
