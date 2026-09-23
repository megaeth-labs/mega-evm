//! Gas detention: a read of volatile data caps the compute the transaction may still spend.
//!
//! The cap is relative: at the read, the transaction's compute — its regular gas, without state
//! and history gas that spilled onto it — may grow by at most the cap. Crossing it stops the
//! transaction with the revert-class stop every transaction-level limit uses. The limit a read
//! sets is its compute then plus the cap, so a transaction that stops has spent exactly its limit
//! and its intrinsic gas on the regular ledger: the allowance it had left when the charge failed
//! is spent, and the gas detention withheld is not.
//!
//! Every case runs below the execution cap, without a reservoir, and above it, with one.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS, TX_GAS_LIMIT_CAP},
    system::{IMegaLimitControl, LIMIT_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    volatile_data_access_disabled_revert_data, LimitCheck, LimitKind, MegaContext, MegaEvm,
    MegaLimitExceeded, MegaSpecId, MegaTransaction, MegaTransactionOutcome, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, BlockEnv, TxEnv},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, InstructionResult, Interpreter,
    },
    Database, Inspector,
};

const CALLER: Address = address!("0000000000000000000000000000000000d00000");
const CONTRACT: Address = address!("0000000000000000000000000000000000d00001");
const CHILD: Address = address!("0000000000000000000000000000000000d00002");
const DELEGATOR: Address = address!("0000000000000000000000000000000000d00003");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000bef000");

const CAP: u64 = BLOCK_ENV_ACCESS_COMPUTE_GAS;

/// Below the execution cap: no reservoir.
const BELOW: u64 = 100_000_000;
/// Above it: a reservoir of 100,000,000.
const ABOVE: u64 = TX_GAS_LIMIT_CAP + 100_000_000;
const TIERS: [u64; 2] = [BELOW, ABOVE];

/// Rounds of [`work`] spent before a read: 5,283,600 of compute.
const WORK: u32 = 1_700;

/// The compute one round of [`work`] spends once its memory is expanded.
const WORK_ROUND: u64 = 3_108;

/// Appends a copy of 32 KiB within memory: 3,075 gas, and 5,120 more to expand the memory the
/// first time. A round of it costs the interpreter little, whatever it costs in gas.
fn copy(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(0x8000_u16).append_many([PUSH0, PUSH0, MCOPY])
}

fn block() -> BlockEnv {
    BlockEnv {
        number: U256::from(300),
        beneficiary: BENEFICIARY,
        timestamp: U256::from(1_700_000_000),
        gas_limit: 10_000_000_000,
        prevrandao: Some(B256::repeat_byte(7)),
        slot_num: 9,
        ..Default::default()
    }
}

fn context<DB: Database>(db: DB) -> MegaContext<DB> {
    MegaContext::new(db, MegaSpecId::SATIN).with_block(block()).with_chain(zero_fee_l1_block_info())
}

fn tx(caller: Address, to: Address, gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv { caller, kind: TxKind::Call(to), gas_limit, ..Default::default() }))
}

/// What a transaction did, and what detention made of it.
struct Run {
    outcome: MegaTransactionOutcome,
    limit: Option<u64>,
    accessed: VolatileDataAccess,
}

fn run_on<INSP>(evm: &mut MegaEvm<MemoryDatabase, INSP>, tx: MegaTransaction) -> Run
where
    INSP: Inspector<MegaContext<MemoryDatabase>, EthInterpreter>,
{
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let detention = evm.ctx().detention();
    Run { outcome, limit: detention.compute_limit(), accessed: detention.accessed() }
}

fn execute(db: MemoryDatabase, tx: MegaTransaction) -> Run {
    run_on(&mut MegaEvm::new(context(db)), tx)
}

/// The regular gas a call from `CALLER` spends before its first instruction.
fn intrinsic(gas_limit: u64) -> u64 {
    let db =
        MemoryDatabase::default().account_code(CONTRACT, BytecodeBuilder::default().stop().build());
    execute(db, tx(CALLER, CONTRACT, gas_limit)).outcome.gas.regular
}

/// Appends a loop that never ends, 3,094 gas a round.
fn spin(code: BytecodeBuilder) -> Bytes {
    let dest = code.len() as u32;
    copy(code.append(JUMPDEST)).push_number(dest).append(JUMP).build()
}

/// Appends `rounds` rounds of a counting loop that also copies memory, [`WORK_ROUND`] gas a
/// round.
fn work(code: BytecodeBuilder, rounds: u32) -> BytecodeBuilder {
    let code = code.push_number(rounds);
    let dest = code.len() as u32;
    copy(code.append(JUMPDEST))
        .push_number(1_u8)
        .append_many([SWAP1, SUB, DUP1])
        .push_number(dest)
        .append_many([JUMPI, POP])
}

