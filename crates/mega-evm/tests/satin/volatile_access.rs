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
    constants::{ORACLE_ACCESS_COMPUTE_GAS, TX_GAS_LIMIT_CAP},
    system::{
        IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, MEGA_SYSTEM_ADDRESS,
        MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{
        op_transaction, record_start_ledger_settlements, take_start_ledger_settlements,
        BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase, RecordingDatabase,
        SharedWitnessRecord,
    },
    volatile_data_access_disabled_revert_data, EvmTxRuntimeLimits, LimitCheck, LimitKind,
    MegaContext, MegaEvm, MegaLimitExceeded, MegaTransaction, VolatileDataAccess,
};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    bytecode::opcode::*,
    context::{
        result::ExecutionResult,
        transaction::{AccessList, AccessListItem, TransactionType},
        Transaction, TxEnv,
    },
    context_interface::cfg::GasId,
    interpreter::{
        gas::MemoryGas,
        interpreter::EthInterpreter,
        interpreter_types::{Jumps, MemoryTr},
        CallInputs, CallOutcome, Gas, InstructionResult, Interpreter,
    },
    primitives::{eip2780::TX_BASE_COST, eip8038::COLD_ACCOUNT_ACCESS},
    state::Bytecode,
    Inspector,
};

use crate::{
    common::body_history,
    detention::{
        assert_caught, assert_stopped, burn, call, cold_access, context, execute, intrinsic,
        memory_cost, on_beneficiary, op, run_on, spin, stop_data, tx, value_transfer,
        with_delegation, work, Calls, Charges, Run, ABOVE, BELOW, BENEFICIARY, CALLER, CAP, CHILD,
        CONTRACT, DELEGATOR, TIERS,
    },
    salt::entry,
};

/// A second contract, never the beneficiary.
const OTHER: Address = address!("0000000000000000000000000000000000d00004");

/// Rounds of [`work`] that spend more compute than the cap: 21,756,000.
const PAST_THE_CAP: u32 = 7_000;

/* ---------- what records a read ---------- */

/// The transaction's and the chain's own fields, and a contract's own balance, record nothing; the
/// opcodes that read the block environment record their kinds in the matrix of reads.
#[test]
fn test_the_transactions_own_fields_record_nothing() {
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
/// calls it directly or through a frame, which catches its stop and ends on a `POP`, and whether
/// the call is a `CALL` or a `STATICCALL`.
#[test]
fn test_the_oracle_reading_its_own_storage_is_held_to_the_cap() {
    assert_eq!(ORACLE_ACCESS_COMPUTE_GAS, CAP);
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let db = || {
            MemoryDatabase::default()
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle_reads_then_spins())
        };

        // Whoever calls it, the `SLOAD`'s cold access, which the read is made before, and the
        // Oracle's `POP` and loop come after its read.
        let left = Charges::default().then(&[2_100, 2]).spin(0).left(CAP);
        let run = execute(db(), tx(CALLER, ORACLE_CONTRACT_ADDRESS, gas_limit));
        assert_eq!(
            assert_stopped(&run, intrinsic, left),
            CAP + 2,
            "the direct call: the PUSH0 before the read"
        );
        assert_eq!(run.accessed, VolatileDataAccess::ORACLE);

        for scheme in [CALL, STATICCALL] {
            let code =
                call(BytecodeBuilder::default(), scheme, ORACLE_CONTRACT_ADDRESS).stop().build();
            let run = execute(db().account_code(CONTRACT, code), tx(CALLER, CONTRACT, gas_limit));
            assert_caught(&run, intrinsic, left, &Charges::default().then(&[2]));
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
/// `TIMESTAMP` costs 2, so the compute at its read is that opcode's gas. An Oracle read is made
/// before its `SLOAD` charges anything, so the compute at it is the `PUSH0`'s 2, and the `SLOAD`'s
/// cold access, 2,100, is the first charge the allowance pays after it.
/// The EIP-2780 intrinsic of a call to an existing account, with no value and no calldata, is
/// `TX_BASE_COST` (12,000) plus the recipient access (3,000).
/// What the loop leaves is the same hand count `Charges` makes: the `POP` after the read, the
/// push of the counter, one round that expands memory to 1,024 words, then rounds of 3,108 until
/// the next round's 3,072-gas copy no longer fits.
///
/// A deposit pays its body's history gas, and a user's deposit is not system-originated.
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

    // What a 20,000,000 allowance has left when the copy that crosses it is not made: after the
    // block environment's read, and after the Oracle's, whose `SLOAD` pays its cold access first.
    let left = allowance_after_the_read(BLOCK_ENV_CAP);
    assert_eq!(left, 1_100);
    assert_eq!(Charges::default().then(&[2]).work(PAST_THE_CAP, 0).left(BLOCK_ENV_CAP), left);
    let oracle_left = allowance_after_the_read(ORACLE_CAP - COLD_SLOAD);
    assert_eq!(
        Charges::default().then(&[COLD_SLOAD, 2]).work(PAST_THE_CAP, 0).left(ORACLE_CAP),
        oracle_left
    );

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
        PUSH_GAS,
        ORACLE_CAP,
        VolatileDataAccess::ORACLE,
        intrinsic,
        oracle_left,
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
    assert_eq!(
        run.outcome.gas.gas_used,
        intrinsic + used + crate::common::body_history(0),
        "{name}: the receipt includes history"
    );
    assert_eq!(run.outcome.gas.state, 0, "{name}");
    assert_eq!(
        run.outcome.gas.history,
        crate::common::body_history(0),
        "{name}: the body pays history"
    );
    assert_eq!(run.outcome.gas.history_bytes, mega_evm::TX_BODY_SIZE, "{name}");
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
    assert_eq!(run.outcome.gas.history, crate::common::body_history(0), "{name} system");
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
/// proceeds, and so does an `EXTCODECOPY` of another account with every operand on the stack.
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

    // A copy of another account's code, its four operands on the stack.
    let copy = BytecodeBuilder::default()
        .push_number(1_u8)
        .append_many([PUSH0, PUSH0])
        .push_address(OTHER)
        .append(EXTCODECOPY)
        .stop()
        .build();
    let db = MemoryDatabase::default()
        .account_code(OTHER, BytecodeBuilder::default().stop().build())
        .account_code(CHILD, copy);
    assert_eq!(refused_child(db), (InstructionResult::Stop, Bytes::new()), "EXTCODECOPY");
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
/// beneficiary's own frame, a `CALL` to the beneficiary or an `EXTCODECOPY` of it short of
/// operands halts with a stack underflow, not a refusal.
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

    let partial_copy =
        BytecodeBuilder::default().push_address(BENEFICIARY).append(EXTCODECOPY).build();
    let db = MemoryDatabase::default().account_code(CHILD, partial_copy);
    assert_eq!(refused_child(db).0, InstructionResult::StackUnderflow, "EXTCODECOPY");
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

/* ---------- a call's delegate, refused where the call loads it ---------- */

/// An account no transaction here funds, and that nothing loads unless a call reads it.
const UNTOUCHED: Address = address!("0000000000000000000000000000000000d00005");

/// A call of `to` through `scheme` forwarding no gas, carrying `value` when the scheme takes one,
/// with an input of `in_len` bytes at offset zero and no output, then a `STOP`; and what the
/// pushes before the call cost.
fn call_of(scheme: u8, to: Address, value: u8, in_len: u32) -> (Bytes, u64) {
    let mut code = BytecodeBuilder::default().append_many([PUSH0, PUSH0]);
    let mut pushes = 2 + 2;
    if in_len == 0 {
        code = code.append(PUSH0);
        pushes += 2;
    } else {
        code = code.push_number(in_len);
        pushes += 3;
    }
    code = code.append(PUSH0);
    pushes += 2;
    if matches!(scheme, CALL | CALLCODE) {
        if value == 0 {
            code = code.append(PUSH0);
            pushes += 2;
        } else {
            code = code.push_number(value);
            pushes += 3;
        }
    }
    let code = code.push_address(to).append(PUSH0).append(scheme).stop();
    (code.build(), pushes + 3 + 2)
}

/// A call of `CHILD` forwarding `gas`, or everything when `gas` is `None`, its status dropped.
fn calls_child(code: BytecodeBuilder, gas: Option<u64>) -> BytecodeBuilder {
    let code = code.append_many([PUSH0; 5]).push_address(CHILD);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    code.append_many([CALL, POP])
}

/// Appends a `BALANCE` of [`DELEGATOR`] and returns, as a word, what it cost with its push, its
/// `POP` and the `GAS` that reads what is left: [`cold_access`] and seven when the account is cold.
fn then_balance_of_the_delegator(code: BytecodeBuilder) -> Bytes {
    code.append(GAS)
        .push_address(DELEGATOR)
        .append_many([BALANCE, POP, GAS, SWAP1, SUB, PUSH0, MSTORE])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN])
        .build()
}

