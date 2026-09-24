//! Gas detention against every other reader of a frame's gas, and against every out-of-gas.
//!
//! A detained frame's spendable gas is held at what the limit leaves the transaction, and the rest
//! is withheld: only a regular charge sees the difference. `GAS`, the 63/64 forward, the `SSTORE`
//! sentry, the skip-cold checks and a callee's inheritance see the whole regular gas. So a
//! transaction that reads volatile data runs as it does without the read — the same result and the
//! same bill on every ledger — unless its compute crosses the limit, which is a regular charge the
//! withheld part would have paid. That is the stop, and nothing else is.
//!
//! Each case runs twice: with a read of the block's timestamp, and with `PUSH0`, which costs the
//! same two gas and reads nothing, in its place.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::{
        keyless::{IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_OVERHEAD_GAS},
        IMegaAccessControl, VolatileDataAccessType,
    },
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    volatile_data_access_disabled_revert_data, EvmTxRuntimeLimits, LimitCheck, LimitKind,
    MegaContext, MegaEvm, MegaHaltReason, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{ExecutionResult, HaltReason, OutOfGasError},
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, Gas, InstructionResult,
        InterpreterResult,
    },
    Database, Inspector,
};

use crate::detention::{
    assert_stopped, burn, context, execute, intrinsic, op, run_on, spin, tx, work, Calls, Run,
    ABOVE, BELOW, BENEFICIARY, CALLER, CAP, CHILD, CONTRACT, TIERS,
};

/// A contract between the transaction's frame and `CHILD`.
const MIDDLE: Address = address!("0000000000000000000000000000000000d00010");

/// An empty account that is not the beneficiary.
const OTHER: Address = address!("0000000000000000000000000000000000d00011");

/// A contract that calls itself with all its gas until the call stack is full.
const RECURSES: Address = address!("0000000000000000000000000000000000d00012");

/// The modexp precompile.
const MODEXP: Address = address!("0000000000000000000000000000000000000005");

/// The BN254 point addition precompile.
const EC_ADD: Address = address!("0000000000000000000000000000000000000006");

/// Runs the transaction `build` makes for a first instruction that reads the timestamp, then for
/// one that pushes a zero at the same price: detained, then not.
fn with_and_without_read(build: impl Fn(u8) -> (MemoryDatabase, u64)) -> (Run, Run) {
    let run = |first| {
        let (db, gas_limit) = build(first);
        execute(db, tx(CALLER, CONTRACT, gas_limit))
    };
    let (detained, plain) = (run(TIMESTAMP), run(PUSH0));
    assert!(detained.limit.is_some(), "the read set a limit");
    assert_eq!(plain.limit, None, "the push read nothing");
    (detained, plain)
}

/// Asserts the detained run is the plain one: its result, every ledger of its bill, and the
/// transaction's own storage.
fn assert_as_without_read(detained: &Run, plain: &Run, case: &str) {
    assert_eq!(detained.outcome.result, plain.outcome.result, "{case}: the result");
    assert_eq!(detained.outcome.gas, plain.outcome.gas, "{case}: the bill");
    assert_eq!(detained.outcome.limit_exceeded, None, "{case}: no stop");
    assert_eq!(slot(detained, 0), slot(plain, 0), "{case}: what the transaction stored");
}

/// What `CONTRACT` holds in slot `index` after the transaction, if it wrote it.
fn slot(run: &Run, index: u64) -> Option<U256> {
    run.outcome
        .state
        .get(&CONTRACT)
        .and_then(|account| account.storage.get(&U256::from(index)))
        .map(|slot| slot.present_value)
}

/// Appends a call to `to` with `gas` (all of it when `None`) that keeps its status on the stack.
fn call_keeping_status(code: BytecodeBuilder, to: Address, gas: Option<u32>) -> BytecodeBuilder {
    let code = code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]).push_address(to);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    code.append(CALL)
}

/// Stores the status a call left on the stack in slot 0, and stops.
fn store_status(code: BytecodeBuilder) -> Bytes {
    code.push_number(0_u8).append(SSTORE).stop().build()
}