/// Appends `rounds` rounds of a counting loop, twenty-six gas a round.
fn burn(code: BytecodeBuilder, rounds: u32) -> BytecodeBuilder {
    let code = code.push_number(rounds);
    let dest = code.len() as u32;
    code.append(JUMPDEST)
        .push_number(1_u8)
        .append_many([SWAP1, SUB, DUP1])
        .push_number(dest)
        .append_many([JUMPI, POP])
}

/// Appends a call of `scheme` to `to` forwarding all gas, dropping its status.
fn call(code: BytecodeBuilder, scheme: u8, to: Address) -> BytecodeBuilder {
    let code = code.append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
    let code = if matches!(scheme, CALL | CALLCODE) { code.append(PUSH0) } else { code };
    code.push_address(to).append(GAS).append(scheme).append(POP)
}

/// The revert data of the detention stop at `limit`.
fn stop_data(limit: u64) -> Bytes {
    MegaLimitExceeded { kind: LimitKind::ComputeGas.as_u8(), limit }.abi_encode().into()
}

/// Asserts the transaction was stopped by detention, having spent exactly its limit.
fn assert_stopped(run: &Run, intrinsic: u64) -> u64 {
    let limit = run.limit.expect("a read set a limit");
    match &run.outcome.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(output, &stop_data(limit)),
        other => panic!("expected the detention stop, got {other:?}"),
    }
    assert_eq!(
        run.outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit,
            used: limit,
            frame_local: false
        })
    );
    assert_eq!(
        run.outcome.gas.regular,
        intrinsic + limit,
        "the transaction computed up to its limit and not one unit past it"
    );
    assert_eq!(run.outcome.gas.state, 0, "a stop keeps no state");
    assert!(run.outcome.result.logs().is_empty(), "a stop keeps no log");
    limit
}

/* ---------- every read ---------- */

/// A piece of code appended to a program.
type Append = fn(BytecodeBuilder) -> BytecodeBuilder;

/// One volatile read: the code that makes it, what else the database needs, and the kind it
/// records.
struct Read {
    name: &'static str,
    code: Append,
    db: fn(MemoryDatabase) -> MemoryDatabase,
    access: VolatileDataAccess,
}

fn no_setup(db: MemoryDatabase) -> MemoryDatabase {
    db
}

fn op(code: BytecodeBuilder, opcode: u8) -> BytecodeBuilder {
    code.append(opcode).append(POP)
}

fn on_beneficiary(code: BytecodeBuilder, opcode: u8) -> BytecodeBuilder {
    code.push_address(BENEFICIARY).append(opcode).append(POP)
}