/// What a [`then_balance_of_the_delegator`] returned.
fn balance_cost(run: &Run) -> u64 {
    let output = run.outcome.result.output().expect("the caller returns the cost");
    U256::from_be_slice(output).to::<u64>()
}

/// What a `BALANCE` of a cold account costs the caller in [`then_balance_of_the_delegator`].
fn cold_balance() -> u64 {
    3 + cold_access() + 2 + 2
}

/// Runs `tx` with volatile-data access off from depth 1 down, and returns the run, how `CHILD`'s
/// frame ended and the gas it spent.
fn run_child(db: MemoryDatabase, tx: MegaTransaction) -> (Run, InstructionResult, Bytes, u64) {
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
        .with_inspector(Calls::default());
    let run = run_on(&mut evm, tx);
    let child =
        evm.inspector().calls.iter().find(|call| call.target == CHILD).expect("the child ran");
    let (result, output, spent) = (child.result, child.output.clone(), child.spent);
    (run, result, output, spent)
}

/// A call of the beneficiary itself is refused before it charges anything, whatever its frame
/// holds: a value `CALL` of it with an input of 1 MiB, from a frame holding 50 at the call — less
/// than its static gas — reverts with `VolatileDataAccessDisabled(10)` spending nothing of the
/// call, and marks nothing for gas detention.
#[test]
fn test_a_direct_call_of_the_beneficiary_is_refused_below_its_static_gas() {
    let (child, pushes) = call_of(CALL, BENEFICIARY, 1, 0x10_0000);
    for gas_limit in TIERS {
        let db = MemoryDatabase::default()
            .account_code(
                CONTRACT,
                calls_child(BytecodeBuilder::default(), Some(pushes + 50)).stop().build(),
            )
            .account_code(CHILD, child.clone());
        let (run, result, output, spent) = run_child(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!((result, output), refused(VolatileDataAccess::BENEFICIARY_BALANCE));
        assert_eq!(spent, pushes, "the call spends nothing");
        assert_eq!(run.accessed, VolatileDataAccess::empty());
        assert_eq!(run.limit, None);
    }
}

/// With access off, a call of a cold EIP-7702 delegator of the beneficiary runs its own steps as
/// it would with access on, and is refused only when it reaches the load of its delegate: its
/// static gas, then the load of its target, which a frame short of the cold surcharge does not
/// make, then the check that it holds its target's access with the delegate's warm access on top.
/// A frame holding 2,999 at the `STATICCALL` runs out of gas without reading the delegator; one
/// holding 3,000 or 3,050 reads it and runs out of gas at that check; one holding 3,100 reaches
/// the delegate and is refused, paying the delegator's cold access, 3,000, and nothing else of the
/// call, so 100 goes back to its caller. Either way the frame's end leaves the delegator cold for
/// its caller, whose `BALANCE` of it pays the cold access, and nothing reads the beneficiary.
#[test]
fn test_a_cold_delegator_of_the_beneficiary_is_refused_for_its_cold_access() {
    let warm = entry(GasId::warm_storage_read_cost());
    let surcharge = entry(GasId::cold_account_additional_cost());
    let (child, pushes) = call_of(STATICCALL, DELEGATOR, 0, 0);
    // (what the child holds at the call, whether it is refused, whether it reads the delegator)
    for (left, is_refused, reads) in [
        (warm + surcharge - 1, false, false),
        (warm + surcharge, false, true),
        (warm + surcharge + warm / 2, false, true),
        (warm + surcharge + warm, true, true),
    ] {
        // The caller stops after the call, so the delegator is in the transaction's state only
        // if the call read it; or it reads the delegator's balance, to find it warm or cold.
        for balance in [false, true] {
            let parent = calls_child(BytecodeBuilder::default(), Some(pushes + left));
            let parent =
                if balance { then_balance_of_the_delegator(parent) } else { parent.stop().build() };
            let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
                .account_code(CONTRACT, parent)
                .account_code(CHILD, child.clone());
            let (run, result, output, spent) = run_child(db, tx(CALLER, CONTRACT, BELOW));
            let case = format!("{left}, balance {balance}");
            assert!(run.outcome.result.is_success(), "{case}: {:?}", run.outcome.result);
            if is_refused {
                let refusal = refused(VolatileDataAccess::BENEFICIARY_BALANCE);
                assert_eq!((result, output), refusal, "{case}");
                assert_eq!(spent, pushes + cold_access(), "{case}: the refusal pays the access");
            } else {
                assert_eq!(result, InstructionResult::OutOfGas, "{case}");
                assert_eq!(spent, pushes + left, "{case}: the halt burns what the frame held");
            }
            if balance {
                assert_eq!(balance_cost(&run), cold_balance(), "{case}: the caller finds it cold");
            } else {
                assert_eq!(run.outcome.state.contains_key(&DELEGATOR), reads, "{case}: read");
            }
            assert_eq!(run.accessed, VolatileDataAccess::empty(), "{case}");
            assert_eq!(run.limit, None, "{case}");
        }
    }
}

/// A transaction from `CALLER` to `to` whose access list warms `listed`.
fn listing(to: Address, listed: Address, gas_limit: u64) -> MegaTransaction {
    alloy_op_evm::OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip2930 as u8,
        caller: CALLER,
        kind: TxKind::Call(to),
        gas_limit,
        access_list: AccessList(vec![AccessListItem { address: listed, storage_keys: vec![] }]),
        ..Default::default()
    }))
}

/// A delegator of the beneficiary the transaction's access list warms is loaded with no cold
/// surcharge, so the call reaches its delegate with the static gas and the delegate's warm access:
/// a frame holding 199 at the `STATICCALL` runs out of gas at the check that it holds them, and
/// one holding 200 is refused, paying the delegator's warm access, 100, and nothing else of the
/// call, so 100 goes back to its caller.
#[test]
fn test_a_warm_delegator_of_the_beneficiary_is_refused_for_its_warm_access() {
    let warm = entry(GasId::warm_storage_read_cost());
    let (child, pushes) = call_of(STATICCALL, DELEGATOR, 0, 0);
    for (left, is_refused) in [(2 * warm - 1, false), (2 * warm, true)] {
        let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
            .account_code(
                CONTRACT,
                calls_child(BytecodeBuilder::default(), Some(pushes + left)).stop().build(),
            )
            .account_code(CHILD, child.clone());
        let (run, result, output, spent) = run_child(db, listing(CONTRACT, DELEGATOR, BELOW));
        assert!(run.outcome.result.is_success(), "{left}: {:?}", run.outcome.result);
        if is_refused {
            assert_eq!(
                (result, output),
                refused(VolatileDataAccess::BENEFICIARY_BALANCE),
                "{left}"
            );
            assert_eq!(spent, pushes + warm, "{left}: the warm access");
        } else {
            assert_eq!(result, InstructionResult::OutOfGas, "{left}");
            assert_eq!(spent, pushes + left, "{left}");
        }
    }
}