/// An operand above `usize`: the instruction fails before any charge, whatever the gas.
fn invalid_operand(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_u256(U256::MAX).append(MLOAD)
}

/// A memory expansion to 16 MiB: about 538,000,000 gas, more than any transaction has.
fn huge_memory(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(0x0100_0000_u32).append(MLOAD)
}

/// A hash of 2^40 bytes, whose word price no transaction can pay: its charge fails before the
/// memory is touched, and the halt zeroes the frame's gas.
fn huge_hash(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(1_u64 << 40).append_many([PUSH0, KECCAK256])
}

/// A copy of 2^40 bytes of calldata, whose word price no transaction can pay.
fn huge_copy(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(1_u64 << 40).append_many([PUSH0, PUSH0, CALLDATACOPY])
}

/// A copy of 2^40 bytes of code, whose word price no transaction can pay.
fn huge_code_copy(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(1_u64 << 40).append_many([PUSH0, PUSH0, CODECOPY])
}

/// A copy of 2^40 bytes within memory, whose word price no transaction can pay.
fn huge_memory_copy(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(1_u64 << 40).append_many([PUSH0, PUSH0, MCOPY])
}

/// A copy of 2^40 bytes of another account's code, whose word price no transaction can pay.
fn huge_account_code_copy(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(1_u64 << 40)
        .append_many([PUSH0, PUSH0])
        .push_address(OTHER)
        .append(EXTCODECOPY)
}

/// A log of 2^40 bytes, whose byte price no transaction can pay.
fn huge_log(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(1_u64 << 40).append_many([PUSH0, LOG0])
}

/* ---------- what the frame's other readers see ---------- */

/// `GAS` answers the whole regular gas after a read, withheld part included, as it does without
/// one.
#[test]
fn test_gas_answers_the_whole_regular_gas_after_a_read() {
    for gas_limit in TIERS {
        let (detained, plain) = with_and_without_read(|first| {
            let code = op(BytecodeBuilder::default(), first)
                .append(GAS)
                .push_number(0_u8)
                .append(SSTORE)
                .stop()
                .build();
            (MemoryDatabase::default().account_code(CONTRACT, code), gas_limit)
        });
        assert_as_without_read(&detained, &plain, "GAS");
        assert!(slot(&detained, 0).unwrap() > U256::from(CAP), "more than the allowance");
    }
}

/// A callee started after the caller's read with an explicit gas stays bounded by it: its state
/// writes draw the reservoir the transaction has, not the caller's withheld gas. Below the
/// execution cap the callee runs out of gas on its third fresh slot, above it the reservoir pays
/// all three — each as without the read.
#[test]
fn test_a_bounded_callee_writes_what_it_writes_without_the_read() {
    let child = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .sstore(U256::from(3), U256::from(1))
        .stop()
        .build();
    for (gas_limit, status) in [(BELOW, 0), (ABOVE, 1)] {
        let (detained, plain) = with_and_without_read(|first| {
            let parent =
                call_keeping_status(op(BytecodeBuilder::default(), first), CHILD, Some(150_000));
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, store_status(parent))
                .account_code(CHILD, child.clone());
            (db, gas_limit)
        });
        assert_as_without_read(&detained, &plain, "a callee bounded to 150,000");
        assert_eq!(slot(&detained, 0), Some(U256::from(status)), "{gas_limit}");
    }
}

/// `SSTORE`'s stipend sentry reads the whole regular gas: a write the frame can pay is not
/// refused because its spendable part is short of 2,300. A fresh slot, the read, 19,998,992 of
/// counting, then a write to the same slot: the compute stays within the limit, and the
/// transaction completes as without the read.
#[test]
fn test_the_sstore_sentry_reads_the_whole_regular_gas() {
    for gas_limit in TIERS {
        let (detained, plain) = with_and_without_read(|first| {
            let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
            let code =
                burn(op(code, first), 769_192).sstore(U256::from(1), U256::from(2)).stop().build();
            (MemoryDatabase::default().account_code(CONTRACT, code), gas_limit)
        });
        assert!(plain.outcome.result.is_success(), "{:?}", plain.outcome.result);
        let limit = detained.limit.unwrap();
        let compute = plain.outcome.gas.regular - intrinsic(gas_limit);
        assert!(compute < limit && limit - compute < 2_300, "{compute} of {limit}");
        assert_as_without_read(&detained, &plain, "the sentry");
    }
}