/// Every opcode read of volatile data.
fn reads() -> Vec<Read> {
    vec![
        Read {
            name: "NUMBER",
            code: |c| op(c, NUMBER),
            db: no_setup,
            access: VolatileDataAccess::BLOCK_NUMBER,
        },
        Read {
            name: "TIMESTAMP",
            code: |c| op(c, TIMESTAMP),
            db: no_setup,
            access: VolatileDataAccess::TIMESTAMP,
        },
        Read {
            name: "COINBASE",
            code: |c| op(c, COINBASE),
            db: no_setup,
            access: VolatileDataAccess::COINBASE,
        },
        Read {
            name: "PREVRANDAO",
            code: |c| op(c, DIFFICULTY),
            db: no_setup,
            access: VolatileDataAccess::PREV_RANDAO,
        },
        Read {
            name: "GASLIMIT",
            code: |c| op(c, GASLIMIT),
            db: no_setup,
            access: VolatileDataAccess::GAS_LIMIT,
        },
        Read {
            name: "BASEFEE",
            code: |c| op(c, BASEFEE),
            db: no_setup,
            access: VolatileDataAccess::BASE_FEE,
        },
        Read {
            name: "BLOBBASEFEE",
            code: |c| op(c, BLOBBASEFEE),
            db: no_setup,
            access: VolatileDataAccess::BLOB_BASE_FEE,
        },
        Read {
            name: "SLOTNUM",
            code: |c| op(c, SLOTNUM),
            db: no_setup,
            access: VolatileDataAccess::SLOT_NUM,
        },
        Read {
            name: "BLOCKHASH",
            code: |c| c.push_number(299_u16).append(BLOCKHASH).append(POP),
            db: no_setup,
            access: VolatileDataAccess::BLOCK_NUMBER | VolatileDataAccess::BLOCK_HASH,
        },
        Read {
            name: "BALANCE",
            code: |c| on_beneficiary(c, BALANCE),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "EXTCODESIZE",
            code: |c| on_beneficiary(c, EXTCODESIZE),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "EXTCODEHASH",
            code: |c| on_beneficiary(c, EXTCODEHASH),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "EXTCODECOPY",
            code: |c| {
                c.push_number(32_u8)
                    .append_many([PUSH0, PUSH0])
                    .push_address(BENEFICIARY)
                    .append(EXTCODECOPY)
            },
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "CALL",
            code: |c| call(c, CALL, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "CALLCODE",
            code: |c| call(c, CALLCODE, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "DELEGATECALL",
            code: |c| call(c, DELEGATECALL, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "STATICCALL",
            code: |c| call(c, STATICCALL, BENEFICIARY),
            db: no_setup,
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "CALL to an EIP-7702 delegator of the beneficiary",
            code: |c| call(c, CALL, DELEGATOR),
            db: |db| with_delegation(db, DELEGATOR, BENEFICIARY),
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "SELFDESTRUCT to the beneficiary",
            code: |c| call(c, CALL, CHILD),
            db: |db| {
                db.account_code(
                    CHILD,
                    BytecodeBuilder::default()
                        .push_address(BENEFICIARY)
                        .append(SELFDESTRUCT)
                        .build(),
                )
            },
            access: VolatileDataAccess::BENEFICIARY_BALANCE,
        },
        Read {
            name: "SLOAD of the Oracle's storage",
            code: |c| call(c, CALL, ORACLE_CONTRACT_ADDRESS),
            db: |db| {
                db.account_code(
                    ORACLE_CONTRACT_ADDRESS,
                    BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build(),
                )
            },
            access: VolatileDataAccess::ORACLE,
        },
    ]
}

/// Gives `address` the `0xef0100 || to` designator an applied EIP-7702 authorization leaves.
fn with_delegation(mut db: MemoryDatabase, address: Address, to: Address) -> MemoryDatabase {
    use revm::{database::AccountState, state::Bytecode};
    let bytecode = Bytecode::new_eip7702(to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).expect("the account is in memory");
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
    db
}

/// Every read caps the transaction from where it read, not from its start: the transaction first
/// spends more compute than the cap, then reads, then computes forever, and it stops having spent
/// exactly its compute at the read plus the cap.
#[test]
fn test_every_volatile_read_caps_the_transaction_from_where_it_read() {
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        for read in reads() {
            let code = spin((read.code)(work(BytecodeBuilder::default(), WORK)));
            let db = (read.db)(MemoryDatabase::default().account_code(CONTRACT, code));
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            let limit = assert_stopped(&run, intrinsic);
            let at_read = limit - CAP;
            assert!(
                at_read > u64::from(WORK) * WORK_ROUND &&
                    at_read < u64::from(WORK) * WORK_ROUND + 20_000,
                "{}: the cap counts from the read, which came after {at_read} of compute",
                read.name
            );
            assert_eq!(run.accessed, read.access, "{}", read.name);
        }
    }
}

/// The cap is relative: a transaction that spent more than the cap before it read may still
/// spend the cap after.
#[test]
fn test_the_cap_counts_from_a_spend_larger_than_itself() {
    let before = 7_000_u32;
    for gas_limit in TIERS {
        let code = spin(op(work(BytecodeBuilder::default(), before), TIMESTAMP));
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let limit = assert_stopped(&run, intrinsic(gas_limit));
        assert!(limit - CAP > u64::from(before) * WORK_ROUND, "{limit}");
        assert!(limit - CAP > CAP);
    }
}

/// Without the read, the same computation is not capped: it runs until the frame's own gas is
/// gone, and halts.
#[test]
fn test_without_a_read_nothing_is_capped() {
    for gas_limit in TIERS {
        let code = spin(work(BytecodeBuilder::default(), WORK));
        let db = MemoryDatabase::default().account_code(CONTRACT, code);
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(
            matches!(
                run.outcome.result,
                ExecutionResult::Halt { reason: mega_evm::MegaHaltReason::Base(_), .. }
            ),
            "{:?}",
            run.outcome.result
        );
        assert_eq!(run.limit, None);
        assert_eq!(run.outcome.limit_exceeded, None);
        assert_eq!(run.accessed, VolatileDataAccess::empty());
    }
}

/// A transaction whose sender, recipient or applied EIP-7702 authority is the beneficiary is
/// detained from its first instruction: its limit is the cap.
#[test]
fn test_a_transaction_touching_the_beneficiary_is_detained_from_the_start() {
    use revm::{
        context::transaction::TransactionType,
        context_interface::{
            either::Either,
            transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
        },
    };
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let spinner = spin(BytecodeBuilder::default());

        // The sender.
        let db = MemoryDatabase::default().account_code(CONTRACT, spinner.clone());
        let run_sender = execute(db, tx(BENEFICIARY, CONTRACT, gas_limit));
        // A sender's intrinsic gas is the same whoever it is.
        assert_eq!(assert_stopped(&run_sender, intrinsic), CAP, "the sender");

        // The recipient.
        let db = MemoryDatabase::default().account_code(BENEFICIARY, spinner.clone());
        let run_recipient = execute(db, tx(CALLER, BENEFICIARY, gas_limit));
        assert_eq!(assert_stopped(&run_recipient, intrinsic), CAP, "the recipient");

        // An applied authority.
        let authorization = Either::Right(RecoveredAuthorization::new_unchecked(
            Authorization { chain_id: U256::ZERO, address: CHILD, nonce: 0 },
            RecoveredAuthority::Valid(BENEFICIARY),
        ));
        let authorizing = OpTx(op_transaction(TxEnv {
            tx_type: TransactionType::Eip7702 as u8,
            caller: CALLER,
            kind: TxKind::Call(CONTRACT),
            gas_limit,
            gas_priority_fee: Some(0),
            authorization_list: vec![authorization],
            ..Default::default()
        }));
        let db = MemoryDatabase::default().account_code(CONTRACT, spinner);
        let run_authority = execute(db, authorizing);
        assert_eq!(run_authority.limit, Some(CAP), "the authority");
        assert!(matches!(run_authority.outcome.result, ExecutionResult::Revert { .. }));
        assert_eq!(run_authority.accessed, VolatileDataAccess::BENEFICIARY_BALANCE);
    }
}

/// An account read of anything but the beneficiary, and a storage read of anything but the
/// Oracle, are not volatile.
#[test]
fn test_other_accounts_and_storage_are_not_volatile() {
    let code = BytecodeBuilder::default()
        .push_address(CHILD)
        .append(BALANCE)
        .append(POP)
        .append_many([PUSH0, SLOAD, POP]);
    let code = call(call(code, CALL, CHILD), STATICCALL, CHILD).stop().build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_code(CHILD, BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]).build());
    let run = execute(db, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success());
    assert_eq!(run.accessed, VolatileDataAccess::empty());
    assert_eq!(run.limit, None);
}