/// A call's memory and its value transfer come before its target's load: a `CALLCODE` of a cold
/// delegator of the beneficiary with an input of 1 MiB, from a frame holding 5,000, runs out of
/// gas on its memory without reading the delegator. A value `CALL` of it from a frame that cannot
/// fund the value pays its value transfer, then loads its target: holding 14,400 — the static gas,
/// the transfer, the cold access and the delegate's warm access — or 15,000, it is refused before
/// revm checks the caller's balance, paying the delegator's cold access and not the transfer;
/// holding one less than 14,400, it reads the delegator and runs out of gas at the check that it
/// holds the delegate's access.
#[test]
fn test_a_calls_memory_and_value_transfer_come_before_its_target_is_read() {
    let (callcode, pushes) = call_of(CALLCODE, DELEGATOR, 0, 0x10_0000);
    let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
        .account_code(
            CONTRACT,
            calls_child(BytecodeBuilder::default(), Some(pushes + 5_000)).stop().build(),
        )
        .account_code(CHILD, callcode);
    let (run, result, _, spent) = run_child(db, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(result, InstructionResult::MemoryOOG, "the memory");
    assert_eq!(spent, pushes + entry(GasId::warm_storage_read_cost()), "the static gas, paid");
    assert!(!run.outcome.state.contains_key(&DELEGATOR), "the target is not read");

    let warm = entry(GasId::warm_storage_read_cost());
    let reach = warm + value_transfer() + entry(GasId::cold_account_additional_cost()) + warm;
    let (value_call, pushes) = call_of(CALL, DELEGATOR, 1, 0);
    for (left, is_refused) in [(15_000, true), (reach, true), (reach - 1, false)] {
        let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
            .account_code(
                CONTRACT,
                calls_child(BytecodeBuilder::default(), Some(pushes + left)).stop().build(),
            )
            .account_code(CHILD, value_call.clone());
        let (run, result, output, spent) = run_child(db, tx(CALLER, CONTRACT, BELOW));
        assert!(run.outcome.result.is_success(), "{left}: {:?}", run.outcome.result);
        if is_refused {
            assert_eq!(
                (result, output),
                refused(VolatileDataAccess::BENEFICIARY_BALANCE),
                "{left}"
            );
            assert_eq!(spent, pushes + cold_access(), "{left}: the access, not the transfer");
        } else {
            assert_eq!((result, spent), (InstructionResult::OutOfGas, pushes + left), "{left}");
        }
        assert!(run.outcome.state.contains_key(&DELEGATOR), "{left}: the target is read");
    }
}

/// A call whose memory crosses gas detention is stopped before it loads its target: with access
/// off below a frame that read the block's timestamp, a `STATICCALL` of an account nothing loaded,
/// with an input of 0x320000 bytes whose memory costs more than the frame may spend and less than
/// it holds, pays its static gas and is stopped at its memory, its frame reverting with the
/// compute stop on everything but its pushes and that static gas, and the account is not read.
#[test]
fn test_a_detained_memory_crossing_reads_no_call_target() {
    let (child, pushes) = call_of(STATICCALL, UNTOUCHED, 0, 0x32_0000);
    for gas_limit in TIERS {
        let parent = calls_child(op(BytecodeBuilder::default(), TIMESTAMP), None).stop().build();
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent)
            .account_code(CHILD, child.clone())
            .account_balance(UNTOUCHED, U256::from(1));
        let (run, result, output, spent) = run_child(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let limit = run.limit.expect("the timestamp detains");
        assert_eq!((result, output), (InstructionResult::Revert, stop_data(limit)));
        assert_eq!(
            spent,
            pushes + entry(GasId::warm_storage_read_cost()),
            "the memory is not paid"
        );
        assert!(!run.outcome.state.contains_key(&UNTOUCHED), "the target is not read");
    }
}

/// Records, for the first call opcode `CHILD`'s frame runs, the frame's gas and memory size when
/// it reaches the opcode and once the opcode returned, and how the frame ended and the gas it
/// handed back.
#[derive(Clone, Default)]
struct AtTheCall {
    pending: bool,
    before: Option<(Gas, usize)>,
    after: Option<(Gas, usize)>,
    child: Option<(InstructionResult, Bytes)>,
    returned: Option<u64>,
}

impl<DB: revm::Database> Inspector<MegaContext<DB>, EthInterpreter> for AtTheCall {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        let opcode = interp.bytecode.opcode();
        if interp.input.target_address == CHILD &&
            self.before.is_none() &&
            matches!(opcode, CALL | CALLCODE | DELEGATECALL | STATICCALL)
        {
            self.pending = true;
            self.before = Some((interp.gas, interp.memory.size()));
        }
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if core::mem::take(&mut self.pending) {
            self.after = Some((interp.gas, interp.memory.size()));
        }
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == CHILD {
            self.child = Some((outcome.result.result, outcome.result.output.clone()));
            self.returned = Some(outcome.result.gas.remaining());
        }
    }
}

/// A call refused at its delegate takes back everything it did before the load but its target's
/// access and the memory it grew, so its frame reverts on the gas it held when it reached the call
/// less those. A value `CALL` of a cold delegator of the beneficiary, with an input of 64 KiB, from
/// a frame that cannot fund the value, has paid its static gas, its memory and its value transfer
/// and loaded its target when the load of its delegate is refused: the frame's gas once the opcode
/// returned is its gas before it — the spendable and withheld parts and the reservoir — with the
/// cold access, 3,000, and the 2,048 words of memory charged to its regular gas, and the memory's
/// record at those words; the value transfer is not charged. The frame-start hook has not run, so
/// the transaction keeps no state or history gas but its body's and opens no start beyond its two
/// frames'. The revert leaves the delegator cold for the caller, whose `BALANCE` of it pays the
/// cold access, and the refusal marks nothing for gas detention: in a transaction a read of the
/// timestamp detained first, the refusal leaves its limit and its reads as they were. The same
/// holds above the execution cap, where the frame holds a reservoir.
#[test]
fn test_a_refusal_at_the_delegate_takes_back_everything_but_the_targets_access_and_memory() {
    let (child, _) = call_of(CALL, DELEGATOR, 1, 0x1_0000);
    for gas_limit in TIERS {
        for detained in [false, true] {
            let start = || {
                if detained {
                    op(BytecodeBuilder::default(), TIMESTAMP)
                } else {
                    BytecodeBuilder::default()
                }
            };
            let parent = then_balance_of_the_delegator(calls_child(start(), None));
            let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
                .account_code(CONTRACT, parent)
                .account_code(CHILD, child.clone());
            let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
                .with_inspector(AtTheCall::default());
            record_start_ledger_settlements(evm.ctx_mut());
            let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
            let case = format!("{gas_limit}, detained {detained}");
            assert!(run.outcome.result.is_success(), "{case}: {:?}", run.outcome.result);
            let inspector = evm.inspector();
            assert_eq!(
                inspector.child.clone(),
                Some(refused(VolatileDataAccess::BENEFICIARY_BALANCE)),
                "{case}"
            );
            let (before, after) = (inspector.before.unwrap(), inspector.after.unwrap());
            let words = 0x1_0000 / 32;
            assert_eq!(before.0.memory().words_num, 0, "{case}");
            let mut expected = before.0;
            assert!(expected.record_regular_cost(cold_access() + memory_cost(words)), "{case}");
            *expected.memory_mut() =
                MemoryGas { words_num: words as usize, expansion_cost: memory_cost(words) };
            assert_eq!(after.0, expected, "{case}: the gas at the call, less access and memory");
            if gas_limit == ABOVE {
                assert!(before.0.reservoir() > 0, "{case}: the frame holds a reservoir");
            }
            if detained {
                assert!(before.0.tracker().withheld() > 0, "{case}: part of its gas is withheld");
            }
            assert_eq!(take_start_ledger_settlements(evm.ctx_mut()).len(), 2, "{case}: two starts");
            assert_eq!(run.outcome.gas.state, 0, "{case}");
            assert_eq!(run.outcome.gas.history, body_history(0), "{case}");
            assert_eq!(balance_cost(&run), cold_balance(), "{case}: the target is left cold");

            // The same transaction whose child stops before its call reads what this one read.
            let twin = then_balance_of_the_delegator(calls_child(start(), None));
            let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
                .account_code(CONTRACT, twin)
                .account_code(CHILD, BytecodeBuilder::default().stop().build());
            let twin = execute(db, tx(CALLER, CONTRACT, gas_limit));
            assert_eq!((run.accessed, run.limit), (twin.accessed, twin.limit), "{case}");
        }
    }
}