/* ---------- out-of-gas no gas could pay ---------- */

/// An out-of-gas the transaction's whole gas could not pay halts and burns as without the read:
/// an operand above `usize`, and a memory expansion beyond everything the transaction has.
#[test]
fn test_an_out_of_gas_nothing_could_pay_halts_as_without_the_read() {
    let cases: [(&str, fn(BytecodeBuilder) -> BytecodeBuilder, OutOfGasError); 3] = [
        ("an operand above usize", invalid_operand, OutOfGasError::InvalidOperand),
        ("a 16 MiB memory expansion", huge_memory, OutOfGasError::Memory),
        ("a hash of 2^40 bytes", huge_hash, OutOfGasError::Basic),
    ];
    for gas_limit in TIERS {
        for (name, fail, reason) in cases {
            let (detained, plain) = with_and_without_read(|first| {
                let code = fail(op(BytecodeBuilder::default(), first)).stop().build();
                (MemoryDatabase::default().account_code(CONTRACT, code), gas_limit)
            });
            assert_as_without_read(&detained, &plain, name);
            match &detained.outcome.result {
                ExecutionResult::Halt { reason: MegaHaltReason::Base(halt), gas, .. } => {
                    assert_eq!(*halt, HaltReason::OutOfGas(reason), "{name}");
                    let burned = gas_limit.min(TX_GAS_LIMIT_CAP);
                    assert!(gas.tx_gas_used() >= burned, "{name}: a halt burns the regular gas");
                }
                other => panic!("{name}: expected a halt, got {other:?}"),
            }
        }
    }
}

/// A creation whose code costs more state gas than the transaction has halts as without the read:
/// a failed state charge is never the cap.
#[test]
fn test_a_creation_that_cannot_pay_its_deposit_halts_as_without_the_read() {
    let run = |first| {
        let initcode = op(BytecodeBuilder::default(), first)
            .push_number(70_000_u32)
            .append_many([PUSH0, RETURN])
            .build();
        let create = OpTx(op_transaction(TxEnv {
            caller: CALLER,
            kind: TxKind::Create,
            data: initcode,
            gas_limit: BELOW,
            ..Default::default()
        }));
        run_on(&mut MegaEvm::new(context(MemoryDatabase::default())), create)
    };
    let (detained, plain) = (run(TIMESTAMP), run(PUSH0));
    assert!(detained.limit.is_some());
    assert!(matches!(detained.outcome.result, ExecutionResult::Halt { .. }));
    assert_eq!(detained.outcome.result, plain.outcome.result);
    assert_eq!(detained.outcome.gas, plain.outcome.gas);
    assert_eq!(detained.outcome.limit_exceeded, None);
}

/// A callee that reads and then runs out of gas nothing could pay halts; its caller catches the
/// failure and completes, as without the read.
#[test]
fn test_a_callees_out_of_gas_is_caught_as_without_the_read() {
    let cases: [(&str, fn(BytecodeBuilder) -> BytecodeBuilder); 4] = [
        ("an operand above usize", invalid_operand),
        ("a 16 MiB memory expansion", huge_memory),
        ("a hash of 2^40 bytes", huge_hash),
        ("a copy of 2^40 bytes", huge_copy),
    ];
    let parent = store_status(call_keeping_status(BytecodeBuilder::default(), CHILD, None));
    for gas_limit in TIERS {
        for (name, fail) in cases {
            let (detained, plain) = with_and_without_read(|first| {
                let child = fail(op(BytecodeBuilder::default(), first)).stop().build();
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, parent.clone())
                    .account_code(CHILD, child);
                (db, gas_limit)
            });
            assert_as_without_read(&detained, &plain, name);
            assert_eq!(slot(&detained, 0), Some(U256::ZERO), "{name}: the call failed");
        }
    }
}

