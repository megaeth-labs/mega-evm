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

use std::sync::{Arc, Mutex};

use alloy_evm::precompiles::DynPrecompile;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    satin_gas_params,
    system::{
        keyless::{
            decode_keyless_tx,
            tests::{CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX},
            IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
            KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
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
    precompile::{modexp, PrecompileHalt, PrecompileId, PrecompileOutput},
    primitives::HashMap,
    Database, Inspector,
};

use crate::detention::{
    assert_stopped, burn, context, execute, intrinsic, memory_cost, op, run_on, spin, stop_data,
    tx, work, Calls, Charges, Run, ABOVE, BELOW, BENEFICIARY, CALLER, CAP, CHILD, CONTRACT, TIERS,
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

/// The BN254 pairing precompile.
const EC_PAIRING: Address = address!("0000000000000000000000000000000000000008");

/// The BLS12-381 G1 MSM precompile (EIP-2537).
const BLS12_G1_MSM: Address = address!("000000000000000000000000000000000000000c");

/// The BLS12-381 G2 MSM precompile (EIP-2537).
const BLS12_G2_MSM: Address = address!("000000000000000000000000000000000000000e");

/// The BLS12-381 pairing precompile (EIP-2537).
const BLS12_PAIRING: Address = address!("000000000000000000000000000000000000000f");

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

/// A piece of code that ends a frame on an out-of-gas, appended to a program.
type Fail = fn(BytecodeBuilder) -> BytecodeBuilder;

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
    let cases: [(&str, Fail, OutOfGasError); 3] = [
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
    let cases: [(&str, Fail); 4] = [
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

/// Runs the transaction `build` makes, under a block-environment cap of `cap`, for a first
/// instruction that reads the timestamp, then for one that pushes a zero at the same price.
fn with_and_without_read_under(
    cap: u64,
    build: impl Fn(u8) -> (MemoryDatabase, u64),
) -> (Run, Run) {
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
    let run = |first| {
        let (db, gas_limit) = build(first);
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit))
    };
    (run(TIMESTAMP), run(PUSH0))
}

/// Appends `rounds` calls to `CHILD`, each forwarding `gas`, dropping their status.
fn calls_to_child(code: BytecodeBuilder, rounds: u32, gas: u32) -> BytecodeBuilder {
    (0..rounds).fold(code, |code, _| {
        code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
            .push_address(CHILD)
            .push_number(gas)
            .append_many([CALL, POP])
    })
}

/// `EXP`'s exponent charge — fifty gas a byte of the exponent, 1,600 for a full word — is noted
/// when it fails: a hundred children forwarded 1,500 gas each, which halt on an `EXP` of a
/// 32-byte exponent with 1,484 left, burn that, and the caller completes under a cap of 100,000,
/// as without the read. Counted as compute, the hundred burns would pass the cap.
#[test]
fn test_an_exp_that_cannot_pay_its_exponent_burns_what_its_frame_had() {
    let child =
        BytecodeBuilder::default().push_u256(U256::MAX).push_number(2_u8).append(EXP).build();
    for gas_limit in TIERS {
        let (detained, plain) = with_and_without_read_under(100_000, |first| {
            let code = calls_to_child(op(BytecodeBuilder::default(), first), 100, 1_500);
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code.stop().build())
                .account_code(CHILD, child.clone());
            (db, gas_limit)
        });
        assert_as_without_read(&detained, &plain, "a hundred halts on EXP");
        let burned = 100 * 1_484;
        assert!(plain.outcome.gas.regular - intrinsic(gas_limit) > burned, "the halts burned");
    }
}

/// A charge the interpreter's step loop makes before the opcode runs — its static gas — is seen by
/// no wrapper, so what a frame had when it fails counts as compute: burned gas counted as compute,
/// which brings the stop earlier, never later, and by less than the failed charge's price per
/// halting frame.
///
/// Fifty children forwarded 5,002 gas each run a `PUSH20` and a `SELFDESTRUCT`, whose static
/// charge of 5,000 fails with 4,999 left. Under a cap of 200,000 the caller stops, though about
/// 8,500 gas of instructions ran after its read. The child that took the compute past the limit
/// did so without a charge crossing it, so the stop is its caller's next charge, and the stop's
/// compute, which the regular ledger bills, is the limit and that child's overshoot, under the
/// 5,000 it failed to pay. Under a cap of 300,000, above fifty times the forward and the caller's
/// own work, the caller completes as without the read.
#[test]
fn test_a_halt_on_a_static_charge_counts_what_its_frame_had_as_compute() {
    let child = BytecodeBuilder::default().push_address(CALLER).append(SELFDESTRUCT).build();
    for gas_limit in TIERS {
        let build = |first| {
            let code = calls_to_child(op(BytecodeBuilder::default(), first), 50, 5_002);
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, code.stop().build())
                .account_code(CHILD, child.clone());
            (db, gas_limit)
        };
        let (detained, plain) = with_and_without_read_under(200_000, build);
        let limit = detained.limit.unwrap();
        assert_eq!(limit, 2 + 200_000);
        assert_eq!(detained.outcome.result.output(), Some(&stop_data(limit)));
        // The compute after the read: its `POP`, then each call — five pushes, the address, the
        // forward's push, the access, cold the first time — and the child's whole forward, which
        // it computes with or burns counted as compute, and the caller's `POP`. The caller's first
        // charge the limit leaves no room for is the stop.
        let mut since_read = 2;
        let crossing = 'calls: {
            for call in 0..50 {
                let access = if call == 0 { 2_600 } else { 100 };
                for charge in [2, 2, 2, 2, 2, 3, 3, access] {
                    if since_read + charge > 200_000 {
                        break 'calls since_read;
                    }
                    since_read += charge;
                }
                since_read += 5_002;
                if since_read + 2 > 200_000 {
                    break 'calls since_read;
                }
                since_read += 2;
            }
            panic!("the calls fit the cap");
        };
        assert!(crossing > 200_000 && crossing - 200_000 < 5_000, "an overshoot: {crossing}");
        let used = 2 + crossing;
        assert_eq!(
            detained.outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                limit,
                used,
                frame_local: false
            })
        );
        assert_eq!(detained.outcome.gas.regular, intrinsic(gas_limit) + used);
        assert!(plain.outcome.result.is_success());
        let (detained, plain) = with_and_without_read_under(300_000, build);
        assert_as_without_read(&detained, &plain, "fifty halts on a static charge");
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
        // The read's `POP`, the call's five pushes, its address, `GAS` and cold access, then the
        // callee's work, in its own memory.
        let left =
            Charges::default().then(&[2, 2, 2, 2, 2, 2, 3, 2, 2_600]).work(10_000, 0).left(CAP);
        assert_stopped(&detained, intrinsic(gas_limit), left);
        assert_eq!(slot(&detained, 0), None, "the caller never resumed");
    }
}