/// A `STATICCALL` of `to` forwarding nothing, with an input of `input.1` bytes at `input.0` and an
/// output of `output.1` bytes at `output.0`, after `prefix`, then a `STOP`.
fn call_with_ranges(
    prefix: BytecodeBuilder,
    to: Address,
    input: (u32, u32),
    output: (u32, u32),
) -> Bytes {
    prefix
        .push_number(output.1)
        .push_number(output.0)
        .push_number(input.1)
        .push_number(input.0)
        .push_address(to)
        .append_many([PUSH0, STATICCALL])
        .stop()
        .build()
}

/// The words a frame's memory holds once a range of `len` bytes at `offset` is in it, starting
/// from `words`: a range of no bytes grows nothing.
fn words_with(words: u64, (offset, len): (u32, u32)) -> u64 {
    if len == 0 {
        return words;
    }
    words.max((u64::from(offset) + u64::from(len)).div_ceil(32))
}

/// A call refused at its delegate keeps the memory it grew charged, and grown, as the same call
/// pays it with access on: the opcode's own expansion, from the words the frame held before it to
/// the words its input and output ranges reach, and nothing for a range inside memory the frame
/// already grew. A `STATICCALL` of a delegator of the beneficiary, cold or warmed by the access
/// list, with an input range, an output range, both, no range, or ranges after an `MSTORE` grew
/// memory first, inside it or past it, hands back the gas the frame held at the call less the warm
/// access, that expansion and, for a cold delegator, the cold surcharge; the frame's memory record
/// is the grown one. With access on, the same call's memory record grows by the same expansion.
#[test]
fn test_a_refusal_at_the_delegate_keeps_the_memory_its_call_grew() {
    let warm = entry(GasId::warm_storage_read_cost());
    let surcharge = entry(GasId::cold_account_additional_cost());
    // (what the child does before the call, the words that leaves, the input, the output)
    let grown =
        || BytecodeBuilder::default().append_many([PUSH0]).push_number(0x1_0000_u32).append(MSTORE);
    let grown_words = (0x1_0000 + 32) / 32;
    let cases = [
        (BytecodeBuilder::default, 0, (0, 0x1_0000), (0, 0)),
        (BytecodeBuilder::default, 0, (0, 0), (0x100, 0x2_0000)),
        (BytecodeBuilder::default, 0, (0, 0x8000), (0x1_0000, 0x8000)),
        (BytecodeBuilder::default, 0, (0, 0), (0, 0)),
    ];
    let cases = cases
        .into_iter()
        .map(|(prefix, words, input, output)| (prefix(), words, input, output))
        .chain([
            (grown(), grown_words, (0, 0x8000), (0x20, 0x100)),
            (grown(), grown_words, (0, 0x8000), (0x2_0000, 0x100)),
        ]);
    for (prefix, words, input, output) in cases {
        let child = call_with_ranges(prefix, DELEGATOR, input, output);
        let after_call = words_with(words_with(words, input), output);
        let expansion = memory_cost(after_call) - memory_cost(words);
        for listed in [false, true] {
            let case = format!("input {input:?}, output {output:?}, listed {listed}");
            let at_the_call = |disabled: bool| {
                let parent = calls_child(BytecodeBuilder::default(), None).stop().build();
                let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
                    .account_code(CONTRACT, parent)
                    .account_code(CHILD, child.clone());
                let mut ctx = context(db);
                if disabled {
                    ctx = ctx.with_volatile_access_disabled_from(1);
                }
                let mut evm = MegaEvm::new(ctx).with_inspector(AtTheCall::default());
                let tx = if listed {
                    listing(CONTRACT, DELEGATOR, BELOW)
                } else {
                    tx(CALLER, CONTRACT, BELOW)
                };
                let run = run_on(&mut evm, tx);
                assert!(run.outcome.result.is_success(), "{case}: {:?}", run.outcome.result);
                evm.inspector().clone()
            };

            let off = at_the_call(true);
            assert_eq!(off.child, Some(refused(VolatileDataAccess::BENEFICIARY_BALANCE)), "{case}");
            let (before, after) = (off.before.unwrap().0, off.after.unwrap().0);
            assert_eq!(before.memory().words_num as u64, words, "{case}");
            let access = if listed { warm } else { warm + surcharge };
            assert_eq!(before.remaining() - after.remaining(), access + expansion, "{case}");
            assert_eq!(
                *after.memory(),
                MemoryGas {
                    words_num: after_call as usize,
                    expansion_cost: memory_cost(after_call)
                },
                "{case}: the memory's record is the grown one"
            );

            let on = at_the_call(false);
            let (before, after) = (on.before.unwrap().0, on.after.unwrap().0);
            assert_eq!(
                after.memory().expansion_cost - before.memory().expansion_cost,
                expansion,
                "{case}: with access on, the same expansion"
            );
        }
    }
}

/// A refusal at a delegate takes back only the warming its own frame made: its revert leaves a
/// delegator the access list warmed, or one its caller loaded before the call, warm for the
/// caller, whose `BALANCE` of it then pays the warm access, and the refused call pays the warm
/// access for it, 100.
#[test]
fn test_a_refusal_at_the_delegate_leaves_earlier_warmth_to_the_caller() {
    let warm = entry(GasId::warm_storage_read_cost());
    let (child, pushes) = call_of(STATICCALL, DELEGATOR, 0, 0);
    // The caller's `BALANCE` after the refusal, with its push, `POP` and the `GAS` that reads it.
    let warm_balance = 3 + warm + 2 + 2;
    for listed in [false, true] {
        let start = if listed {
            BytecodeBuilder::default()
        } else {
            BytecodeBuilder::default().push_address(DELEGATOR).append_many([BALANCE, POP])
        };
        let parent = then_balance_of_the_delegator(calls_child(start, Some(pushes + 2 * warm)));
        let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
            .account_code(CONTRACT, parent)
            .account_code(CHILD, child.clone());
        let tx =
            if listed { listing(CONTRACT, DELEGATOR, BELOW) } else { tx(CALLER, CONTRACT, BELOW) };
        let (run, result, output, spent) = run_child(db, tx);
        assert!(run.outcome.result.is_success(), "listed {listed}: {:?}", run.outcome.result);
        assert_eq!((result, output), refused(VolatileDataAccess::BENEFICIARY_BALANCE));
        assert_eq!(spent, pushes + warm, "listed {listed}: the warm access");
        assert_eq!(balance_cost(&run), warm_balance, "listed {listed}: still warm for the caller");
    }
}

/// A contract that switches its access off, then calls `helper` forwarding `forward` for as long
/// as it holds `forward` and a fiftieth more, at most `most` times, counting the calls, and
/// returns the count.
fn calls_the_helper_while_it_can(helper: Address, forward: u64, most: u32) -> Bytes {
    let code = disables_volatile_access(BytecodeBuilder::default()).append(PUSH0);
    let top = code.len() as u32;
    // The jumps to the end are patched in once the end is known: a four-byte push holds each.
    let mut to_the_end = Vec::new();
    let code =
        code.append(JUMPDEST).push_number(forward + forward / 49 + 1_000).append_many([GAS, LT]);
    to_the_end.push(code.len());
    let code =
        code.push_number(0_u32).append(JUMPI).push_number(most).append_many([DUP2, LT, ISZERO]);
    to_the_end.push(code.len());
    let code = code
        .push_number(0_u32)
        .append(JUMPI)
        .append_many([PUSH0; 5])
        .push_address(helper)
        .push_number(forward)
        .append_many([CALL, POP])
        .push_number(1_u8)
        .append(ADD)
        .push_number(top)
        .append(JUMP);
    let end = code.len() as u32;
    let code = code
        .append(JUMPDEST)
        .append_many([PUSH0, MSTORE])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN]);
    let mut bytes = code.build().to_vec();
    for at in to_the_end {
        assert_eq!(bytes[at], PUSH4, "the jump's push");
        bytes[at + 1..at + 5].copy_from_slice(&end.to_be_bytes());
    }
    bytes.into()
}