/// What a halting callee burns is not compute, however the halt ended it: a caller that read and
/// then computed to within about a hundred thousand of its limit, whose callee burns more than
/// that, completes as without the read.
///
/// The callee's charge fails on an operand, a memory expansion, a hash, four copies, a log and an
/// invalid opcode, all on all the gas; and, on 400,000, on the history of the write records a value
/// call makes, after the call forwarded 63/64 of its gas to a frame that never starts. Above the
/// execution cap the reservoir pays that history, so that case runs below it. A precompile that
/// rejects its input halts without running a frame, and burns all it was forwarded too.
#[test]
fn test_what_a_halting_callee_burns_is_not_compute() {
    let cases: [(&str, Bytes); 10] = [
        ("an operand above usize", invalid_operand(BytecodeBuilder::default()).stop().build()),
        ("a 16 MiB memory expansion", huge_memory(BytecodeBuilder::default()).stop().build()),
        ("a hash of 2^40 bytes", huge_hash(BytecodeBuilder::default()).stop().build()),
        ("a copy of 2^40 bytes", huge_copy(BytecodeBuilder::default()).stop().build()),
        ("a code copy of 2^40 bytes", huge_code_copy(BytecodeBuilder::default()).stop().build()),
        (
            "a memory copy of 2^40 bytes",
            huge_memory_copy(BytecodeBuilder::default()).stop().build(),
        ),
        (
            "an account's code copy of 2^40 bytes",
            huge_account_code_copy(BytecodeBuilder::default()).stop().build(),
        ),
        ("a log of 2^40 bytes", huge_log(BytecodeBuilder::default()).stop().build()),
        ("an invalid opcode", BytecodeBuilder::default().append(INVALID).build()),
        (
            "a value call whose records the caller cannot pay",
            call_keeping_value(BytecodeBuilder::default(), OTHER).stop().build(),
        ),
    ];
    // 6,396 rounds: 19,878,768 of compute before the call.
    let rounds = 6_396;
    for gas_limit in TIERS {
        for (name, callee) in &cases {
            let records = name.starts_with("a value call");
            if records && gas_limit == ABOVE {
                continue;
            }
            let (to, gas) = if records { (MIDDLE, Some(400_000)) } else { (CHILD, None) };
            let (detained, plain) = with_and_without_read(|first| {
                let parent = work(op(BytecodeBuilder::default(), first), rounds);
                let parent = store_status(call_keeping_status(parent, to, gas));
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, parent)
                    .account_code(to, callee.clone())
                    .account_balance(MIDDLE, U256::from(1));
                (db, gas_limit)
            });
            assert_as_without_read(&detained, &plain, name);
            assert_eq!(slot(&detained, 0), Some(U256::ZERO), "{name}: the call failed");
            if records {
                // `MIDDLE` halted on all 400,000; the rest of the regular ledger is compute, and
                // it leaves the limit less than the 63/64 `MIDDLE` forwarded, so counting that
                // forward as compute would have stopped the transaction.
                let compute = plain.outcome.gas.regular - intrinsic(gas_limit) - 400_000;
                let margin = detained.limit.unwrap() - compute;
                assert!(margin < 150_000, "{name}: {margin} short of the limit");
            }
        }

        // `(1, 1)` is not on the curve: the addition fails on it and burns what it was given.
        let (detained, plain) = with_and_without_read(|first| {
            let point = [U256::from(1).to_be_bytes::<32>(), U256::from(1).to_be_bytes::<32>()];
            let parent = work(op(BytecodeBuilder::default(), first), rounds)
                .mstore(0, point.concat())
                .append_many([PUSH0, PUSH0])
                .push_number(128_u8)
                .append_many([PUSH0, PUSH0])
                .push_address(EC_ADD)
                .append_many([GAS, CALL]);
            (MemoryDatabase::default().account_code(CONTRACT, store_status(parent)), gas_limit)
        });
        assert_as_without_read(&detained, &plain, "a precompile rejecting its input");
        assert_eq!(slot(&detained, 0), Some(U256::ZERO), "the addition failed");
    }
}

/// Appends a call carrying one wei to `to` with all the gas, keeping its status.
fn call_keeping_value(code: BytecodeBuilder, to: Address) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(to)
        .append_many([GAS, CALL])
}

/* ---------- crossing the cap ---------- */