/// The stop reports the transaction's compute as what was used, whatever a callee burned before
/// it: a caller that read, called a callee of 5,000,000 that halted, then computed to the stop, is
/// billed its compute up to the charge that crossed the limit and what the callee burned, and
/// nothing past either.
#[test]
fn test_the_stop_reports_its_compute_whatever_a_callee_burned() {
    // Each callee, and what it computes before it halts: the pushes and the static gas of the
    // instruction whose dynamic charge fails, or nothing before an invalid opcode.
    let cases: [(&str, Bytes, u64); 3] = [
        ("out of gas", huge_hash(BytecodeBuilder::default()).stop().build(), 3 + 2 + 30),
        ("out of memory gas", huge_memory(BytecodeBuilder::default()).stop().build(), 3 + 3),
        ("an invalid opcode", BytecodeBuilder::default().append(INVALID).build(), 0),
    ];
    for gas_limit in TIERS {
        let intrinsic = intrinsic(gas_limit);
        for (name, child, computed) in &cases {
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
            // The read's `POP`, the call's five pushes, its address, the forward's push and cold
            // access, what the callee computed, the caller's `POP`, then its loop.
            let left = Charges::default()
                .then(&[2, 2, 2, 2, 2, 2, 3, 3, 2_600, *computed, 2])
                .spin(0)
                .left(CAP);
            let used = limit - left;
            assert_eq!(
                run.outcome.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: LimitKind::ComputeGas,
                    limit,
                    used,
                    frame_local: false
                }),
                "{name}"
            );
            let burned = run.outcome.gas.regular - intrinsic - used;
            assert!(
                (4_990_000..=5_000_000).contains(&burned),
                "{name}: the callee burned {burned}"
            );
        }
    }
}

/// What a halting callee burns is what it had when it halted, not what it had at an opcode before
/// it: a callee that hashes, then works through 260,000 gas, then hits an invalid opcode burns
/// what it had at the invalid opcode. The transaction its caller stops after it bills that burn
/// beside its compute, to the gas, so the work between the hash and the halt is compute.
#[test]
fn test_a_halt_burns_what_its_frame_had_when_it_halted() {
    let hashes = BytecodeBuilder::default().push_number(32_u8).append_many([PUSH0, KECCAK256, POP]);
    let child = burn(hashes, 10_000).append(INVALID).build();
    for gas_limit in TIERS {
        let parent =
            call_keeping_status(op(BytecodeBuilder::default(), TIMESTAMP), CHILD, Some(5_000_000));
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, spin(parent.append(POP)))
            .account_code(CHILD, child.clone());
        let mut evm = MegaEvm::new(context(db)).with_inspector(Calls::default());
        let run = run_on(&mut evm, tx(CALLER, CONTRACT, gas_limit));
        let call = evm.inspector().calls.iter().find(|call| call.target == CHILD).unwrap();
        assert_eq!(call.result, InstructionResult::InvalidFEOpcode);
        assert!(call.spent > 260_000, "{}", call.spent);
        let limit = run.limit.unwrap();
        assert_eq!(run.outcome.result.output(), Some(&stop_data(limit)));
        // The read's `POP`, the call's five pushes, its address, the forward's push and cold
        // access; the callee's push, `PUSH0`, hash of a word with the memory it expands, `POP` and
        // work; then the caller's `POP` and loop.
        let callee = Charges::default().then(&[3, 2, 30 + 6 + 3, 2]).burn(10_000).total();
        assert_eq!(call.spent, callee, "the callee spent its charges before the invalid opcode");
        let left =
            Charges::default().then(&[2, 2, 2, 2, 2, 2, 3, 3, 2_600, callee, 2]).spin(0).left(CAP);
        let burned = run.outcome.gas.regular - intrinsic(gas_limit) - (limit - left);
        assert_eq!(burned, 5_000_000 - call.spent, "the callee burned what it had at the halt");
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

/// A precompile that charges the price its input names, as one big-endian word.
pub(crate) const PRICED: Address = address!("0000000000000000000000000000000000d00013");

/// The gas limits a precompile was run on, in order, each with whether its price was within it:
/// a precompile that cannot pay its price answers out of gas without computing anything.
pub(crate) type Runs = Arc<Mutex<Vec<(u64, bool)>>>;

/// The precompile at [`PRICED`], recording what it runs on into `runs`.
pub(crate) fn priced(runs: &Runs) -> DynPrecompile {
    let runs = Arc::clone(runs);
    DynPrecompile::new(PrecompileId::Custom("priced".into()), move |input| {
        let price = U256::from_be_slice(input.data).saturating_to::<u64>();
        let within = price <= input.gas;
        runs.lock().unwrap().push((input.gas, within));
        Ok(if within {
            PrecompileOutput::new(price, Bytes::new(), input.reservoir)
        } else {
            PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir)
        })
    })
}

/// Modexp as the Satin set runs it, recording what it runs on into `runs`.
fn recording_modexp(runs: &Runs) -> DynPrecompile {
    let runs = Arc::clone(runs);
    DynPrecompile::new(PrecompileId::ModExp, move |input| {
        let result = modexp::osaka_run(input.data, input.gas);
        runs.lock().unwrap().push((input.gas, result.is_ok()));
        Ok(PrecompileOutput::from_eth_result(result, input.reservoir))
    })
}