/// With access off, a loop cannot have the node grow memory it does not pay for. A transaction at
/// the execution cap calls a helper as long as it can, at most 32 times, each call forwarding the
/// helper what its `STATICCALL` of a warm delegator of the beneficiary, with an output range of
/// 3 MiB, needs to grow that memory and reach the delegate. Each refusal keeps the expansion
/// charged, so the transaction makes about as many calls as the cap holds expansions, ten, and
/// grows ten times 3 MiB; a refusal that handed the expansion back would let it grow 3 MiB for
/// each 270 gas or so, hundreds of thousands of times, which the bound of 32 calls cuts short.
#[test]
fn test_an_access_off_memory_loop_pays_for_the_memory_it_grows() {
    const HELPER: Address = CHILD;
    const OUTPUT: u32 = 3 << 20;
    let warm = entry(GasId::warm_storage_read_cost());
    let words = u64::from(OUTPUT) / 32;
    let helper = call_with_ranges(BytecodeBuilder::default(), DELEGATOR, (0, 0), (0, OUTPUT));
    // Four pushes of a number, the address's and a `PUSH0`, then the static gas, the memory and
    // the delegate's warm access the guard asks for.
    let pushes = 3 + 3 + 3 + 3 + 3 + 2;
    let forward = pushes + warm + memory_cost(words) + warm;
    let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
        .account_code(CONTRACT, calls_the_helper_while_it_can(HELPER, forward, 32))
        .account_code(HELPER, helper);
    let mut evm = MegaEvm::new(context(db)).with_inspector(Calls::default());
    let run = run_on(&mut evm, listing(CONTRACT, DELEGATOR, TX_GAS_LIMIT_CAP));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    let calls = U256::from_be_slice(run.outcome.result.output().unwrap()).to::<u64>();
    let helpers: Vec<_> =
        evm.inspector().calls.iter().filter(|call| call.target == HELPER).collect();
    assert_eq!(helpers.len() as u64, calls);
    for call in &helpers {
        assert_eq!(
            (call.result, call.output.clone()),
            refused(VolatileDataAccess::BENEFICIARY_BALANCE)
        );
        assert_eq!(call.spent, pushes + warm + memory_cost(words), "the expansion is kept");
    }
    assert!(calls >= 1, "the loop calls the helper");
    assert!(calls <= TX_GAS_LIMIT_CAP / memory_cost(words), "no more calls than expansions fit");
}

/// The words a pre-built call stack's levels sit at, its delegators and their delegates.
fn stack_address(kind: u64, i: u64) -> Address {
    Address::from_word(B256::from(U256::from((kind << 32) + i)))
}

/// Runs a call stack of `levels` frames below a transaction's own frame that read the block's
/// timestamp, with access off from the first level down. Each level but the deepest pushes the
/// operands of a `STATICCALL` of its own fresh delegator, calls the next level with all its gas,
/// then makes that `STATICCALL`, either dropping the next level's status first or, with
/// `status_as_gas`, taking it, a zero once the level was stopped, as the call's gas; the deepest
/// makes its `STATICCALL` at once, the cap set so it may spend `spendable` there. Each delegator
/// delegates to `delegate(i)`. Returns how many delegators and how many delegates the transaction
/// read.
fn run_pre_built_stack(
    levels: u64,
    spendable: u64,
    status_as_gas: bool,
    delegate: impl Fn(u64) -> Address,
) -> (usize, usize) {
    let level = |i: u64| if i == levels { CHILD } else { stack_address(1, i) };
    let delegator = |i: u64| stack_address(2, i);
    let staticcall = |code: BytecodeBuilder, i: u64| {
        let code = code.append_many([PUSH0; 4]).push_address(delegator(i));
        if status_as_gas && i < levels {
            code
        } else {
            code.append(PUSH0)
        }
    };
    let root = op(BytecodeBuilder::default(), TIMESTAMP)
        .append_many([PUSH0; 5])
        .push_address(level(1))
        .append_many([GAS, CALL])
        .stop()
        .build();
    let mut db = MemoryDatabase::default().account_code(CONTRACT, root);
    for i in 1..=levels {
        let code = staticcall(BytecodeBuilder::default(), i);
        let code = if i == levels {
            code
        } else {
            let code =
                code.append_many([PUSH0; 5]).push_address(level(i + 1)).append_many([GAS, CALL]);
            if status_as_gas {
                code
            } else {
                code.append(POP)
            }
        };
        db = db.account_code(level(i), code.append(STATICCALL).stop().build());
        db = with_delegation(db, delegator(i), delegate(i));
    }
    let run_under = |cap: u64| {
        let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
        let ctx = context(db.clone())
            .with_tx_runtime_limits(limits)
            .with_volatile_access_disabled_from(1);
        let mut evm = MegaEvm::new(ctx).with_inspector(AtTheCall::default());
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let gas = evm.inspector().before.expect("the deepest level reached its call").0;
        (run, gas.remaining() - gas.tracker().withheld(), gas.remaining())
    };
    let first = 1_000_000;
    let (_, first_spendable, whole) = run_under(first);
    assert!(first_spendable < whole, "the first run detains the deepest level");
    let (run, held, _) = run_under(first - first_spendable + spendable);
    assert_eq!(held, spendable, "what the deepest level may spend at its call");
    let read = |address: Address| run.outcome.state.contains_key(&address);
    let delegators = (1..=levels).filter(|&i| read(delegator(i))).count();
    let delegates = (1..=levels).filter(|&i| read(delegate(i))).count();
    (delegators, delegates)
}

/// The detention tail of a call refused at its delegate is bounded by the warm access it keeps:
/// once what the limit leaves is below the cold access, each such call reads its cold target and
/// is stopped at the cold surcharge, keeping 100 of compute. A call stack built in advance, whose
/// levels each make that call as they resume, reads at most 29 delegators of the beneficiary from
/// 2,999, whether each level pays a `POP` of the status first, 102 a call, 43 left, or takes the
/// status as the call's gas, 100 a call, 99 left. The tail every detained call has, which this path
/// does not change, is wider: a call of a cold delegator of another cold account reads both before
/// it charges their 5,900, so the same stack reads 58 delegators and their 58 delegates from 5,999
/// with the `POP`s, 85 left, and 59 and 59 without, 99 left.
#[test]
fn test_a_detained_call_stack_reads_a_bounded_tail() {
    let warm = entry(GasId::warm_storage_read_cost());
    let surcharge = entry(GasId::cold_account_additional_cost());
    // The first call keeps the warm access; each later one also its caller's `POP`, if any.
    let tail = |window: u64, pop: u64| 1 + (window - 1 - warm) / (warm + pop);
    let double = warm + 2 * surcharge + warm;
    for (status_as_gas, pop) in [(false, 2), (true, 0)] {
        let case = format!("status as gas {status_as_gas}");
        // The fee settlement loads the beneficiary's account in every transaction, so only the
        // delegators tell what the tail read.
        let (delegators, _) =
            run_pre_built_stack(32, warm + surcharge - 1, status_as_gas, |_| BENEFICIARY);
        assert_eq!(tail(warm + surcharge, pop), 29, "{case}");
        assert_eq!(delegators, 29, "{case}: 29 delegators of the beneficiary");
        let doubly_cold =
            run_pre_built_stack(62, double - 1, status_as_gas, |i| stack_address(3, i));
        let calls = tail(double, pop);
        assert_eq!(calls, 58 + status_as_gas as u64, "{case}");
        assert_eq!(doubly_cold, (calls as usize, calls as usize), "{case}: and as many delegates");
    }
}