/// A callee given all the gas after its caller's read, which computes past the cap, crosses it:
/// the transaction stops at the limit, and the caller never stores the status it would have
/// stored without the read.
#[test]
fn test_a_callee_computing_past_the_cap_stops_the_transaction() {
    let child = work(BytecodeBuilder::default(), 10_000).stop().build();
    for gas_limit in TIERS {
        let (detained, plain) = with_and_without_read(|first| {
            let parent = store_status(call_keeping_status(
                op(BytecodeBuilder::default(), first),
                CHILD,
                None,
            ));
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, parent)
                .account_code(CHILD, child.clone());
            (db, gas_limit)
        });
        assert!(plain.outcome.result.is_success());
        assert_eq!(slot(&plain, 0), Some(U256::from(1)), "the callee completes without the read");
        assert_stopped(&detained, intrinsic(gas_limit));
        assert_eq!(slot(&detained, 0), None, "the caller never resumed");
    }
}

/// The stop reports the limit as what was used, whatever a callee burned before it: a caller that
/// read, called a callee of 5,000,000 that halted, then computed to the stop, is billed its
/// compute up to the limit and what the callee burned, and nothing past either.
#[test]
fn test_the_stop_reports_the_limit_whatever_a_callee_burned() {
    let cases: [(&str, Bytes); 3] = [
        ("out of gas", huge_hash(BytecodeBuilder::default()).stop().build()),
        ("out of memory gas", huge_memory(BytecodeBuilder::default()).stop().build()),
        ("an invalid opcode", BytecodeBuilder::default().append(INVALID).build()),
    ];
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        for (name, child) in &cases {
            let parent = call_keeping_status(
                op(BytecodeBuilder::default(), TIMESTAMP),
                CHILD,
                Some(5_000_000),
            );
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, spin(parent.append(POP)))
                .account_code(CHILD, child.clone());
            let run = execute(db, tx(CALLER, CONTRACT, gas_limit));
            let limit = run.limit.unwrap();
            assert_eq!(
                run.outcome.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: LimitKind::ComputeGas,
                    limit,
                    used: limit,
                    frame_local: false
                }),
                "{name}"
            );
            let burned = run.outcome.gas.regular - intrinsic - limit;
            assert!(
                (4_990_000..=5_000_000).contains(&burned),
                "{name}: the callee burned {burned}"
            );
        }
    }
}

/* ---------- the same bill ---------- */