/// `BLOBHASH` reads the transaction's own blob hashes, which nothing but the transaction decides:
/// it is not volatile.
#[test]
fn test_blobhash_is_not_volatile() {
    let code = BytecodeBuilder::default().append_many([PUSH0, BLOBHASH, POP, STOP]).build();
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, BELOW),
    );
    assert!(run.outcome.result.is_success());
    assert_eq!(run.accessed, VolatileDataAccess::empty());
}

/* ---------- the limit ---------- */

/// A later read lowers the limit only when its own is lower: with equal caps the first read
/// binds, in either order of block environment and Oracle.
#[test]
fn test_the_most_restrictive_limit_binds_whatever_the_order() {
    assert_eq!(ORACLE_ACCESS_COMPUTE_GAS, CAP, "the two caps are equal today");
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP, STOP]).build();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        for block_env_first in [true, false] {
            let first = |c| {
                if block_env_first {
                    op(c, TIMESTAMP)
                } else {
                    call(c, CALL, ORACLE_CONTRACT_ADDRESS)
                }
            };
            let second = |c| {
                if block_env_first {
                    call(c, CALL, ORACLE_CONTRACT_ADDRESS)
                } else {
                    op(c, TIMESTAMP)
                }
            };
            // The first read, 1,000,000 of compute, the second read, then forever.
            let code = spin(second(work(first(BytecodeBuilder::default()), 300)));
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code)
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone());
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            let limit = assert_stopped(&run, intrinsic);
            assert!(limit < CAP + 100_000, "the first read set the limit: {limit}");
            assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP | VolatileDataAccess::ORACLE);
        }
    }
}

/// Spending exactly the cap after the read is within it; the limit is strict.
#[test]
fn test_spending_exactly_the_cap_completes() {
    // TIMESTAMP (2) and POP (2) leave 19,999,998 of the cap; the loop spends 26 a round after
    // the three gas its counter's push costs, and its last round ends in POP (2) and STOP.
    let rounds = (CAP - 2 - 2 - 3 - 2) / 26;
    let spent = 2 + 2 + 3 + u64::from(u32::try_from(rounds).unwrap()) * 26 + 2;
    assert!(spent <= CAP + 2, "{spent}");
    for gas_limit in TIERS {
        let code = burn(op(BytecodeBuilder::default(), TIMESTAMP), u32::try_from(rounds).unwrap())
            .stop()
            .build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(run.outcome.gas.regular, intrinsic(gas_limit) + spent);

        // One more round crosses it.
        let code =
            burn(op(BytecodeBuilder::default(), TIMESTAMP), u32::try_from(rounds).unwrap() + 1)
                .stop()
                .build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert_stopped(&run, intrinsic(gas_limit));
    }
}

/* ---------- across frames ---------- */