/// The calldata of a modexp that is costly to compute: a 1,024-byte base with no zero byte, an
/// exponent of `exponent_len` bytes, all ones, and an odd 1,024-byte modulus, then `padding`
/// zero bytes the precompile ignores. Its price grows with the exponent, and so does the work.
pub(crate) fn costly_modexp_input(exponent_len: usize, padding: usize) -> Vec<u8> {
    let mut input = Vec::new();
    for len in [1_024_usize, exponent_len, 1_024] {
        input.extend_from_slice(&U256::from(len).to_be_bytes::<32>());
    }
    input.extend((0..1_024_u32).map(|i| (i % 251) as u8 + 1));
    input.extend(core::iter::repeat_n(0xff_u8, exponent_len));
    let mut modulus: Vec<u8> = (0..1_024_u32).map(|i| (i % 241) as u8).collect();
    modulus[0] = 0xff;
    modulus[1_023] |= 1;
    input.extend(modulus);
    input.extend(core::iter::repeat_n(0_u8, padding));
    input
}

/// A contract that copies its calldata into memory and calls `to` with it and `gas` (all of it
/// when `None`), after `first`, storing the call's status.
fn calls_precompile(first: u8, to: Address, gas: Option<u32>) -> Bytes {
    let code = op(BytecodeBuilder::default(), first)
        .append_many([CALLDATASIZE, PUSH0, PUSH0, CALLDATACOPY])
        .append_many([PUSH0, PUSH0, CALLDATASIZE, PUSH0])
        .push_address(to);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    store_status(code.append(STATICCALL))
}

/// The regular charges [`calls_precompile`] makes after its first instruction reads, with
/// calldata of `len` bytes, up to its call: the read's `POP`; `CALLDATASIZE`, two `PUSH0` and
/// `CALLDATACOPY` — its static gas, the copy and the memory it expands; the call's four pushes
/// and its address; the push of `gas`, or `GAS`; and the precompile's warm access.
fn charges_to_the_precompile(len: usize, gas: Option<u32>) -> Charges {
    let words = (len as u64).div_ceil(32);
    let forward = if gas.is_some() { 3 } else { 2 };
    Charges::default().then(&[
        2,
        2,
        2,
        2,
        3,
        3 * words,
        memory_cost(words),
        2,
        2,
        2,
        2,
        3,
        forward,
        100,
    ])
}

/// A transaction from `caller` to `to` carrying `input`.
fn tx_to(caller: Address, to: Address, input: &[u8], gas_limit: u64) -> mega_evm::MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller,
        kind: TxKind::Call(to),
        gas_limit,
        data: input.to_vec().into(),
        ..Default::default()
    }))
}

/// A transaction from `CALLER` to `CONTRACT` carrying `input`.
fn tx_with(input: &[u8], gas_limit: u64) -> mega_evm::MegaTransaction {
    tx_to(CALLER, CONTRACT, input, gas_limit)
}

/// What a transaction carrying `input` spends before its first instruction.
pub(crate) fn intrinsic_with(input: &[u8], gas_limit: u64) -> u64 {
    let stops =
        MemoryDatabase::default().account_code(CONTRACT, BytecodeBuilder::default().stop().build());
    execute(stops, tx_with(input, gas_limit)).outcome.gas.regular
}

/// A precompile's run in a transaction: what the transaction did, what the precompile ran on,
/// and the gas its call spent as the caller got it back.
struct PrecompileRun {
    run: Run,
    ran_on: Vec<(u64, bool)>,
    spent: u64,
}

/// Runs `tx` against `db` with the precompile `make` builds installed at `at`.
fn run_precompile(
    db: MemoryDatabase,
    at: Address,
    make: fn(&Runs) -> DynPrecompile,
    tx: mega_evm::MegaTransaction,
) -> PrecompileRun {
    let runs = Runs::default();
    let evm =
        MegaEvm::new(context(db)).with_dyn_precompiles(HashMap::from_iter([(at, make(&runs))]));
    let mut evm = evm.with_inspector(Calls::default());
    let run = run_on(&mut evm, tx);
    let spent = evm.inspector().calls.iter().find(|call| call.target == at).unwrap().spent;
    let ran_on = runs.lock().unwrap().clone();
    PrecompileRun { run, ran_on, spent }
}

/// revm runs a precompile inside the frame's start, against the gas limit of the frame, so after
/// a read a precompile the engine cannot price — here a node's own — is run on the allowance the
/// frame would have — what the limit leaves the transaction — and never on what its caller
/// forwarded past it. Without the read, it runs on the forward.
///
/// Priced within the allowance, it answers as without the read. Priced at the allowance exactly,
/// it answers, and its caller's next charge — the push of its status's slot — crosses the limit.
/// Priced a unit past it, it answers out of gas without computing, and the transaction stops,
/// billed nothing of the allowance the precompile ran on.
#[test]
fn test_a_precompile_runs_on_the_allowance() {
    for gas_limit in TIERS {
        let run = |first, price: u64| {
            let code = calls_precompile(first, PRICED, None);
            let db = MemoryDatabase::default().account_code(CONTRACT, code);
            let input = U256::from(price).to_be_bytes::<32>();
            run_precompile(db, PRICED, priced, tx_with(&input, gas_limit))
        };
        // The price is the charge after the call, then the push of the status's slot.
        let to_the_call = charges_to_the_precompile(32, None);
        let allowance = CAP - to_the_call.total();
        let stopped = |run: &PrecompileRun, price: u64| {
            let input = U256::from(price).to_be_bytes::<32>();
            let left = to_the_call.clone().then(&[price, 3]).left(CAP);
            assert_stopped(&run.run, intrinsic_with(&input, gas_limit), left);
        };

        // Priced past every allowance: the stop.
        let past = run(TIMESTAMP, CAP + 1);
        stopped(&past, CAP + 1);
        assert_eq!(past.ran_on, [(allowance, false)], "run on the allowance, computing nothing");
        assert_eq!(past.spent, 0, "the stop gives the precompile its whole forward back");

        for price in [allowance - 100_000, allowance, allowance + 1] {
            let (detained, plain) = (run(TIMESTAMP, price), run(PUSH0, price));
            assert_eq!(detained.ran_on, [(allowance, price <= allowance)], "priced {price}");
            assert!(matches!(plain.ran_on[..], [(forward, true)] if forward > CAP), "{price}");
            assert_eq!(slot(&plain.run, 0), Some(U256::from(1)));
            if price < allowance {
                assert_as_without_read(&detained.run, &plain.run, "a price the allowance pays");
            } else {
                stopped(&detained, price);
            }
        }
    }
}