/// Answers every call to `CHILD` itself, with the gas it was forwarded untouched.
struct AnswersChild;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersChild {
    fn call(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        (inputs.target_address == CHILD).then(|| {
            let gas = Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
            CallOutcome::new(
                InterpreterResult::new(InstructionResult::Stop, Bytes::new(), gas),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

/// A transaction whose callee stays within the cap bills the same on every ledger with the read
/// and without it, whatever the callee: one that succeeds, reverts or halts on all the gas or on a
/// little; a value call running on its stipend; a creation; the depth guard; a small callee
/// writing a fresh slot; and a callee an inspector answers.
#[test]
fn test_a_callee_within_the_cap_bills_the_same_as_without_the_read() {
    let small = |code| work(code, 50);
    type Parent = fn(BytecodeBuilder) -> BytecodeBuilder;
    let cases: Vec<(&str, Parent, Bytes)> = vec![
        (
            "success",
            |c| call_keeping_status(c, CHILD, None),
            small(BytecodeBuilder::default()).stop().build(),
        ),
        (
            "revert",
            |c| call_keeping_status(c, CHILD, None),
            small(BytecodeBuilder::default()).revert().build(),
        ),
        (
            "halt",
            |c| call_keeping_status(c, CHILD, None),
            small(BytecodeBuilder::default()).append(INVALID).build(),
        ),
        (
            "success on a little gas",
            |c| call_keeping_status(c, CHILD, Some(200_000)),
            small(BytecodeBuilder::default()).stop().build(),
        ),
        (
            "halt on a little gas",
            |c| call_keeping_status(c, CHILD, Some(200_000)),
            small(BytecodeBuilder::default()).append(INVALID).build(),
        ),
        (
            "a value call running on its stipend, with a log",
            |c| {
                c.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
                    .push_number(1_u8)
                    .push_address(CHILD)
                    .append_many([PUSH0, CALL])
            },
            BytecodeBuilder::default().append_many([PUSH0, PUSH0, LOG0, STOP]).build(),
        ),
        (
            "a creation",
            |c| {
                // The initcode returns 32 zero bytes: `PUSH1 0x20 PUSH0 RETURN`.
                c.push_number(0x6020_5ff3_u32)
                    .push_number(0_u8)
                    .append(MSTORE)
                    .push_number(4_u8)
                    .push_number(28_u8)
                    .append_many([PUSH0, CREATE])
            },
            BytecodeBuilder::default().stop().build(),
        ),
        (
            "the depth guard",
            |c| call_keeping_status(c, RECURSES, None),
            BytecodeBuilder::default().stop().build(),
        ),
        (
            "a fresh slot on 150,000",
            |c| call_keeping_status(c, CHILD, Some(150_000)),
            BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build(),
        ),
    ];
    let recurses = call_keeping_status(BytecodeBuilder::default(), RECURSES, None).stop().build();
    for gas_limit in TIERS {
        for (name, parent, child) in &cases {
            let (detained, plain) = with_and_without_read(|first| {
                let code = store_status(parent(op(BytecodeBuilder::default(), first)));
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, code)
                    .account_balance(CONTRACT, U256::from(10))
                    .account_code(CHILD, child.clone())
                    .account_code(RECURSES, recurses.clone());
                (db, gas_limit)
            });
            assert_as_without_read(&detained, &plain, name);
        }

        let answered = |first| {
            let code = store_status(call_keeping_status(
                op(BytecodeBuilder::default(), first),
                CHILD,
                Some(200_000),
            ));
            let db = MemoryDatabase::default().account_code(CONTRACT, code);
            let mut evm = MegaEvm::new(context(db)).with_inspector(AnswersChild);
            run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit))
        };
        assert_as_without_read(&answered(TIMESTAMP), &answered(PUSH0), "an inspector's answer");
    }
}

/// A call that reads the beneficiary itself, with a little gas or none, with value or without,
/// costs what the same call to a warm empty account that is not the beneficiary costs, on every
/// ledger, the reservoir included: the read withholds nothing the callee or the caller then lacks.
/// The same holds when an inspector answers the call.
#[test]
fn test_a_call_that_reads_the_beneficiary_costs_what_a_warm_account_costs() {
    /// Answers every call to `BENEFICIARY` or `OTHER` itself, with the gas it was forwarded.
    struct AnswersTargets;
    impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersTargets {
        fn call(
            &mut self,
            _context: &mut MegaContext<DB>,
            inputs: &mut CallInputs,
        ) -> Option<CallOutcome> {
            [BENEFICIARY, OTHER].contains(&inputs.target_address).then(|| {
                let gas =
                    Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
                CallOutcome::new(
                    InterpreterResult::new(InstructionResult::Stop, Bytes::new(), gas),
                    inputs.return_memory_offset.clone(),
                )
            })
        }
    }
    for gas_limit in TIERS {
        for gas in [0_u32, 100_000] {
            for value in [0_u8, 1] {
                for answered in [false, true] {
                    let run = |to: Address| {
                        let code = BytecodeBuilder::default()
                            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
                            .push_number(value)
                            .push_address(to)
                            .push_number(gas)
                            .append_many([CALL, POP, STOP])
                            .build();
                        let db = MemoryDatabase::default()
                            .account_code(CONTRACT, code)
                            .account_balance(CONTRACT, U256::from(10));
                        // The beneficiary is warm; the other account is warmed by the access list,
                        // which both transactions carry.
                        let tx = OpTx(op_transaction(TxEnv {
                            tx_type: TransactionType::Eip2930 as u8,
                            caller: CALLER,
                            kind: TxKind::Call(CONTRACT),
                            gas_limit,
                            access_list: AccessList::from(vec![AccessListItem {
                                address: OTHER,
                                storage_keys: vec![],
                            }]),
                            ..Default::default()
                        }));
                        let mut evm = MegaEvm::new(context(db));
                        if answered {
                            let mut evm = evm.with_inspector(AnswersTargets);
                            run_on(&mut evm, tx)
                        } else {
                            run_on(&mut evm, tx)
                        }
                    };
                    let case = format!("gas {gas}, value {value}, answered {answered}");
                    let (read, plain) = (run(BENEFICIARY), run(OTHER));
                    assert!(read.limit.is_some(), "{case}");
                    assert_eq!(read.accessed, VolatileDataAccess::BENEFICIARY_BALANCE, "{case}");
                    assert_eq!(plain.limit, None, "{case}");
                    assert!(read.outcome.result.is_success(), "{case}: {:?}", read.outcome.result);
                    assert_eq!(read.outcome.gas, plain.outcome.gas, "{case}");
                    assert_eq!(
                        read.outcome.result.gas().tx_gas_used(),
                        plain.outcome.result.gas().tx_gas_used(),
                        "{case}"
                    );
                }
            }
        }
    }
}

/* ---------- answers ---------- */

/// The calldata of a modexp of a 1,024-byte zero base to an exponent of `exponent_len` bytes, all
/// ones, modulo a 1,024-byte modulus of one: its price grows with the exponent, and its result is
/// cheap to compute.
fn modexp_input(exponent_len: usize) -> Vec<u8> {
    let mut input = Vec::new();
    for len in [1_024_usize, exponent_len, 1_024] {
        input.extend_from_slice(&U256::from(len).to_be_bytes::<32>());
    }
    input.extend(core::iter::repeat_n(0_u8, 1_024));
    input.extend(core::iter::repeat_n(0xff_u8, exponent_len));
    let mut modulus = vec![0_u8; 1_024];
    modulus[1_023] = 1;
    input.extend(modulus);
    input
}

/// A contract that copies its calldata into memory and calls modexp with it and `gas` (all of it
/// when `None`), after `first`, storing the call's status.
fn calls_modexp(first: u8, gas: Option<u32>) -> Bytes {
    let code = op(BytecodeBuilder::default(), first)
        .append_many([CALLDATASIZE, PUSH0, PUSH0, CALLDATACOPY])
        .append_many([PUSH0, PUSH0, CALLDATASIZE, PUSH0])
        .push_address(MODEXP);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    store_status(code.append(STATICCALL))
}

/// A transaction from `CALLER` to `CONTRACT` carrying `input`.
fn tx_with(input: &[u8], gas_limit: u64) -> mega_evm::MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit,
        data: input.to_vec().into(),
        ..Default::default()
    }))
}