/// A child's read caps its caller after the child returns: the caller resumes on what the limit
/// leaves the transaction, and stops there.
#[test]
fn test_a_childs_read_caps_its_caller() {
    let child = work(op(BytecodeBuilder::default(), TIMESTAMP), 500).stop().build();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let parent = spin(call(BytecodeBuilder::default(), CALL, CHILD));
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent)
            .account_code(CHILD, child.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        let limit = assert_stopped(&run, intrinsic);
        assert!(limit < CAP + 100_000, "the child read near the start: {limit}");
    }
}

/// When a child crosses the cap, no caller resumes: the caller's code after the call — a write
/// and a log — never runs, and every frame returns the stop.
#[test]
fn test_no_caller_resumes_after_the_stop() {
    let child = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let parent = call(BytecodeBuilder::default(), CALL, CHILD)
        .sstore(U256::from(1), U256::from(1))
        .append_many([PUSH0, PUSH0, LOG0])
        .stop()
        .build();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone());
        let mut evm = MegaEvm::new(context(db)).with_inspector(Calls::default());
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
        let limit = assert_stopped(&run, intrinsic);
        assert!(run.outcome.state[&CONTRACT].storage.values().all(|slot| !slot.is_changed()));
        let calls = &evm.inspector().calls;
        assert_eq!(
            calls.iter().map(|c| (c.target, c.result, c.output.clone())).collect::<Vec<_>>(),
            vec![
                (CHILD, InstructionResult::Revert, stop_data(limit)),
                (CONTRACT, InstructionResult::Revert, stop_data(limit)),
            ]
        );
        assert!(!evm.inspector().opcodes.contains(&SSTORE), "the caller did not resume");
    }
}

/// The stop settles like an EIP-8037 revert: what the sender pays does not depend on whether
/// the transaction had a reservoir, because the reservoir and everything detention withheld go
/// back to it. Above the execution cap the reservoir comes back whole, less the body's history,
/// which it paid first.
#[test]
fn test_the_stop_bills_the_same_above_and_below_the_execution_cap() {
    let child = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let parent = spin(call(BytecodeBuilder::default(), CALL, CHILD));
    let used: Vec<_> = TIERS
        .iter()
        .map(|gas_limit| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, parent.clone())
                .account_code(CHILD, child.clone());
            let run = execute(db, tx(CALLER, CONTRACT, *gas_limit));
            assert_stopped(&run, intrinsic(*gas_limit));
            let body_history = run.outcome.gas.history;
            let reservoir = gas_limit.saturating_sub(TX_GAS_LIMIT_CAP).saturating_sub(body_history);
            assert_eq!(run.outcome.gas.reservoir_remaining, reservoir, "the reservoir came back");
            let gas = run.outcome.gas;
            (gas.gas_used, gas.regular, gas.state, gas.history)
        })
        .collect();
    assert_eq!(used[0], used[1]);
    assert!(used[0].0 < CAP + 1_000_000, "the stop does not burn the gas: {}", used[0].0);
}

/* ---------- real out-of-gas ---------- */

/// A frame detention withheld nothing from runs out of its own gas: it halts and burns what it
/// was given, below and above the execution cap.
#[test]
fn test_running_out_of_the_frames_own_gas_still_halts() {
    // The transaction has less than the cap: nothing is withheld.
    let code = spin(op(BytecodeBuilder::default(), TIMESTAMP));
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, 1_000_000),
    );
    assert!(matches!(run.outcome.result, ExecutionResult::Halt { .. }), "{:?}", run.outcome.result);
    assert_eq!(run.outcome.result.gas().tx_gas_used(), 1_000_000, "a halt burns the gas");
    assert_eq!(run.outcome.limit_exceeded, None);
    assert!(run.limit.is_some(), "the read still set a limit");

    // A child given less than the limit leaves halts on its own, and its caller resumes.
    let child = spin(BytecodeBuilder::default());
    let parent = op(BytecodeBuilder::default(), TIMESTAMP)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .push_number(100_000_u32)
        .append(CALL)
        .push_number(0_u8)
        .append(SSTORE)
        .stop()
        .build();
    for gas_limit in TIERS {
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, parent.clone())
            .account_code(CHILD, child.clone());
        let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let slot = run.outcome.state[&CONTRACT].storage.get(&U256::ZERO).unwrap();
        assert_eq!(slot.present_value, U256::ZERO, "the call failed: the child halted");
        assert!(slot.is_changed() || slot.original_value.is_zero());
    }
}

/// Writes after a read are held to the cap by the regular gas they spend. With room in the
/// reservoir for their state and history gas, a thousand fresh slots — 22,100,000 of compute —
/// cross it and stop the transaction. Without that room the writes drain the withheld gas first
/// and the frame runs out of its own gas where it would have undetained: it halts.
#[test]
fn test_writes_after_a_read_stop_at_the_cap_or_run_out_of_their_own_gas() {
    let mut code = op(BytecodeBuilder::default(), TIMESTAMP);
    for slot in 1..=1_000_u64 {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    let code = code.stop().build();

    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code.clone()),
        tx(CALLER, CONTRACT, ABOVE),
    );
    assert_stopped(&run, intrinsic(ABOVE));

    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, BELOW),
    );
    assert!(matches!(run.outcome.result, ExecutionResult::Halt { .. }), "{:?}", run.outcome.result);
    assert_eq!(run.outcome.limit_exceeded, None);
    assert_eq!(run.outcome.result.gas().tx_gas_used(), BELOW, "a halt burns the gas");
}

