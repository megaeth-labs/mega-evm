//! Volatile reads the legacy engine's tests pinned, on Satin's gas detention: what records a
//! read, which read sets the limit, where the Oracle's storage is read and where it is not, who
//! is exempt, and how a refused read keeps the failures an opcode has on its own.
//!
//! The legacy engine's switch was steered through `MegaAccessControl`; here the tests switch it
//! off with `MegaContext::with_volatile_access_disabled_from`, as if the frame at that depth had
//! called `disableVolatileDataAccess()` first.

use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::ORACLE_ACCESS_COMPUTE_GAS,
    system::{
        IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, MEGA_SYSTEM_ADDRESS,
        MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{op_transaction, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    volatile_data_access_disabled_revert_data, EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaEvm,
    MegaLimitExceeded, MegaTransaction, VolatileDataAccess,
};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, Transaction, TxEnv},
    interpreter::InstructionResult,
    primitives::{eip2780::TX_BASE_COST, eip8038::COLD_ACCOUNT_ACCESS},
    state::Bytecode,
};

use crate::{
    common::body_history,
    detention::{
        assert_stopped, burn, call, context, execute, intrinsic, on_beneficiary, op, run_on, spin,
        stop_data, tx, with_delegation, work, Calls, Charges, BELOW, BENEFICIARY, CALLER, CAP,
        CHILD, CONTRACT, DELEGATOR, TIERS,
    },
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

/// A read that stays within the cap costs what it ran: the gas detention withheld is never
/// spent, below and above the execution cap.
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
            // What it ran is well under 100,000; the body's history comes on top.
            let used = run.outcome.result.gas().tx_gas_used();
            assert!(
                used < 100_000 + body_history(0),
                "{opcode:#04x}: {used}, not the gas it was given"
            );
        }
        let code = BytecodeBuilder::default().push_number(299_u16).append(BLOCKHASH).stop().build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success());
        assert!(run.outcome.result.gas().tx_gas_used() < 100_000 + body_history(0));
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
        // The reading frame's `POP`, the middle frame's `POP` and work, then the transaction's own
        // frame's `POP` and loop, each in its own memory.
        let left = Charges::default().then(&[2, 2]).work(10, 0).then(&[2]).spin(0).left(CAP);
        let limit = assert_stopped(&run, intrinsic(gas_limit), left);
        assert!(limit < CAP + 100_000, "the read was the third frame's first opcode: {limit}");
        assert_eq!(run.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);

        let run = execute(
            db(call(BytecodeBuilder::default(), CALL, CHILD).stop().build()),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success());
        assert!(run.outcome.result.gas().tx_gas_used() < 200_000 + body_history(0));
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
        // The read is the child's call to the beneficiary: the beneficiary's revert — two
        // `PUSH0` — the child's `POP` and its own revert, then its caller's `POP` and loop.
        let left = Charges::default().then(&[2, 2, 2, 2, 2, 2]).spin(0).left(CAP);
        let limit = assert_stopped(&run, intrinsic(gas_limit), left);
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
            EvmTxRuntimeLimits::default().with_tx_data_size_limit(data_limit),
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

        // Whoever calls it, the Oracle's `POP` and loop come after its read.
        let left = Charges::default().then(&[2]).spin(0).left(CAP);
        let run = execute(db(), tx(CALLER, ORACLE_CONTRACT_ADDRESS, gas_limit));
        assert_eq!(
            assert_stopped(&run, intrinsic, left),
            CAP + 2 + 2_100,
            "the direct call: PUSH0 and a cold SLOAD"
        );
        assert_eq!(run.accessed, VolatileDataAccess::ORACLE);

        for scheme in [CALL, STATICCALL] {
            let code =
                call(BytecodeBuilder::default(), scheme, ORACLE_CONTRACT_ADDRESS).stop().build();
            let run = execute(db().account_code(CONTRACT, code), tx(CALLER, CONTRACT, gas_limit));
            assert_stopped(&run, intrinsic, left);
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
    let db = || {
        MemoryDatabase::default()
            .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone())
            .sequencer_registry(MEGA_SYSTEM_ADDRESS)
    };

    let system = op_transaction(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        gas_limit: BELOW,
        chain_id: Some(1),
        ..Default::default()
    });
    let run = execute(db(), alloy_op_evm::OpTx(system));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert!(!run.detains);
    assert_eq!(run.accessed, VolatileDataAccess::empty());
    assert_eq!(run.limit, None);
    assert!(run.outcome.gas.regular > CAP);

    let run = execute(db(), tx(CALLER, ORACLE_CONTRACT_ADDRESS, BELOW));
    assert!(run.detains);
    assert!(
        matches!(
            run.outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, .. })
        ),
        "{:?}",
        run.outcome.result
    );
}