/// The regular gas modexp charges for `input`, read off the call's own result.
fn modexp_price(input: &[u8]) -> u64 {
    let db = MemoryDatabase::default().account_code(CONTRACT, calls_modexp(PUSH0, None));
    let mut evm = MegaEvm::new(context(db)).with_inspector(Calls::default());
    let run = run_on(&mut evm, tx_with(input, BELOW));
    assert!(run.outcome.result.is_success());
    let call = evm.inspector().calls.iter().find(|call| call.target == MODEXP).unwrap();
    assert_eq!(call.result, InstructionResult::Return);
    call.spent
}

/// revm runs a precompile inside the frame's start, against all the gas the caller forwarded, the
/// part detention withholds from the caller's own charges included. A precompile whose price is
/// within what the limit leaves the transaction runs as without the read; one whose price is above
/// it, though within what the caller forwarded, crosses the cap and stops the transaction at the
/// limit; and one whose price is above what it was forwarded runs out of gas as without the read.
#[test]
fn test_a_precompile_is_held_to_what_the_limit_leaves() {
    let (within, above) = (modexp_input(32), modexp_input(64));
    let (within_price, above_price) = (modexp_price(&within), modexp_price(&above));
    assert!(within_price < CAP / 2, "{within_price}");
    assert!(above_price > CAP + CAP / 10, "{above_price}");
    for gas_limit in TIERS {
        let run = |first, gas, input: &[u8]| {
            let db = MemoryDatabase::default().account_code(CONTRACT, calls_modexp(first, gas));
            run_on(&mut MegaEvm::new(context(db)), tx_with(input, gas_limit))
        };

        let (detained, plain) = (run(TIMESTAMP, None, &within), run(PUSH0, None, &within));
        assert_as_without_read(&detained, &plain, "a price within the allowance");
        assert_eq!(slot(&detained, 0), Some(U256::from(1)));

        let (detained, plain) = (run(TIMESTAMP, None, &above), run(PUSH0, None, &above));
        assert_eq!(slot(&plain, 0), Some(U256::from(1)), "the precompile runs without the read");
        // What a transaction carrying the same input spends before its first instruction.
        let stops = MemoryDatabase::default()
            .account_code(CONTRACT, BytecodeBuilder::default().stop().build());
        let intrinsic = execute(stops, tx_with(&above, gas_limit)).outcome.gas.regular;
        let limit = assert_stopped(&detained, intrinsic);
        assert!(limit < above_price, "the stop billed the allowance, not the price");

        // Forwarded less than its price: a precompile's out-of-gas, the call's failure.
        let short = Some(u32::try_from(within_price).unwrap() - 1);
        let (detained, plain) = (run(TIMESTAMP, short, &within), run(PUSH0, short, &within));
        assert_as_without_read(&detained, &plain, "a price above the forward");
        assert_eq!(slot(&detained, 0), Some(U256::ZERO));
    }
}