/* ---------- what is not marked ---------- */

/// A read that does not happen marks nothing, and neither does one whose opcode fails after it:
/// the frame halts, and the caller it halts back into computes on uncapped.
///
/// - The Oracle's slot is cold and the frame cannot pay the cold access, so the Host does not load
///   it. (The block beneficiary is always warm, so its load cannot be skipped this way.)
/// - A value transfer to the empty beneficiary loads it, then cannot pay the new account, and the
///   `CALL` fails.
#[test]
fn test_a_failed_load_caps_nothing() {
    // The push and SLOAD's static 100 leave less than the 2,000 more a cold slot costs.
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, STOP]).build();
    // 9,000 for the transfer and 25,000 for the new account do not fit in 30,000.
    let child = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(BENEFICIARY)
        .append_many([PUSH0, CALL, STOP])
        .build();
    for (callee, gas) in [(ORACLE_CONTRACT_ADDRESS, 2_000_u32), (CHILD, 30_000)] {
        let parent = BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(callee)
            .push_number(gas)
            .append(CALL)
            .push_number(0_u8)
            .append(SSTORE);
        // More compute than the cap.
        let parent = work(parent, 7_000).stop().build();
        for gas_limit in TIERS {
            let db = MemoryDatabase::default()
                .account_balance(CHILD, U256::from(10))
                .account_code(CONTRACT, parent.clone())
                .account_code(ORACLE_CONTRACT_ADDRESS, oracle.clone())
                .account_code(CHILD, child.clone());
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            assert!(run.outcome.result.is_success(), "{callee}: {:?}", run.outcome.result);
            let slot = run.outcome.state[&CONTRACT].storage.get(&U256::ZERO);
            assert!(
                slot.is_none_or(|slot| slot.present_value.is_zero()),
                "{callee}: the call failed"
            );
            assert!(run.outcome.gas.regular > CAP, "{callee}: the caller computed past the cap");
            assert_eq!(run.limit, None, "{callee}");
            assert_eq!(run.accessed, VolatileDataAccess::empty(), "{callee}");
        }
    }
}

/* ---------- refused reads ---------- */

/// Records every call's result and the gas it spent, and every opcode that ran.
#[derive(Default)]
struct Calls {
    calls: Vec<CallRecord>,
    opcodes: Vec<u8>,
}

struct CallRecord {
    target: Address,
    result: InstructionResult,
    output: Bytes,
    spent: u64,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Calls {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        use revm::interpreter::interpreter_types::Jumps;
        self.opcodes.push(interp.bytecode.opcode());
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let gas = outcome.result.gas;
        self.calls.push(CallRecord {
            target: inputs.target_address,
            result: outcome.result.result,
            output: outcome.result.output.clone(),
            spent: gas.limit() - gas.remaining(),
        });
    }
}