/// Modexp on a costly input, replaced by a node's recording copy of it, which the engine does not
/// price, after a read, against the same call without it:
///
/// - priced within the allowance, it computes on the allowance and answers as without the read;
/// - priced past the allowance but within what its caller forwarded, it runs out of the allowance
///   without computing, and the transaction stops at the limit — without the read it computes;
/// - forwarded less than its price, within the allowance, it runs out of gas as without the read;
/// - forwarded less than its price, past the allowance, it runs out of the allowance too, and the
///   transaction stops at the limit. Without the read the call fails and its caller goes on: with
///   no price, it cannot be told from one priced within the forward.
///
/// The cheap input is padded to the length of the costly one, so the two calls leave the same
/// allowance, which the stop's bill reads out.
#[test]
fn test_a_precompile_is_held_to_what_the_limit_leaves() {
    let (within, above) = (costly_modexp_input(32, 32), costly_modexp_input(64, 0));
    assert_eq!(within.len(), above.len());
    for gas_limit in TIERS {
        let run = |first, gas, input: &[u8]| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, calls_precompile(first, MODEXP, gas));
            run_precompile(db, MODEXP, recording_modexp, tx_with(input, gas_limit))
        };

        let (detained, plain) = (run(TIMESTAMP, None, &above), run(PUSH0, None, &above));
        let allowance = CAP - charges_to_the_precompile(above.len(), None).total();
        assert_stopped(&detained.run, intrinsic_with(&above, gas_limit), allowance);
        assert_eq!(detained.ran_on, [(allowance, false)], "past the allowance, nothing computed");
        let above_price = plain.spent;
        assert!(allowance < CAP && CAP < above_price, "{allowance} {above_price}");
        assert!(plain.ran_on[0].1, "without the read it computes");
        assert_eq!(slot(&plain.run, 0), Some(U256::from(1)));

        let (detained, plain) = (run(TIMESTAMP, None, &within), run(PUSH0, None, &within));
        assert_as_without_read(&detained.run, &plain.run, "a price within the allowance");
        assert_eq!(detained.ran_on, [(allowance, true)], "run on the allowance");
        assert_eq!(slot(&detained.run, 0), Some(U256::from(1)));
        let within_price = plain.spent;
        assert!(within_price < allowance, "{within_price}");

        // Forwarded less than its price, within the allowance: the call's own out-of-gas.
        let short = u32::try_from(within_price).unwrap() - 1;
        let (detained, plain) =
            (run(TIMESTAMP, Some(short), &within), run(PUSH0, Some(short), &within));
        assert_as_without_read(
            &detained.run,
            &plain.run,
            "a price above a forward the allowance holds",
        );
        assert_eq!(detained.ran_on, [(u64::from(short), false)]);
        assert_eq!(slot(&detained.run, 0), Some(U256::ZERO));

        // Forwarded less than its price, past the allowance: the stop, where the call would fail.
        let short = u32::try_from(above_price).unwrap() - 1;
        let (detained, plain) =
            (run(TIMESTAMP, Some(short), &above), run(PUSH0, Some(short), &above));
        let allowance = CAP - charges_to_the_precompile(above.len(), Some(short)).total();
        assert_stopped(&detained.run, intrinsic_with(&above, gas_limit), allowance);
        assert_eq!(detained.ran_on, [(allowance, false)], "run on the allowance");
        assert_eq!(plain.ran_on, [(u64::from(short), false)]);
        assert!(plain.run.outcome.result.is_success());
        assert_eq!(slot(&plain.run, 0), Some(U256::ZERO), "without the read the call fails");
    }
}

/// A modexp priced far above the cap, 58,687,488 and 125,796,352 gas, called after a read with all
/// the gas.
///
/// The built-in set the chain runs prices it before it runs: priced within what its caller
/// forwarded, it needs gas the limit withholds, and is the stop, answered without running; priced
/// past the forward, it runs out of gas as it does without the read, and its caller goes on. Below
/// the execution cap the forward is under 100,000,000, so the two prices fall on either side of
/// it; above the cap both are within it.
///
/// A modexp a node replaced is not priced: it is run on the allowance, computes nothing, and is
/// the stop, whatever its price. A transaction the beneficiary sends straight to it is detained
/// from the start, so it runs on the cap itself. The same transaction from another sender
/// computes.
#[test]
fn test_a_precompile_priced_past_the_cap_computes_nothing() {
    let mut cases = (0, 0);
    for (exponent_len, price) in [(128, 58_687_488), (256, 125_796_352)] {
        let input = costly_modexp_input(exponent_len, 0);
        let builtin = mega_evm::satin_precompiles().get(&MODEXP).unwrap();
        assert_eq!(builtin.required_gas(&input), Some(price), "the built-in's price");
        // Without the read, it computes, and costs its price on top of what the transaction spends
        // before its first instruction.
        let plain = run_precompile(
            MemoryDatabase::default(),
            MODEXP,
            recording_modexp,
            tx_to(CALLER, MODEXP, &input, ABOVE),
        );
        assert!(plain.run.outcome.result.is_success());
        assert!(matches!(plain.ran_on[..], [(_, true)]), "{:?}", plain.ran_on);
        assert_eq!(plain.spent, price);
        let intrinsic = plain.run.outcome.gas.regular - price;

        for gas_limit in TIERS {
            let db = |first| {
                MemoryDatabase::default()
                    .account_code(CONTRACT, calls_precompile(first, MODEXP, None))
            };
            let called =
                run_precompile(db(TIMESTAMP), MODEXP, recording_modexp, tx_with(&input, gas_limit));
            let allowance = CAP - charges_to_the_precompile(input.len(), None).total();
            assert_stopped(&called.run, intrinsic_with(&input, gas_limit), allowance);
            assert_eq!(called.ran_on, [(allowance, false)], "run on the allowance");

            let built_in = |first| {
                let mut evm = MegaEvm::new(context(db(first))).with_inspector(Calls::default());
                let run = run_on(&mut evm, tx_with(&input, gas_limit));
                let call = evm.inspector().calls.iter().find(|call| call.target == MODEXP).unwrap();
                let (forward, ran) = (call.forward, call.precompile_ran);
                (run, forward, ran)
            };
            let (detained, forward, ran) = built_in(TIMESTAMP);
            let (plain, plain_forward, plain_ran) = built_in(PUSH0);
            assert_eq!(forward, plain_forward, "the read leaves the forward as it is");
            assert!(forward > CAP, "{forward}");
            if price <= forward {
                assert_stopped(&detained, intrinsic_with(&input, gas_limit), allowance);
                assert!(!ran, "answered without running");
                assert!(plain_ran && plain.outcome.result.is_success());
                assert_eq!(slot(&plain, 0), Some(U256::from(1)), "without the read it computes");
                cases.0 += 1;
            } else {
                assert_as_without_read(&detained, &plain, "a price past the forward");
                assert!(ran && plain_ran, "run on the forward, out of gas");
                assert_eq!(slot(&detained, 0), Some(U256::ZERO), "the call fails");
                cases.1 += 1;
            }

            let tx = tx_to(BENEFICIARY, MODEXP, &input, gas_limit);
            let sent = run_precompile(MemoryDatabase::default(), MODEXP, recording_modexp, tx);
            assert_eq!(sent.run.limit, Some(CAP), "the sender is the beneficiary");
            assert_eq!(sent.ran_on, [(CAP, false)], "run on the cap, computing nothing");
            assert_eq!(sent.spent, 0, "the stop gives the precompile its whole forward back");
            assert_stopped(&sent.run, intrinsic, CAP);
        }
    }
    assert_eq!(cases, (3, 1), "the stop above the cap for both, and below it for the cheaper");
}