/// A user's deposit is detained. [S12.51] [S19.8]
///
/// independent: the cap, the compute at the read and the bill are the spec's own numbers.
/// The cap is 20,000,000.
/// `TIMESTAMP` costs 2, `PUSH0` costs 2 and a cold `SLOAD` costs 2,100, so the compute at the
/// read is that opcode's gas.
/// The EIP-2780 intrinsic of a call to an existing account, with no value and no calldata, is
/// `TX_BASE_COST` (12,000) plus the recipient access (3,000).
/// What the loop leaves is the same hand count `Charges` makes: the `POP` after the read, the
/// push of the counter, one round that expands memory to 1,024 words, then rounds of 3,108 until
/// the next round's 3,072-gas copy no longer fits.
///
/// A deposit pays no history gas, and a user's deposit is not system-originated.
/// The same body, run as a Mega System Transaction, reads the same thing and is not stopped.
#[test]
fn test_a_users_deposit_is_detained() {
    // The spec's numbers, held equal to the constants the engine is built with.
    const BLOCK_ENV_CAP: u64 = 20_000_000;
    const ORACLE_CAP: u64 = 20_000_000;
    const TIMESTAMP_GAS: u64 = 2;
    const PUSH_GAS: u64 = 2;
    const COLD_SLOAD: u64 = 2_100;
    const BASE_INTRINSIC: u64 = 12_000;
    const RECIPIENT_ACCESS: u64 = 3_000;
    assert_eq!(BLOCK_ENV_CAP, CAP);
    assert_eq!(BLOCK_ENV_CAP, ORACLE_CAP);
    assert_eq!(ORACLE_CAP, ORACLE_ACCESS_COMPUTE_GAS);
    assert_eq!(BASE_INTRINSIC, TX_BASE_COST);
    assert_eq!(RECIPIENT_ACCESS, COLD_ACCOUNT_ACCESS);
    assert_eq!(LimitKind::ComputeGas.as_u8(), 2);
    let intrinsic = BASE_INTRINSIC + RECIPIENT_ACCESS;
    assert_eq!(intrinsic, 15_000);

    // A source hash a user can carry: not empty, and not the protocol's own.
    let source = B256::repeat_byte(0x11);
    assert_ne!(source, B256::ZERO);
    assert_ne!(source, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH);
    assert_ne!(CALLER, MEGA_SYSTEM_ADDRESS);
    assert_ne!(CALLER, mega_evm::SYSTEM_ADDRESS);

    // What a 20,000,000 allowance has left when the copy that crosses it is not made.
    let left = allowance_after_the_read(BLOCK_ENV_CAP);
    assert_eq!(left, 1_100);
    assert_eq!(Charges::default().then(&[2]).work(PAST_THE_CAP, 0).left(BLOCK_ENV_CAP), left);

    let timestamp = work(op(BytecodeBuilder::default(), TIMESTAMP), PAST_THE_CAP).stop().build();
    let oracle = work(BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]), PAST_THE_CAP)
        .stop()
        .build();
    assert_deposit_stopped(
        "TIMESTAMP",
        timestamp.clone(),
        TIMESTAMP_GAS,
        BLOCK_ENV_CAP,
        VolatileDataAccess::TIMESTAMP,
        intrinsic,
        left,
        source,
    );
    assert_deposit_stopped(
        "Oracle",
        oracle.clone(),
        PUSH_GAS + COLD_SLOAD,
        ORACLE_CAP,
        VolatileDataAccess::ORACLE,
        intrinsic,
        left,
        source,
    );
    assert_system_transaction_is_not_stopped("TIMESTAMP", timestamp, BLOCK_ENV_CAP);
    assert_system_transaction_is_not_stopped("Oracle", oracle, ORACLE_CAP);
}

/// What `allowance` has left after the `POP` that follows a volatile read and `work` of
/// `PAST_THE_CAP` rounds, at the spec's opcode costs.
///
/// The first round expands memory from nothing to the 1,024 words the copy writes
/// (`3w + w²/512` = 5,120). Each later round costs 3,108. The round that crosses pays
/// `JUMPDEST`, the size push, two `PUSH0` and `MCOPY`'s static gas, and cannot pay the copy.
fn allowance_after_the_read(allowance: u64) -> u64 {
    let mut left = allowance;
    left -= 2; // the POP of the value the read pushed
    left -= 3; // the push of the round counter
    let words = 1_024u64;
    let expansion = 3 * words + words * words / 512;
    assert_eq!(expansion, 5_120);
    let before_copy = 1 + 3 + 2 + 2 + 3; // JUMPDEST, PUSH of 0x8000, two PUSH0, MCOPY
    let copy = 1_024 * 3;
    assert_eq!(copy, 3_072);
    let after_copy = 3 + 3 + 3 + 3 + 3 + 10; // PUSH1, SWAP1, SUB, DUP1, the dest push, JUMPI
    left -= before_copy + copy + expansion + after_copy;
    let round = before_copy + copy + after_copy;
    assert_eq!(round, 3_108);
    let full = left / round;
    left -= full * round;
    assert!(full < u64::from(PAST_THE_CAP) - 1, "the loop does not finish");
    left -= before_copy;
    assert!(copy > left, "the copy is the charge that crosses");
    left
}