/// While volatile-data access is off, every read is refused: the frame reverts with
/// `VolatileDataAccessDisabled(accessType)`, having paid the static gas of what ran and nothing
/// more — not a cold access, not a copy's memory, not a call's value transfer — and the refused
/// read caps nothing.
#[test]
fn test_a_refused_read_reverts_the_frame_and_charges_its_static_gas() {
    // (code, the static gas of everything in it, the refused kind)
    let cases: Vec<(&str, Bytes, u64, VolatileDataAccess)> = vec![
        (
            "TIMESTAMP",
            BytecodeBuilder::default().append(TIMESTAMP).build(),
            2,
            VolatileDataAccess::TIMESTAMP,
        ),
        (
            "SLOTNUM",
            BytecodeBuilder::default().append(SLOTNUM).build(),
            2,
            VolatileDataAccess::SLOT_NUM,
        ),
        (
            "BLOCKHASH",
            BytecodeBuilder::default().push_number(299_u16).append(BLOCKHASH).build(),
            3 + 20,
            VolatileDataAccess::BLOCK_NUMBER,
        ),
        (
            "BALANCE",
            on_beneficiary(BytecodeBuilder::default(), BALANCE).build(),
            3 + 100,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "EXTCODECOPY",
            BytecodeBuilder::default()
                .push_number(1024_u16)
                .append_many([PUSH0, PUSH0])
                .push_address(BENEFICIARY)
                .append(EXTCODECOPY)
                .build(),
            3 + 2 + 2 + 3 + 100,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "CALL with value",
            BytecodeBuilder::default()
                .push_number(64_u8)
                .append_many([PUSH0, PUSH0, PUSH0])
                .push_number(1_u8)
                .push_address(BENEFICIARY)
                .append(GAS)
                .append(CALL)
                .build(),
            3 + 2 + 2 + 2 + 3 + 3 + 2 + 100,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "SELFDESTRUCT",
            on_beneficiary(BytecodeBuilder::default(), SELFDESTRUCT).build(),
            3 + 5_000,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
        (
            "SELFBALANCE of the beneficiary's own frame",
            BytecodeBuilder::default().append(SELFBALANCE).build(),
            5,
            VolatileDataAccess::BENEFICIARY_BALANCE,
        ),
    ];
    for gas_limit in TIERS {
        for (name, code, static_gas, refused) in &cases {
            // The beneficiary runs its own SELFBALANCE; every other read runs in the child.
            let target = if name.starts_with("SELFBALANCE") { BENEFICIARY } else { CHILD };
            let parent = call(BytecodeBuilder::default(), CALL, target).stop().build();
            let db = MemoryDatabase::default()
                .account_balance(CHILD, U256::from(10))
                .account_code(CONTRACT, parent)
                .account_code(target, code.clone());
            let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
                .with_inspector(Calls::default());
            let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
            assert!(run.outcome.result.is_success(), "{name}: the caller resumes");
            let record = &evm.inspector().calls[0];
            assert_eq!(record.target, target);
            assert_eq!(record.result, InstructionResult::Revert, "{name}");
            assert_eq!(
                record.output,
                volatile_data_access_disabled_revert_data(*refused),
                "{name}"
            );
            assert_eq!(record.spent, *static_gas, "{name}: the static gas of what ran");
            // Only the calls a transaction to the beneficiary makes read it; the refusal did not.
            if target == CHILD {
                assert_eq!(run.accessed, VolatileDataAccess::empty(), "{name}");
                assert_eq!(run.limit, None, "{name}");
            }
        }
    }
}

/// The refusal of a `SLOTNUM` names access type 12, one past the contract's enum; the others name
/// their enum variant.
#[test]
fn test_the_refusal_names_the_access_type() {
    use mega_evm::system::{IMegaAccessControl, VolatileDataAccessType};
    assert_eq!(
        volatile_data_access_disabled_revert_data(VolatileDataAccess::ORACLE),
        Bytes::from(
            IMegaAccessControl::VolatileDataAccessDisabled {
                accessType: VolatileDataAccessType::Oracle
            }
            .abi_encode()
        )
    );
    let slot_num = volatile_data_access_disabled_revert_data(VolatileDataAccess::SLOT_NUM);
    assert_eq!(&slot_num[..4], IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR.as_slice());
    assert_eq!(U256::from_be_slice(&slot_num[4..]), U256::from(12));
}

/// The switch holds for the frame it is off from and every frame below, and a frame above it
/// reads as usual once that frame returned.
#[test]
fn test_the_switch_is_scoped_to_its_subtree() {
    let child = op(BytecodeBuilder::default(), TIMESTAMP).stop().build();
    // The top frame calls the child twice, then reads itself.
    let parent = call(call(BytecodeBuilder::default(), CALL, CHILD), CALL, CHILD);
    let parent = op(parent, NUMBER).stop().build();
    let db = MemoryDatabase::default().account_code(CONTRACT, parent).account_code(CHILD, child);
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(1))
        .with_inspector(Calls::default());
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    assert!(run.outcome.result.is_success());
    let results: Vec<_> = evm.inspector().calls.iter().map(|c| c.result).collect();
    assert_eq!(
        results,
        vec![InstructionResult::Revert, InstructionResult::Stop, InstructionResult::Stop],
        "the first child is refused; the switch was its own, so the second child reads"
    );
    assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP | VolatileDataAccess::BLOCK_NUMBER);

    // Off from the top: the transaction's own frame is refused.
    let code = op(BytecodeBuilder::default(), TIMESTAMP).stop().build();
    let db = MemoryDatabase::default().account_code(CONTRACT, code);
    let mut evm = MegaEvm::new(context(db).with_volatile_access_disabled_from(0));
    let run = run_on(&mut evm, tx(CALLER, CONTRACT, BELOW));
    match &run.outcome.result {
        ExecutionResult::Revert { output, .. } => assert_eq!(
            output,
            &volatile_data_access_disabled_revert_data(VolatileDataAccess::TIMESTAMP)
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(run.outcome.limit_exceeded, None, "a refusal is the frame's revert, not a stop");
}

/* ---------- withheld gas does not leak ---------- */

/// An interceptor's answer carries the forwarded gas back without running a frame: the caller
/// is held to the limit as it resumes, and what it hears from `remainingComputeGas()` is no more
/// than the limit leaves it.
#[test]
fn test_an_interceptors_answer_does_not_lift_the_cap() {
    let calldata = IMegaLimitControl::remainingComputeGasCall {}.abi_encode();
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        let code = op(BytecodeBuilder::default(), TIMESTAMP)
            .mstore(0, &calldata)
            .push_number(32_u8)
            .push_number(0_u8)
            .push_number(4_u8)
            .push_number(0_u8)
            .push_address(LIMIT_CONTROL_ADDRESS)
            .append(GAS)
            .append(STATICCALL)
            .append(POP)
            .push_number(0_u8)
            .append(MLOAD)
            .push_number(0_u8)
            .append(SSTORE);
        let code = spin(code);
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert_stopped(&run, intrinsic);

        // The answer, read without the loop.
        let code = op(BytecodeBuilder::default(), TIMESTAMP)
            .mstore(0, &calldata)
            .push_number(32_u8)
            .push_number(0_u8)
            .push_number(4_u8)
            .push_number(0_u8)
            .push_address(LIMIT_CONTROL_ADDRESS)
            .append(GAS)
            .append(STATICCALL)
            .append(POP)
            .push_number(32_u8)
            .push_number(0_u8)
            .append(RETURN)
            .build();
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let answer = U256::from_be_slice(run.outcome.result.output().unwrap());
        assert!(answer < U256::from(CAP), "the call was forwarded from the capped frame: {answer}");
    }
}