/// The built-in modexp priced within the allowance runs as without the read: revm runs it, on the
/// whole forward, and it charges its price, the same result and the same bill.
#[test]
fn test_a_priced_precompile_within_the_allowance_runs_as_without_the_read() {
    let input = costly_modexp_input(32, 32);
    let price = mega_evm::satin_precompiles().get(&MODEXP).unwrap().required_gas(&input).unwrap();
    assert!(price < CAP / 2, "{price}");
    for gas_limit in TIERS {
        let run = |first| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, calls_precompile(first, MODEXP, None));
            let mut evm = MegaEvm::new(context(db)).with_inspector(Calls::default());
            let run = run_on(&mut evm, tx_with(&input, gas_limit));
            let call = evm.inspector().calls.iter().find(|call| call.target == MODEXP).unwrap();
            assert!(call.precompile_ran, "revm runs it");
            assert_eq!(call.spent, price, "it charges its price");
            run
        };
        let (detained, plain) = (run(TIMESTAMP), run(PUSH0));
        assert_as_without_read(&detained, &plain, "a price the allowance pays");
        assert_eq!(slot(&detained, 0), Some(U256::from(1)));
    }
}

/// A Satin address a node replaced is not priced from the built-in set, even when its precompile
/// carries the built-in's id: forwarded less than its price and more than the allowance, the
/// replaced modexp runs on the allowance and is the stop, where the built-in one, which the engine
/// prices, runs out of gas as without the read, and its caller goes on.
#[test]
fn test_a_replaced_precompile_is_not_priced_from_the_built_in_set() {
    let input = costly_modexp_input(64, 0);
    let price = mega_evm::satin_precompiles().get(&MODEXP).unwrap().required_gas(&input).unwrap();
    assert!(price > CAP, "{price}");
    let short = u32::try_from(price - 1).unwrap();
    for gas_limit in TIERS {
        let db = |first| {
            MemoryDatabase::default()
                .account_code(CONTRACT, calls_precompile(first, MODEXP, Some(short)))
        };
        let replaced =
            run_precompile(db(TIMESTAMP), MODEXP, recording_modexp, tx_with(&input, gas_limit));
        let allowance = CAP - charges_to_the_precompile(input.len(), Some(short)).total();
        assert_stopped(&replaced.run, intrinsic_with(&input, gas_limit), allowance);
        assert_eq!(replaced.ran_on, [(allowance, false)], "run on the allowance");

        let (built_in, plain) = (
            execute(db(TIMESTAMP), tx_with(&input, gas_limit)),
            execute(db(PUSH0), tx_with(&input, gas_limit)),
        );
        assert_as_without_read(&built_in, &plain, "a price past the forward");
        assert_eq!(slot(&built_in, 0), Some(U256::ZERO), "the call fails and its caller goes on");
    }
}

/// op-revm's wrapper of the BN254 pairing is priced, so a call forwarded less than its price and
/// more than the allowance — three pairs, 147,000 gas, forwarded 120,000 under a cap of 100,000 —
/// runs on its forward and runs out of gas as without the read, and its caller goes on, as the KZG
/// entry does — 100,000, forwarded 80,000 under a cap of 50,000. A node's own precompile, which the
/// engine cannot price, keeps the clamp: at the pairing's price and forward, it runs on the
/// allowance and is the stop.
#[test]
fn test_a_nodes_own_precompile_keeps_the_clamp_a_priced_wrapper_does_not() {
    let pairing = vec![0_u8; 3 * 192];
    // EIP-1108: 45,000, and 34,000 a pair.
    let price = 45_000 + 3 * 34_000;
    let entry = mega_evm::satin_precompiles().get(&EC_PAIRING).unwrap();
    assert_eq!(entry.required_gas(&pairing), Some(price));
    let kzg = vec![0_u8; 192];
    let kzg_address = mega_evm::kzg_point_evaluation::ADDRESS;
    let own_input = U256::from(price).to_be_bytes::<32>();
    let limits = |cap| EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
    for gas_limit in TIERS {
        let run = |first, to, forward, cap, input: &[u8]| {
            let db = MemoryDatabase::default()
                .account_code(CONTRACT, calls_precompile(first, to, Some(forward)));
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits(cap)))
                .with_inspector(Calls::default());
            let run = run_on(&mut evm, tx_with(input, gas_limit));
            let call = evm.inspector().calls.iter().find(|call| call.target == to).unwrap();
            assert_eq!(call.forward, u64::from(forward));
            assert!(call.precompile_ran, "{to}: revm runs it, on its forward");
            run
        };

        let detained = run(TIMESTAMP, EC_PAIRING, 120_000, 100_000, &pairing);
        let plain = run(PUSH0, EC_PAIRING, 120_000, 100_000, &pairing);
        assert_as_without_read(&detained, &plain, "a price past the forward");
        assert_eq!(slot(&detained, 0), Some(U256::ZERO), "the call fails and its caller goes on");

        let detained = run(TIMESTAMP, kzg_address, 80_000, 50_000, &kzg);
        let plain = run(PUSH0, kzg_address, 80_000, 50_000, &kzg);
        assert_as_without_read(&detained, &plain, "a price past the forward");
        assert_eq!(slot(&detained, 0), Some(U256::ZERO), "the call fails and its caller goes on");

        let runs = Runs::default();
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, calls_precompile(TIMESTAMP, PRICED, Some(120_000)));
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits(100_000)))
            .with_dyn_precompiles(HashMap::from_iter([(PRICED, priced(&runs))]));
        let own = run_on(&mut evm, tx_with(&own_input, gas_limit));
        let allowance = 100_000 - charges_to_the_precompile(32, Some(120_000)).total();
        assert_stopped(&own, intrinsic_with(&own_input, gas_limit), allowance);
        assert_eq!(*runs.lock().unwrap(), [(allowance, false)], "run on the allowance");
    }
}