/// `code` on the Oracle, as a user's deposit from `CALLER`. Both accounts already exist, so
/// the run adds no state.
fn user_deposit(source: B256) -> MegaTransaction {
    let mut tx = alloy_op_evm::OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        gas_limit: BELOW,
        gas_price: 0,
        chain_id: Some(1),
        ..Default::default()
    }));
    tx.0.deposit.source_hash = source;
    tx.0.deposit.is_system_transaction = false;
    assert_eq!(Transaction::tx_type(&tx.0), DEPOSIT_TRANSACTION_TYPE);
    assert!(!mega_evm::system::is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    tx
}

fn deposit_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_code(ORACLE_CONTRACT_ADDRESS, code)
        .account_balance(CALLER, U256::from(1))
        .account_balance(ORACLE_CONTRACT_ADDRESS, U256::from(1))
        .sequencer_registry(MEGA_SYSTEM_ADDRESS)
}

#[allow(clippy::too_many_arguments)]
fn assert_deposit_stopped(
    name: &str,
    code: Bytes,
    compute_at_read: u64,
    cap: u64,
    access: VolatileDataAccess,
    intrinsic: u64,
    left: u64,
    source: B256,
) {
    let limit = compute_at_read + cap;
    let used = limit - left;
    let run = execute(deposit_db(code), user_deposit(source));
    assert!(run.detains, "{name}: a user's deposit is detained, got {:?}", run.outcome.result);
    assert_eq!(run.accessed, access, "{name}");
    assert_eq!(run.limit, Some(limit), "{name}: the read set compute at the read plus the cap");
    match &run.outcome.result {
        ExecutionResult::Revert { output, .. } => {
            assert_eq!(
                output.as_ref(),
                MegaLimitExceeded { kind: 2, limit }.abi_encode(),
                "{name}"
            );
        }
        other => panic!("{name}: expected the detention stop, got {other:?}"),
    }
    assert_eq!(
        run.outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit,
            used,
            frame_local: false,
        }),
        "{name}"
    );
    assert_eq!(run.outcome.gas.regular, intrinsic + used, "{name}: billed what ran");
    assert_eq!(run.outcome.gas.gas_used, intrinsic + used, "{name}: the receipt bills the same");
    assert_eq!(run.outcome.gas.state, 0, "{name}");
    assert_eq!(run.outcome.gas.history, 0, "{name}: a deposit pays no history");
    assert_eq!(run.outcome.gas.history_bytes, 0, "{name}");
    assert!(run.outcome.result.logs().is_empty(), "{name}");
}

/// The same body as a Mega System Transaction: a legacy call from the system address to the
/// Oracle. It is the protocol's own transaction, so the read does not detain it.
fn assert_system_transaction_is_not_stopped(name: &str, code: Bytes, cap: u64) {
    let system = alloy_op_evm::OpTx(op_transaction(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        gas_limit: BELOW,
        chain_id: Some(1),
        ..Default::default()
    }));
    assert!(mega_evm::system::is_system_originated(&system, MEGA_SYSTEM_ADDRESS), "{name}");
    let run = execute(deposit_db(code), system);
    assert!(run.outcome.result.is_success(), "{name} system: {:?}", run.outcome.result);
    assert!(!run.detains, "{name} system");
    assert_eq!(run.accessed, VolatileDataAccess::empty(), "{name} system");
    assert_eq!(run.limit, None, "{name} system");
    assert!(run.outcome.gas.regular > cap, "{name} system: it ran past the cap");
    assert_eq!(run.outcome.gas.history, 0, "{name} system");
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
        // Detained from its start: `SELFBALANCE`, its `POP` and the loop all come after the read.
        let left = Charges::default().then(&[5, 2]).spin(0).left(CAP);
        assert_eq!(assert_stopped(&run, intrinsic(gas_limit), left), CAP);

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

/* ---------- an applied authority ---------- */

/// A transaction authorizing `authority` to delegate to `CHILD`, from `CALLER` to `CONTRACT`.
fn authorizing(authority: Address, gas_limit: u64) -> mega_evm::MegaTransaction {
    use alloy_op_evm::OpTx;
    use revm::{
        context::transaction::TransactionType,
        context_interface::{
            either::Either,
            transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
        },
    };
    let authorization = Either::Right(RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::ZERO, address: CHILD, nonce: 0 },
        RecoveredAuthority::Valid(authority),
    ));
    OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip7702 as u8,
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit,
        gas_priority_fee: Some(0),
        authorization_list: vec![authorization],
        ..Default::default()
    }))
}