/// Runs a transaction whose own frame reads the block's timestamp and then calls `CHILD`, holding
/// `whole` at its first call opcode, of which it may spend `spendable`, with access off from
/// `CHILD` down. Returns the run and what [`AtTheCall`] recorded. The cap is derived from a run
/// under another, which detains the child too: the child's spendable gas at the call is the cap
/// less what the transaction computed since its read.
fn run_detained_child(
    child: &Bytes,
    pushes: u64,
    whole: u64,
    spendable: u64,
    warm: bool,
) -> (Run, AtTheCall) {
    // The parent stops once the call returned, which costs nothing of what the limit leaves it.
    let parent = op(BytecodeBuilder::default(), TIMESTAMP)
        .append_many([PUSH0; 5])
        .push_address(CHILD)
        .push_number(pushes + whole)
        .append(CALL)
        .stop()
        .build();
    let run_under = |cap: u64| {
        let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone());
        let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
        let ctx = context(db).with_tx_runtime_limits(limits).with_volatile_access_disabled_from(1);
        let mut evm = MegaEvm::new(ctx).with_inspector(AtTheCall::default());
        let tx =
            if warm { listing(CONTRACT, DELEGATOR, BELOW) } else { tx(CALLER, CONTRACT, BELOW) };
        let run = run_on(&mut evm, tx);
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let at_the_call = evm.inspector().clone();
        (run, at_the_call)
    };
    let held = |at_the_call: &AtTheCall| {
        let gas = at_the_call.before.expect("the child reached its call").0;
        (gas.remaining() - gas.tracker().withheld(), gas.remaining())
    };
    // The call the transaction's own frame makes, a cold one, fits under it, and what the child may
    // spend at its call is below what it holds.
    let first = cold_access() + whole / 2;
    let (_, at_the_call) = run_under(first);
    let (first_spendable, _) = held(&at_the_call);
    assert!(first_spendable < whole, "the first run detains the child: {first_spendable}");
    let (run, at_the_call) = run_under(first - first_spendable + spendable);
    assert_eq!(held(&at_the_call), (spendable, whole), "what the child holds at the call");
    (run, at_the_call)
}

/// Gas detention applies to what a refusal at a delegate pays as to the same charges with access
/// on: the warm access, the memory, then the cold surcharge, each a regular charge of the
/// spendable part. A `STATICCALL` of a delegator of the beneficiary, from a frame holding 5,200 of
/// which it may spend a part, is stopped at its static gas when it may spend 99, having read
/// nothing, and hands back all 5,200. With a cold delegator, the frame that may spend from 100 to
/// 2,999 reaches the delegate, its target read, and is stopped at the cold surcharge with
/// `MegaLimitExceeded(2, limit)`, handing back 5,100; one that may spend 3,000 is refused, paying
/// the cold access, and hands back 2,200. With a delegator the access list warms, one that may
/// spend 100 is refused, paying the warm access, and hands back 5,100.
///
/// With an output range of 32 words, whose memory costs 98, the memory is charged before the
/// surcharge, so a stop at the surcharge keeps it: a frame that may spend 197 is stopped at the
/// memory, its target not read, handing back 5,100; one that may spend 1,000 or 3,097 is stopped at
/// the surcharge, handing back 5,002; one that may spend 3,098 is refused, handing back 2,102.
#[test]
fn test_a_refusal_at_the_delegate_pays_the_targets_access_within_the_compute_limit() {
    let warm = entry(GasId::warm_storage_read_cost());
    let (plain, plain_pushes) = call_of(STATICCALL, DELEGATOR, 0, 0);
    let words = 32;
    let memory = memory_cost(words);
    let ranged =
        call_with_ranges(BytecodeBuilder::default(), DELEGATOR, (0, 0), (0, words as u32 * 32));
    // Four pushes of a number, the address's and a `PUSH0`.
    let ranged_pushes = 4 * 3 + 3 + 2;
    // (whether the call has the output range, whether the access list warms the delegator, what
    // the child may spend, whether it is refused rather than stopped, what it hands back, whether
    // it reads the delegator)
    for (with_memory, listed, spendable, is_refused, returned, reads) in [
        (false, false, warm - 1, false, 5_200, false),
        (false, false, warm, false, 5_200 - warm, true),
        (false, false, 2 * warm, false, 5_200 - warm, true),
        (false, false, cold_access() - 1, false, 5_200 - warm, true),
        (false, false, cold_access(), true, 5_200 - cold_access(), true),
        (false, true, warm - 1, false, 5_200, false),
        (false, true, warm, true, 5_200 - warm, true),
        (true, false, warm + memory - 1, false, 5_200 - warm, false),
        (true, false, 1_000, false, 5_200 - warm - memory, true),
        (true, false, cold_access() + memory - 1, false, 5_200 - warm - memory, true),
        (true, false, cold_access() + memory, true, 5_200 - cold_access() - memory, true),
    ] {
        let case = format!("memory {with_memory}, listed {listed}, spendable {spendable}");
        let (child, pushes) =
            if with_memory { (&ranged, ranged_pushes) } else { (&plain, plain_pushes) };
        let (run, at_the_call) = run_detained_child(child, pushes, 5_200, spendable, listed);
        let limit = run.limit.expect("the timestamp detains");
        let ending = if is_refused {
            refused(VolatileDataAccess::BENEFICIARY_BALANCE)
        } else {
            (InstructionResult::Revert, stop_data(limit))
        };
        assert_eq!(at_the_call.child, Some(ending), "{case}");
        assert_eq!(at_the_call.returned, Some(returned), "{case}: handed back");
        assert_eq!(run.outcome.state.contains_key(&DELEGATOR), reads, "{case}: read");
        assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP, "{case}");
    }
}

/// A value call refused at its delegate always pays its cold surcharge: the value transfer it was
/// charged from its spendable gas before the load, and is given back, leaves room for the
/// surcharge there, so the surcharge is never gas detention's crossing. A value `CALL` of a cold
/// delegator of the beneficiary, from a frame that cannot fund the value and holds 20,000 at the
/// call, of which it may spend its static gas and the value transfer, 11,400, reaches the delegate
/// and is refused, paying the cold access and handing back 17,000; one that may spend 11,399 is
/// stopped at the value transfer, the charge that crosses, before its target is read, handing back
/// 19,900, the value transfer not among what it spent.
#[test]
fn test_a_detained_value_call_refused_at_the_delegate_pays_its_surcharge() {
    let warm = entry(GasId::warm_storage_read_cost());
    let (child, pushes) = call_of(CALL, DELEGATOR, 1, 0);
    let whole = 20_000;
    let reach = warm + value_transfer();
    // (what the child may spend, whether it is refused rather than stopped, what it hands back,
    // whether it reads the delegator)
    for (spendable, is_refused, returned, reads) in
        [(reach, true, whole - cold_access(), true), (reach - 1, false, whole - warm, false)]
    {
        let (run, at_the_call) = run_detained_child(&child, pushes, whole, spendable, false);
        let limit = run.limit.expect("the timestamp detains");
        let ending = if is_refused {
            refused(VolatileDataAccess::BENEFICIARY_BALANCE)
        } else {
            (InstructionResult::Revert, stop_data(limit))
        };
        assert_eq!(at_the_call.child, Some(ending), "{spendable}");
        assert_eq!(at_the_call.returned, Some(returned), "{spendable}: handed back");
        assert_eq!(run.outcome.state.contains_key(&DELEGATOR), reads, "{spendable}: read");
    }
}

/// The identity precompile, whose address every transaction finds warm.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");