/// A precompile priced between the allowance and its forward whose input fails a check made after
/// its gas check: the BN254 pairing of three pairs and a stray byte, priced 147,000 for its three
/// whole pairs, which checks the input's length after its gas. Under a cap of 100,000 it is
/// answered without running, before the length check, and the transaction stops; without the read
/// the length check fails the call, which burns its forward, and the caller goes on to store the
/// failure. Under the default cap the allowance pays the price, and the call fails on its input as
/// without the read.
#[test]
fn test_a_precompile_whose_input_fails_past_its_gas_check_stops_past_the_allowance() {
    let input = vec![0_u8; 3 * 192 + 1];
    for gas_limit in TIERS {
        let run = |first, cap| {
            let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
            let code = calls_precompile(first, EC_PAIRING, None);
            let db = MemoryDatabase::default().account_code(CONTRACT, code);
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
                .with_inspector(Calls::default());
            let run = run_on(&mut evm, tx_with(&input, gas_limit));
            let call = evm.inspector().calls.iter().find(|call| call.target == EC_PAIRING).unwrap();
            (run, call.precompile_ran)
        };
        let ((detained, ran), (plain, plain_ran)) = (run(TIMESTAMP, 100_000), run(PUSH0, 100_000));
        let allowance = 100_000 - charges_to_the_precompile(input.len(), None).total();
        assert_stopped(&detained, intrinsic_with(&input, gas_limit), allowance);
        assert!(!ran && plain_ran, "answered without running");
        assert!(plain.outcome.result.is_success());
        assert_eq!(slot(&plain, 0), Some(U256::ZERO), "without the read the call fails");
        let ((detained, _), (plain, _)) = (run(TIMESTAMP, CAP), run(PUSH0, CAP));
        assert_as_without_read(&detained, &plain, "a price the allowance pays");
        assert_eq!(slot(&detained, 0), Some(U256::ZERO), "the call fails on its input");
    }
}

/// A size-limited precompile of the Satin set called with `input`, which its EIP prices at
/// `price`, after a read, against the same call without it:
///
/// - forwarded all the gas under a cap that leaves it less than its price, it needs gas the limit
///   withholds: it is answered without running, and the transaction stops, where without the read
///   it computes;
/// - under the spec's cap, which leaves it its price, revm runs it and it charges its price, as
///   without the read;
/// - forwarded one gas short of its price under a cap that leaves it less still, it runs on its
///   forward and runs out of gas as without the read, and its caller goes on.
fn assert_held_from_its_price(to: Address, input: &[u8], price: u64) {
    let entry = mega_evm::satin_precompiles().get(&to).unwrap();
    assert_eq!(entry.required_gas(input), Some(price), "the entry's price is the EIP's");
    let short = u32::try_from(price - 1).unwrap();
    let cap = price / 2;
    for gas_limit in TIERS {
        let run = |first, cap, gas| {
            let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
            let db =
                MemoryDatabase::default().account_code(CONTRACT, calls_precompile(first, to, gas));
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
                .with_inspector(Calls::default());
            let run = run_on(&mut evm, tx_with(input, gas_limit));
            let call = evm.inspector().calls.iter().find(|call| call.target == to).unwrap();
            let (forward, spent, ran) = (call.forward, call.spent, call.precompile_ran);
            (run, forward, spent, ran)
        };

        // Past the allowance, within the forward: the stop, without running.
        let (detained, forward, _, ran) = run(TIMESTAMP, cap, None);
        let (plain, _, plain_spent, plain_ran) = run(PUSH0, cap, None);
        let allowance = cap - charges_to_the_precompile(input.len(), None).total();
        assert!(allowance < price && price <= forward, "{to}: {allowance} {price} {forward}");
        assert_stopped(&detained, intrinsic_with(input, gas_limit), allowance);
        assert!(!ran, "{to}: answered without running");
        assert!(plain_ran && plain.outcome.result.is_success(), "{to}");
        assert_eq!(plain_spent, price, "{to}: without the read it computes, at its price");
        assert_eq!(slot(&plain, 0), Some(U256::from(1)), "{to}");

        // Within the allowance: run, at its price, as without the read.
        let (detained, _, spent, ran) = run(TIMESTAMP, CAP, None);
        let (plain, ..) = run(PUSH0, CAP, None);
        assert!(ran, "{to}: revm runs it");
        assert_eq!(spent, price, "{to}: it charges its price");
        assert_as_without_read(&detained, &plain, "a price the allowance pays");
        assert_eq!(slot(&detained, 0), Some(U256::from(1)), "{to}");

        // Past the whole forward: run on the forward, out of gas, as without the read.
        let (detained, forward, _, ran) = run(TIMESTAMP, cap, Some(short));
        let (plain, ..) = run(PUSH0, cap, Some(short));
        let allowance = cap - charges_to_the_precompile(input.len(), Some(short)).total();
        assert!(allowance < forward && forward < price, "{to}: {allowance} {forward} {price}");
        assert!(ran, "{to}: revm runs it");
        assert_as_without_read(&detained, &plain, "a price past the forward");
        assert_eq!(
            slot(&detained, 0),
            Some(U256::ZERO),
            "{to}: the call fails, its caller goes on"
        );
    }
}