/// An interceptor that charges its frame by taking gas off the frame's limit and then answers is
/// held to the allowance as a precompile is: a `keylessDeploy` call carrying value, from the
/// beneficiary under a cap below the keyless overhead, stops at the cap instead of answering. The
/// stop's gas is the frame's as its caller forwarded it, so a tracer reading the answer sees the
/// allowance spent and the rest left.
#[test]
fn test_an_interceptors_charge_is_held_to_what_the_limit_leaves() {
    let calldata: Bytes = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(b"a transaction"),
        gasLimitOverride: U256::ZERO,
    }
    .abi_encode()
    .into();
    let cap = KEYLESS_DEPLOY_OVERHEAD_GAS / 2;
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
    let run = |caller: Address| {
        let db = MemoryDatabase::default().account_balance(caller, U256::from(10));
        let tx = OpTx(op_transaction(TxEnv {
            caller,
            kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
            gas_limit: BELOW,
            value: U256::from(1),
            data: calldata.clone(),
            ..Default::default()
        }));
        let evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let mut evm = evm.with_inspector(Calls::default());
        let run = run_on(&mut evm, tx);
        (run, evm.inspector().calls[0].spent)
    };
    let ((detained, stop_spent), (plain, answer_spent)) = (run(BENEFICIARY), run(CALLER));
    assert_eq!(answer_spent, 0, "the answer spent nothing of the limit it was left");
    assert_eq!(stop_spent, cap, "the stop spent the allowance of the limit it was forwarded");
    assert_eq!(
        plain.outcome.result.output(),
        Some(&Bytes::from(IKeylessDeploy::NoEtherTransfer::SELECTOR.to_vec())),
        "the answer without detention"
    );
    assert_eq!(detained.limit, Some(cap), "the sender is the beneficiary");
    let intrinsic = plain.outcome.gas.regular - KEYLESS_DEPLOY_OVERHEAD_GAS;
    assert_stopped(&detained, intrinsic);
}

/* ---------- the refusal's access type ---------- */

/// A `SLOTNUM` refusal carries access type 12, which `IMegaAccessControl`'s enum does not declare:
/// a decoder that validates the enum rejects it, while every other refusal decodes, `BLOCKHASH`'s
/// as `BlockHash`. The enum is the access-control contract's to extend.
#[test]
fn test_a_slotnum_refusal_is_past_the_enum() {
    let slot_num = volatile_data_access_disabled_revert_data(VolatileDataAccess::SLOT_NUM);
    assert!(IMegaAccessControl::VolatileDataAccessDisabled::abi_decode_validate(&slot_num).is_err());
    let block_hash = volatile_data_access_disabled_revert_data(VolatileDataAccess::BLOCK_HASH);
    assert_eq!(
        IMegaAccessControl::VolatileDataAccessDisabled::abi_decode_validate(&block_hash)
            .unwrap()
            .accessType,
        VolatileDataAccessType::BlockHash
    );
}