/// A frame's return hands back what detention withheld, on success, revert and halt alike: the
/// transaction spends exactly what it spends without the read, which a child that stays within
/// the cap does not notice.
#[test]
fn test_a_frames_return_hands_back_what_was_withheld() {
    // The child reads (or pushes a word, for the same two gas), computes a little, then ends.
    let ends: [(&str, Append); 3] =
        [("success", |c| c.stop()), ("revert", |c| c.revert()), ("halt", |c| c.append(INVALID))];
    for gas_limit in TIERS {
        for (name, end) in ends {
            let used = |read: u8| {
                let child = end(work(op(BytecodeBuilder::default(), read), 50)).build();
                let parent = call(BytecodeBuilder::default(), CALL, CHILD).stop().build();
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, parent)
                    .account_code(CHILD, child);
                let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
                assert!(run.outcome.result.is_success(), "{name}: {:?}", run.outcome.result);
                (run.outcome.result.gas().tx_gas_used(), run.limit.is_some())
            };
            let (detained, limited) = used(TIMESTAMP);
            let (plain, unlimited) = used(PUSH0);
            assert!(limited && !unlimited);
            assert_eq!(detained, plain, "{name}: nothing withheld stays behind");
        }
    }
}

/// State and history gas are not compute, whether they come out of the reservoir or spill onto
/// regular gas: after the read, a frame writes more state gas than the cap and still completes.
#[test]
fn test_state_and_history_gas_are_not_compute() {
    // 300 fresh slots: 29,376,000 of state gas and 1,056,000 of history, 6,630,000 of compute.
    let mut code = op(BytecodeBuilder::default(), TIMESTAMP);
    for slot in 1..=300_u64 {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    let code = code.stop().build();
    for gas_limit in TIERS {
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code.clone()),
            tx(CALLER, CONTRACT, gas_limit),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert!(run.outcome.gas.state > CAP, "{}", run.outcome.gas.state);
        assert!(run.outcome.gas.regular < CAP);
    }
}

/// State and history gas that spilled onto regular gas before the read are not compute either.
/// Below the execution cap there is no reservoir, so every fresh slot's state gas and its record's
/// history spill; above it the reservoir pays them. The limit is the same: the compute before the
/// read is the writes' regular gas alone.
#[test]
fn test_gas_spilled_before_the_read_is_not_compute() {
    let mut code = BytecodeBuilder::default();
    for slot in 1..=100_u64 {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    let code = spin(op(code, TIMESTAMP));
    for gas_limit in TIERS {
        let run = execute(
            MemoryDatabase::default().account_code(CONTRACT, code.clone()),
            tx(CALLER, CONTRACT, gas_limit),
        );
        let limit = assert_stopped(&run, intrinsic(gas_limit));
        // Two pushes and a fresh slot's 22,100 of regular gas, a hundred times, then TIMESTAMP.
        assert_eq!(limit - CAP, 100 * (3 + 3 + 22_100) + 2, "{gas_limit}");
    }
}

/// A slot restored to its original value refills the state gas it spilled onto regular gas; the
/// spill here predates the read, and the refill must not give the frame compute past the cap.
#[test]
fn test_a_refill_after_the_read_does_not_lift_the_cap() {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
    let code = op(code, TIMESTAMP).sstore(U256::from(1), U256::ZERO);
    let code = spin(code);
    let run = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        tx(CALLER, CONTRACT, BELOW),
    );
    assert_stopped(&run, intrinsic(BELOW));
}