/// op-revm's wrapper of the BN254 pairing, two pairs of zeros: 45,000, and 34,000 a pair
/// (EIP-1108).
#[test]
fn test_the_bn254_pairing_is_held_from_its_price() {
    assert_held_from_its_price(EC_PAIRING, &[0; 2 * 192], 45_000 + 2 * 34_000);
}

/// op-revm's wrapper of the BLS12-381 G1 MSM, two pairs of zeros: 12,000 a pair, discounted to
/// 949 per mille for two (EIP-2537).
#[test]
fn test_the_bls12_g1_msm_is_held_from_its_price() {
    assert_held_from_its_price(BLS12_G1_MSM, &[0; 2 * 160], 2 * 12_000 * 949 / 1_000);
}

/// op-revm's wrapper of the BLS12-381 G2 MSM, two pairs of zeros: 22,500 a pair, undiscounted for
/// two (EIP-2537).
#[test]
fn test_the_bls12_g2_msm_is_held_from_its_price() {
    assert_held_from_its_price(BLS12_G2_MSM, &[0; 2 * 288], 2 * 22_500 * 1_000 / 1_000);
}

/// op-revm's wrapper of the BLS12-381 pairing, two pairs of zeros: 37,700, and 32,600 a pair
/// (EIP-2537).
#[test]
fn test_the_bls12_pairing_is_held_from_its_price() {
    assert_held_from_its_price(BLS12_PAIRING, &[0; 2 * 384], 37_700 + 2 * 32_600);
}

/// A keyless deployment's call is the transaction's own frame, and runs no code: its own charges —
/// the overhead of decoding the transaction and recovering its signer, then the `CREATE` opcode's
/// regular gas — are compute, drawn from gas held to what the limit leaves the frame, whether a
/// rule then refuses the call or its creation runs. A `keylessDeploy` call from the beneficiary,
/// detained from its start, carrying value (refused, `NoEtherTransfer()`) or deploying the
/// canonical `CREATE2` factory, against the same call from another sender, plain and inspected:
///
/// - under a cap below the overhead, the overhead alone crosses the limit, and both stop at it;
/// - under a cap past the overhead and short of the `CREATE` opcode's regular gas, the refusal
///   answers as without the read, and the deployment stops at the opcode's charge;
/// - under a cap past both and one gas short of what the creation computes, the creation stops at
///   the limit where it would deploy, and the stop takes the deployment back; one gas more, and it
///   deploys as without the read;
/// - under the spec's cap, both run as without the read.
#[test]
fn test_a_keyless_calls_charges_are_held_to_what_the_limit_leaves() {
    let refused = keyless_deploy_call(b"a transaction");
    let deploys = keyless_deploy_call(CREATE2_FACTORY_TX);
    let init_code = decode_keyless_tx(CREATE2_FACTORY_TX).unwrap().tx().input.len();
    let create = satin_gas_params().create_cost() + satin_gas_params().initcode_cost(init_code);
    for gas_limit in TIERS {
        let run = |caller: Address, value: u64, data: &Bytes, cap: u64, inspected: bool| {
            let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
            let db = MemoryDatabase::default()
                .account_balance(caller, U256::from(10))
                .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);
            let tx = OpTx(op_transaction(TxEnv {
                caller,
                kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
                gas_limit,
                value: U256::from(value),
                data: data.clone(),
                ..Default::default()
            }));
            let evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
            if inspected {
                run_on(&mut evm.with_inspector(Calls::default()), tx)
            } else {
                run_on(&mut { evm }, tx)
            }
        };
        // Each case plain and inspected: the inspected path settles the call as the plain one.
        let both = |caller, value, data: &Bytes, cap| {
            let plain = run(caller, value, data, cap, false);
            let inspected = run(caller, value, data, cap, true);
            assert_eq!(inspected.outcome.result, plain.outcome.result, "{caller} under {cap}");
            assert_eq!(inspected.outcome.gas, plain.outcome.gas, "{caller} under {cap}");
            plain
        };
        let answered = both(CALLER, 1, &refused, CAP);
        assert_eq!(
            answered.outcome.result.output(),
            Some(&Bytes::from(IKeylessDeploy::NoEtherTransfer::SELECTOR.to_vec())),
            "the answer without detention"
        );
        let refused_intrinsic = answered.outcome.gas.regular - KEYLESS_DEPLOY_OVERHEAD_GAS;
        let deployed = both(CALLER, 0, &deploys, CAP);
        let answer = IKeylessDeploy::keylessDeployCall::abi_decode_returns(
            deployed.outcome.result.output().unwrap(),
        )
        .unwrap();
        assert_eq!(answer.deployedAddress, CREATE2_FACTORY_CONTRACT, "it deploys");
        let deploy_intrinsic = intrinsic_with(&deploys, gas_limit);

        // Detained from its start, the call's overhead is its first charge, then the `CREATE`
        // opcode's, then its creation's, whose last is the hash of the code it deposits.
        let cap = KEYLESS_DEPLOY_OVERHEAD_GAS / 2;
        let detained = both(BENEFICIARY, 1, &refused, cap);
        assert_eq!(detained.limit, Some(cap), "the sender is the beneficiary");
        assert_stopped(&detained, refused_intrinsic, cap);
        assert_stopped(&both(BENEFICIARY, 0, &deploys, cap), deploy_intrinsic, cap);

        let cap = KEYLESS_DEPLOY_OVERHEAD_GAS + create - 1;
        let detained = both(BENEFICIARY, 1, &refused, cap);
        assert_eq!(detained.outcome.result, answered.outcome.result, "the overhead fits");
        assert_eq!(detained.outcome.gas, answered.outcome.gas);
        let stopped = both(BENEFICIARY, 0, &deploys, cap);
        let left = Charges::default().then(&[KEYLESS_DEPLOY_OVERHEAD_GAS, create]).left(cap);
        assert_eq!(left, create - 1);
        assert_stopped(&stopped, deploy_intrinsic, left);
        assert_eq!(nonce_of(&stopped, CREATE2_FACTORY_DEPLOYER), 0, "the creation never started");

        // The creation's own compute: the regular gas the deployment spent past the call's.
        let creation =
            deployed.outcome.gas.regular - deploy_intrinsic - KEYLESS_DEPLOY_OVERHEAD_GAS - create;
        let cap = KEYLESS_DEPLOY_OVERHEAD_GAS + create + creation - 1;
        let stopped = both(BENEFICIARY, 0, &deploys, cap);
        let code = deployed.outcome.state[&CREATE2_FACTORY_CONTRACT].info.code.clone().unwrap();
        let hash = 6 * (code.len() as u64).div_ceil(32);
        let left = Charges::default()
            .then(&[KEYLESS_DEPLOY_OVERHEAD_GAS, create, creation - hash, hash])
            .left(cap);
        assert_eq!(left, hash - 1, "the hash of the deposited code crosses");
        assert_stopped(&stopped, deploy_intrinsic, left);
        assert!(
            stopped.outcome.state.get(&CREATE2_FACTORY_CONTRACT).is_none_or(|a| a.info.is_empty()),
            "the stop takes the deployment back"
        );
        assert_eq!(nonce_of(&stopped, CREATE2_FACTORY_DEPLOYER), 0, "the signer's nonce with it");
        let detained = both(BENEFICIARY, 0, &deploys, cap + 1);
        assert_eq!(detained.outcome.result, deployed.outcome.result, "the creation fits");
        assert_eq!(detained.outcome.gas, deployed.outcome.gas);

        let detained = both(BENEFICIARY, 0, &deploys, CAP);
        assert_eq!(detained.limit, Some(CAP));
        assert_eq!(detained.outcome.result, deployed.outcome.result, "as without the read");
        assert_eq!(detained.outcome.gas, deployed.outcome.gas);
    }
}