/// A call runs a precompile only once it reaches the precompile's dispatch, after its own steps,
/// whatever code the state holds at the precompile's address. With access off, a `STATICCALL` of
/// the identity precompile's address, which the state gives a delegation indicator naming the
/// beneficiary, with an input of 64 bytes and an output range past it, is refused at the
/// delegate's load: the address is warm, so the frame pays the warm access and the memory its
/// ranges grew, four words, and the precompile never runs. With access on, the same call runs the
/// precompile.
#[test]
fn test_a_precompile_address_delegating_to_the_beneficiary_is_refused_before_it_runs() {
    let warm = entry(GasId::warm_storage_read_cost());
    let child = call_with_ranges(BytecodeBuilder::default(), IDENTITY, (0, 64), (64, 64));
    let db = || {
        with_delegation(MemoryDatabase::default(), IDENTITY, BENEFICIARY)
            .account_code(CONTRACT, calls_child(BytecodeBuilder::default(), None).stop().build())
            .account_code(CHILD, child.clone())
    };

    let mut evm = MegaEvm::new(context(db()).with_volatile_access_disabled_from(1))
        .with_inspector(AtTheCall::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    let at_the_call = evm.inspector();
    assert_eq!(at_the_call.child, Some(refused(VolatileDataAccess::BENEFICIARY_BALANCE)));
    let (before, after) = (at_the_call.before.unwrap().0, at_the_call.after.unwrap().0);
    assert_eq!(before.remaining() - after.remaining(), warm + memory_cost(4));

    let mut evm = MegaEvm::new(context(db())).with_inspector(Calls::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    let call = evm.inspector().calls.iter().find(|call| call.target == IDENTITY);
    assert!(call.is_some_and(|call| call.precompile_ran), "with access on the precompile runs");
}

/// Contracts that load an account no transaction here funds, then do something else.
const LOADS_THEN_CALLS: Address = address!("0000000000000000000000000000000000d00006");
const LOADS_THEN_READS_THE_TIME: Address = address!("0000000000000000000000000000000000d00007");
const LOADS: Address = address!("0000000000000000000000000000000000d00008");

/// A refusal at a delegate pays for the target its own call loaded, and for no account an earlier
/// opcode or transaction loaded. With access off from the transaction's own frame, on one EVM:
/// a frame that loads a cold account and stops leaves nothing for the next transaction, whose
/// `TIMESTAMP` after the same load is refused for nothing; and a `STATICCALL` of a delegator of the
/// beneficiary the access list warms, after the same cold load in its own frame, pays the warm
/// access, 100, not the cold one.
#[test]
fn test_a_refusal_at_the_delegate_pays_for_its_own_target_alone() {
    let loads = || BytecodeBuilder::default().push_address(UNTOUCHED).append_many([BALANCE, POP]);
    let load = 3 + cold_access() + 2;
    let (call, pushes) = call_of(STATICCALL, DELEGATOR, 0, 0);
    let then_calls: Bytes = [loads().build().as_ref(), call.as_ref()].concat().into();
    let db = with_delegation(MemoryDatabase::default(), DELEGATOR, BENEFICIARY)
        .account_code(LOADS, loads().stop().build())
        .account_code(LOADS_THEN_READS_THE_TIME, loads().append(TIMESTAMP).stop().build())
        .account_code(LOADS_THEN_CALLS, then_calls);
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(0))
        .with_inspector(Calls::default());
    let mut last = |tx: MegaTransaction| {
        let run = run_on(&mut evm, tx);
        let frame = evm.inspector().calls.last().expect("the transaction's own frame");
        (run, frame.result, frame.output.clone(), frame.spent)
    };

    let (run, result, _, spent) = last(tx(CALLER, LOADS, BELOW));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!((result, spent), (InstructionResult::Stop, load));

    let (_, result, output, spent) = last(tx(CALLER, LOADS_THEN_READS_THE_TIME, BELOW));
    assert_eq!((result, output), refused(VolatileDataAccess::TIMESTAMP));
    assert_eq!(spent, load, "a direct refusal pays nothing");

    let _ = last(tx(CALLER, LOADS, BELOW));
    let warm = entry(GasId::warm_storage_read_cost());
    let (_, result, output, spent) = last(listing(LOADS_THEN_CALLS, DELEGATOR, BELOW));
    assert_eq!((result, output), refused(VolatileDataAccess::BENEFICIARY_BALANCE));
    assert_eq!(spent, load + pushes + warm, "the warm target's access");
}

/// With access off, a call's target is a required read once the call reaches its load, and no
/// read before: a `CALLCODE` of a delegator of the beneficiary that holds the gas to reach its
/// target gives the transaction no outcome when the target's account or code cannot be read,
/// rather than taking it for a target without a delegate; one with an input of 1 MiB its 5,000
/// gas cannot pay runs out of gas on its memory before its target's load, so the same unreadable
/// target fails nothing, and the transaction has its outcome.
#[test]
fn test_a_call_target_unreadable_where_the_call_loads_it_has_no_outcome() {
    let code_hash = Bytecode::new_eip7702(BENEFICIARY).hash_slow();
    for (in_len, left, reaches) in [(0, 100_000, true), (0x10_0000, 5_000, false)] {
        let (child, pushes) = call_of(CALLCODE, DELEGATOR, 0, in_len);
        for fails_on_code in [false, true] {
            let case = format!("input {in_len}, code {fails_on_code}");
            let inner = MemoryDatabase::default()
                .account_code(
                    CONTRACT,
                    calls_child(BytecodeBuilder::default(), Some(pushes + left)).stop().build(),
                )
                .account_code(CHILD, child.clone())
                .account_lazy_code(DELEGATOR, code_hash);
            let mut db = ErrorInjectingDatabase::new(inner);
            if fails_on_code {
                db.fail_on_code_by_hash = Some(code_hash);
            } else {
                db.fail_on_account = Some(DELEGATOR);
            }
            let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
                .with_inspector(Calls::default());
            let result = evm.execute_transaction(tx(CALLER, CONTRACT, BELOW));
            if reaches {
                assert!(result.is_err(), "{case}: {result:?}");
                continue;
            }
            let outcome = result.expect("the call reads nothing, so nothing fails");
            assert!(outcome.result.is_success(), "{case}");
            let child = evm.inspector().calls.iter().find(|call| call.target == CHILD).unwrap();
            assert_eq!(child.result, InstructionResult::MemoryOOG, "{case}: on the memory");
            assert!(!outcome.state.contains_key(&DELEGATOR), "{case}");
        }
    }
}

/// A call whose target delegates to another account than the beneficiary is not refused, and
/// pays with access off what it pays with access on: the cold access to its target and to the
/// target's delegate, whose code runs.
#[test]
fn test_a_call_of_a_delegator_of_another_account_pays_as_with_access_on() {
    let child = call(BytecodeBuilder::default(), CALL, DELEGATOR).stop().build();
    let spent = |disabled: bool| {
        let db = with_delegation(MemoryDatabase::default(), DELEGATOR, OTHER)
            .account_code(CONTRACT, call(BytecodeBuilder::default(), CALL, CHILD).stop().build())
            .account_code(CHILD, child.clone())
            .account_code(OTHER, BytecodeBuilder::default().stop().build());
        let mut ctx = context(db);
        if disabled {
            ctx = ctx.with_volatile_access_disabled_from(1);
        }
        let mut evm = MegaEvm::new(ctx).with_inspector(Calls::default());
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
        assert!(run.outcome.result.is_success(), "disabled {disabled}");
        let child = evm.inspector().calls.iter().find(|call| call.target == CHILD).unwrap();
        assert_eq!(child.result, InstructionResult::Stop, "disabled {disabled}");
        child.spent
    };
    let on = spent(false);
    // The five pushes and the address's, the `GAS`, the call's cold target and its delegate's
    // cold account, and the `POP`: the callee stops at once and hands back what it was forwarded.
    let cold =
        entry(GasId::warm_storage_read_cost()) + entry(GasId::cold_account_additional_cost());
    assert_eq!(on, 2 * 5 + 3 + 2 + 2 * cold + 2, "access on");
    assert_eq!(spent(true), on, "access off");
}

/// The fresh addresses a loop of reads calls, none of which any account holds.
fn fresh_addresses(count: u64) -> Vec<Address> {
    (0..count).map(|i| Address::from_word(B256::from(U256::from(0xa000_0000_u64 + i)))).collect()
}

/// A contract that calls `helper` once for each of `targets`, by `STATICCALL` with the target's
/// address as its 32 bytes of input and `gas` forwarded, or everything when `gas` is `None`, after
/// running `start`.
fn calls_the_helper_for_each(
    start: BytecodeBuilder,
    helper: Address,
    targets: &[Address],
    gas: Option<u64>,
) -> Bytes {
    let mut code = start;
    for target in targets {
        code = code
            .push_address(*target)
            .append_many([PUSH0, MSTORE, PUSH0, PUSH0])
            .push_number(32_u8)
            .append(PUSH0)
            .push_address(helper);
        code = match gas {
            Some(gas) => code.push_number(gas),
            None => code.append(GAS),
        };
        code = code.append_many([STATICCALL, POP]);
    }
    code.stop().build()
}

/// A helper that `STATICCALL`s the address its input holds, forwarding nothing, with an input of
/// `in_len` bytes and no output; and what its pushes cost.
fn calls_its_input(in_len: u32) -> (Bytes, u64) {
    let code = BytecodeBuilder::default().append_many([PUSH0, PUSH0]);
    let (code, pushes) = if in_len == 0 {
        (code.append(PUSH0), 2 + 2 + 2)
    } else {
        (code.push_number(in_len), 2 + 2 + 3)
    };
    let code = code.append_many([PUSH0, PUSH0, CALLDATALOAD, PUSH0, STATICCALL]);
    (code.build(), pushes + 2 + 2 + 3 + 2)
}

/// Appends a call of `disableVolatileDataAccess()`, which switches access off for the running
/// frame and every frame below it until the running frame returns.
fn disables_volatile_access(code: BytecodeBuilder) -> BytecodeBuilder {
    let disable = IMegaAccessControl::disableVolatileDataAccessCall {}.abi_encode();
    code.mstore(0, &disable)
        .append_many([PUSH0, PUSH0])
        .push_number(disable.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append_many([GAS, CALL, POP])
}

/// One run of a loop of reads: the regular gas the transaction spent, and how many of the loop's
/// targets its database was asked for.
struct ReadLoop {
    gas: u64,
    reads: usize,
}

impl ReadLoop {
    fn run(db: MemoryDatabase, targets: &[Address]) -> Self {
        let record = SharedWitnessRecord::default();
        let outcome = MegaEvm::new(context(RecordingDatabase::new(db, record.clone())))
            .execute_transaction(tx(CALLER, CONTRACT, BELOW))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let record = record.borrow();
        let reads = targets.iter().filter(|target| record.accounts.contains_key(*target)).count();
        Self { gas: outcome.gas.regular, reads }
    }
}

/// With access off, a frame cannot read an account it does not pay for. A contract switches its
/// access off, then calls a warm helper for each of 1,000 fresh addresses, forwarding the helper
/// exactly the gas of its pushes, so the helper reaches its `STATICCALL` of the address holding
/// nothing: the call halts at its static gas and no address is read, as with access on.
/// Forwarding the helper the static gas and the cold access too, every address is read, at the
/// cold access each, and the transaction spends with access off what it spends with access on.
///
/// The same holds for a frame gas detention holds: in a contract that read the block's timestamp
/// before it switched its access off, a helper whose call's memory crosses the limit is stopped
/// at its memory, before it reads its target.
///
/// A delegator of the beneficiary is read for its access too: a helper holding the static gas, the
/// cold access and the delegate's warm access reaches the delegate and is refused, paying the
/// delegator's cold access, and its revert hands back the delegate's warm access alone. Each read
/// costs the loop the cold access, as with access on, where the call goes on to the delegate and
/// spends its warm access too.
#[test]
fn test_an_access_off_loop_reads_no_account_it_does_not_pay_for() {
    const HELPER: Address = CHILD;
    let targets = fresh_addresses(1_000);
    let warm = entry(GasId::warm_storage_read_cost());
    let (helper, pushes) = calls_its_input(0);
    let start = |disabled: bool| {
        let code = BytecodeBuilder::default();
        if disabled {
            disables_volatile_access(code)
        } else {
            code
        }
    };
    let loop_db = |disabled: bool, gas: u64| {
        MemoryDatabase::default()
            .account_code(
                CONTRACT,
                calls_the_helper_for_each(start(disabled), HELPER, &targets, Some(gas)),
            )
            .account_code(HELPER, helper.clone())
    };

    let starved = ReadLoop::run(loop_db(true, pushes), &targets);
    let starved_on = ReadLoop::run(loop_db(false, pushes), &targets);
    let paying = ReadLoop::run(loop_db(true, pushes + cold_access()), &targets);
    let paying_on = ReadLoop::run(loop_db(false, pushes + cold_access()), &targets);

    let (crossing, _) = calls_its_input(0x32_0000);
    let detained_start = disables_volatile_access(op(BytecodeBuilder::default(), TIMESTAMP));
    let detained_db = MemoryDatabase::default()
        .account_code(CONTRACT, calls_the_helper_for_each(detained_start, HELPER, &targets, None))
        .account_code(HELPER, crossing);
    let detained = ReadLoop::run(detained_db, &targets);

    let delegators = fresh_addresses(500);
    let mut delegators_db = MemoryDatabase::default()
        .account_code(
            CONTRACT,
            calls_the_helper_for_each(
                start(true),
                HELPER,
                &delegators,
                Some(pushes + cold_access() + warm),
            ),
        )
        .account_code(HELPER, helper.clone());
    for delegator in &delegators {
        delegators_db = with_delegation(delegators_db, *delegator, BENEFICIARY);
    }
    let refused_reads = ReadLoop::run(delegators_db.clone(), &delegators);
    let delegators_on_db = delegators_db.account_code(
        CONTRACT,
        calls_the_helper_for_each(
            start(false),
            HELPER,
            &delegators,
            Some(pushes + cold_access() + warm),
        ),
    );
    let delegators_on = ReadLoop::run(delegators_on_db, &delegators);

    assert_eq!(starved.reads, 0, "a helper short of its static gas reads nothing");
    assert_eq!(starved_on.reads, 0, "with access on either");
    assert_eq!(paying.reads, targets.len(), "a helper with the cold access reads its target");
    assert_eq!(paying.reads, paying_on.reads, "as with access on");
    assert_eq!(
        paying.gas - starved.gas,
        paying_on.gas - starved_on.gas,
        "and the reads cost with access off what they cost with access on"
    );
    assert_eq!(paying.gas - starved.gas, targets.len() as u64 * cold_access(), "the cold access");
    assert_eq!(detained.reads, 0, "a helper stopped at its memory reads nothing");
    assert_eq!(refused_reads.reads, delegators.len(), "a delegator of the beneficiary is read");
    assert_eq!(delegators_on.reads, delegators.len(), "as with access on");
    assert!(refused_reads.gas > delegators.len() as u64 * cold_access(), "for its cold access");
    // The two loops differ only in the call that switches access off, which the starved loops
    // measure.
    let disabling = starved.gas - starved_on.gas;
    assert_eq!(
        delegators_on.gas + disabling - refused_reads.gas,
        delegators.len() as u64 * warm,
        "for what it costs with access on, but the delegate's warm access"
    );
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
    // The pushes (2 + 2 + 3 + 2 + 2 + 3 + 3), the cold call, POP and GAS.
    let cold_call =
        entry(GasId::warm_storage_read_cost()) + entry(GasId::cold_account_additional_cost());
    assert_eq!(consumed, U256::from(17 + cold_call + 2 + 2));
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