/// Whether `account` carries the designator of a delegation to `CHILD` after the run.
fn delegates_to_child(run: &crate::detention::Run, account: Address) -> bool {
    run.outcome.state.get(&account).is_some_and(|account| {
        account.info.code.as_ref().is_some_and(|code| code == &Bytecode::new_eip7702(CHILD))
    })
}

/// An applied EIP-7702 authority that is the block beneficiary writes the beneficiary's account:
/// the transaction is detained from its first frame, its limit the cap, though neither its sender
/// nor its recipient is the beneficiary. An authority that is not the beneficiary detains nothing.
#[test]
fn test_an_applied_authority_that_is_the_beneficiary_detains() {
    let code = work(BytecodeBuilder::default(), 10).stop().build();
    for gas_limit in TIERS {
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code.clone()),
            authorizing(BENEFICIARY, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert!(delegates_to_child(&run, BENEFICIARY), "the authority applied");
        assert_eq!(run.limit, Some(CAP));
        assert_eq!(run.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);

        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code.clone()),
            authorizing(OTHER, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert!(delegates_to_child(&run, OTHER), "the authority applied");
        assert_eq!(run.limit, None);
        assert_eq!(run.accessed, VolatileDataAccess::empty());
    }
}

/// A transaction whose recipient is an EIP-7702 delegator of the beneficiary runs the
/// beneficiary's code, as a call a contract makes to that delegator does: it is detained from its
/// first frame, its limit the cap. A delegator of another account detains nothing.
#[test]
fn test_a_transaction_to_a_delegator_of_the_beneficiary_detains() {
    let code = work(BytecodeBuilder::default(), 10).stop().build();
    for gas_limit in TIERS {
        let db = with_delegation(
            MemoryDatabase::default().account_code(BENEFICIARY, code.clone()),
            DELEGATOR,
            BENEFICIARY,
        );
        let run = execute(db, tx(CALLER, DELEGATOR, gas_limit));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(run.limit, Some(CAP));
        assert_eq!(run.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);

        let db = with_delegation(
            MemoryDatabase::default().account_code(OTHER, code.clone()),
            DELEGATOR,
            OTHER,
        );
        let run = execute(db, tx(CALLER, DELEGATOR, gas_limit));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(run.limit, None);
        assert_eq!(run.accessed, VolatileDataAccess::empty());
    }
}

/// Compute is what the frames spend, not the transaction's intrinsic gas: under a cap far below
/// what an authorization costs before any frame, an authority that is the beneficiary stops
/// nothing before the first frame, and stays applied. The first frame is held to the cap: a frame
/// within it completes, and one past it stops the transaction, its authorization standing.
#[test]
fn test_a_cap_below_the_intrinsic_gas_stops_nothing_before_the_first_frame() {
    let tiny = 1_000;
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(tiny);
    for gas_limit in TIERS {
        let run = |code: Bytes| {
            let db = MemoryDatabase::default().account_code(CONTRACT, code);
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
            run_on(&mut evm, authorizing(BENEFICIARY, gas_limit))
        };

        // A frame that only stops spends no compute: its bill is the intrinsic gas.
        let stops = run(BytecodeBuilder::default().stop().build());
        assert!(stops.outcome.result.is_success(), "{:?}", stops.outcome.result);
        let intrinsic = stops.outcome.gas.regular;
        assert!(intrinsic > tiny, "the authorization costs more than the cap before any frame");
        assert!(delegates_to_child(&stops, BENEFICIARY));
        assert_eq!(stops.limit, Some(tiny));

        let within = run(burn(BytecodeBuilder::default(), 10).stop().build());
        assert!(within.outcome.result.is_success(), "{:?}", within.outcome.result);

        // The stop takes back what the frame did, not the authorization or its state gas. The
        // frame is detained from its start: the loop's copy is the first charge past the cap.
        let past = run(spin(BytecodeBuilder::default()));
        let used = tiny - Charges::default().spin(0).left(tiny);
        assert_eq!(used, 1 + 3 + 2 + 2 + 3, "the loop up to the copy");
        assert_eq!(
            past.outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                limit: tiny,
                used,
                frame_local: false
            })
        );
        assert_eq!(past.outcome.result.output(), Some(&stop_data(tiny)));
        assert_eq!(past.outcome.gas.regular, intrinsic + used);
        assert_eq!(past.outcome.gas.state, stops.outcome.gas.state);
        assert!(delegates_to_child(&past, BENEFICIARY), "the authorization stands");
    }
}