/// A refused `keylessDeploy` call is an answer held to the allowance by the regular gas it spent:
/// below the execution cap, the signer's account it was charged spills onto its regular gas, and
/// that is state gas, not compute. The canonical `CREATE2` factory's signer has no account, and
/// its address is taken, so the call pays the overhead and the signer's account and is refused
/// `ContractAlreadyExists()`. From the beneficiary, under a cap past the overhead and short of the
/// two together, it is refused as without the read, below and above the cap.
#[test]
fn test_a_refused_keyless_calls_spilled_state_gas_is_not_compute() {
    let data = keyless_deploy_call(CREATE2_FACTORY_TX);
    for gas_limit in TIERS {
        let run = |caller: Address, cap: u64| {
            let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
            let db = MemoryDatabase::default()
                .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
                .account_code(CREATE2_FACTORY_CONTRACT, BytecodeBuilder::default().stop().build());
            let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
            run_on(&mut evm, tx_to(caller, KEYLESS_DEPLOY_ADDRESS, &data, gas_limit))
        };
        let refused = run(CALLER, CAP);
        assert_eq!(
            refused.outcome.result.output(),
            Some(&Bytes::from(IKeylessDeploy::ContractAlreadyExists::SELECTOR.to_vec())),
        );
        let cap = KEYLESS_DEPLOY_OVERHEAD_GAS + 1_000;
        let detained = run(BENEFICIARY, cap);
        assert_eq!(detained.limit, Some(cap), "the sender is the beneficiary");
        assert_eq!(detained.outcome.result, refused.outcome.result, "refused as without the read");
        assert_eq!(detained.outcome.gas, refused.outcome.gas);
        assert_eq!(detained.outcome.limit_exceeded, None);
    }
}

/// The calldata of a `keylessDeploy` call carrying `tx`, with a `gasLimitOverride` past any
/// forward.
fn keyless_deploy_call(tx: &[u8]) -> Bytes {
    IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::copy_from_slice(tx),
        gasLimitOverride: U256::from(u64::MAX),
    }
    .abi_encode()
    .into()
}

/// The nonce `address` ends the transaction with; zero when the transaction did not touch it.
fn nonce_of(run: &Run, address: Address) -> u64 {
    run.outcome.state.get(&address).map_or(0, |account| account.info.nonce)
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

/// Answers the call to [`CONTRACT`] itself: past the cap in regular gas, and with 1,000 of state or
/// history gas charged on top.
#[derive(Clone, Copy)]
struct AnswersPastTheCapWithASpill {
    history: bool,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersPastTheCapWithASpill {
    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address != CONTRACT {
            return None;
        }
        let mut gas = Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
        assert!(gas.record_regular_cost(CAP + 1));
        if self.history {
            assert!(gas.record_history_cost(1_000));
        } else {
            assert!(gas.record_state_cost(1_000));
        }
        Some(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Return, Bytes::new(), gas),
            inputs.return_memory_offset.clone(),
        ))
    }
}

/// An answer past the allowance is the stop, billed the intrinsic gas alone, whatever state or
/// history gas the answer charged: the answer's spending is the charge that crossed, so none of it
/// is made, and the frame, detained from its start, computed nothing before it. Below the
/// execution cap the state or history gas spills onto regular gas and the stop's revert gives it
/// back; above the cap the reservoir pays it and nothing spills.
#[test]
fn test_an_answer_past_the_allowance_with_a_spill_is_billed_nothing_of_it() {
    for gas_limit in TIERS {
        for history in [false, true] {
            let db = MemoryDatabase::default().account_code(CONTRACT, Bytes::from_static(&[STOP]));
            let intrinsic = run_on(
                &mut MegaEvm::new(context(db.clone())),
                tx(BENEFICIARY, CONTRACT, gas_limit),
            )
            .outcome
            .gas
            .regular;
            let mut evm =
                MegaEvm::new(context(db)).with_inspector(AnswersPastTheCapWithASpill { history });
            let run = run_on(&mut evm, tx(BENEFICIARY, CONTRACT, gas_limit));
            assert_eq!(run.outcome.result.output(), Some(&stop_data(CAP)), "{gas_limit} {history}");
            assert_eq!(
                run.outcome.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: LimitKind::ComputeGas,
                    limit: CAP,
                    used: 0,
                    frame_local: false
                }),
                "{gas_limit}, history {history}"
            );
            assert_eq!(run.outcome.gas.regular, intrinsic, "{gas_limit}, history {history}");
        }
    }
}
