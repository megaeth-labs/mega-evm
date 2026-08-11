//! REX7 checkpoint compute-gas settlement.
//!
//! Under checkpoint accounting the plain opcodes run revm's raw instructions with no per-opcode
//! recording; compute gas settles as an interpreter-gas delta at each checkpoint — the storage-gas
//! opcodes, the CALL / CREATE family, the volatile opcodes, and frame entry / resume / exit.
//!
//! The property these tests pin is the **precision invariant**: for a transaction that stays
//! inside every per-tx limit, the settled totals are bit-identical to per-opcode recording, so
//! REX6 and REX7 produce the same compute gas, the same four-dimension usage, the same receipt
//! `gas_used`, and the same execution result. The interpreter's gas counter meters every opcode
//! anyway, so summing it by segment reproduces the per-opcode sum exactly.
//!
//! The two places where the models are *not* identical are pinned at the bottom of this file:
//! a limit crossing inside a plain-opcode segment surfaces at the next checkpoint rather than at
//! the crossing opcode, and a frame that halts out of gas settles its burned remainder as compute
//! gas.

use crate::common::{
    transact, transact_default, transact_with_bucket_capacity, Outcome, CALLEE, CALLER, CONTRACT,
    EMPTY_TARGET, ONE_ETH,
};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{
    ADD, BALANCE, CALL, CALLCODE, CREATE, CREATE2, DELEGATECALL, DUP1, EXTCODEHASH, EXTCODESIZE,
    GAS, JUMPDEST, JUMPI, LOG1, MUL, POP, SELFDESTRUCT, SLOAD, SSTORE, STATICCALL, STOP, SUB,
    SWAP1, TIMESTAMP,
};

/// A third contract, so a CALL chain can reach depth 2.
const INNER: Address = address!("0000000000000000000000000000000000300004");

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// A SALT bucket capacity four times the minimum, so every SALT-scaled storage-gas charge
/// (`SSTORE` set, new account, contract creation) is non-zero and the settlement sites that have
/// to exclude those charges from the compute window are actually exercised.
const SCALED_BUCKET_CAPACITY: u64 = 4 * mega_evm::MIN_BUCKET_SIZE as u64;

/// Asserts that `build_db()` executes identically under REX6 (per-opcode recording) and REX7
/// (checkpoint settlement): same success, same result, same four-dimension usage, same `gas_used`.
///
/// Every case is run twice — once with minimum SALT buckets and once with
/// [`SCALED_BUCKET_CAPACITY`] — so the storage-gas exclusions are checked with a non-zero charge
/// as well. The returned outcomes are the minimum-bucket ones.
fn assert_settlement_parity(
    label: &str,
    expect_success: bool,
    build_db: impl Fn() -> MemoryDatabase,
) -> (Outcome, Outcome) {
    assert_settlement_parity_with_limits(
        label,
        expect_success,
        build_db,
        EvmTxRuntimeLimits::from_spec,
    )
}

/// [`assert_settlement_parity`] with per-spec runtime limits.
fn assert_settlement_parity_with_limits(
    label: &str,
    expect_success: bool,
    build_db: impl Fn() -> MemoryDatabase,
    limits: impl Fn(MegaSpecId) -> EvmTxRuntimeLimits,
) -> (Outcome, Outcome) {
    let r6 = transact(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7));
    assert_outcomes_match(label, expect_success, &r6, &r7);

    let scaled_label = alloc_scaled_label(label);
    let s6 = transact_with_bucket_capacity(
        MegaSpecId::REX6,
        build_db(),
        limits(MegaSpecId::REX6),
        SCALED_BUCKET_CAPACITY,
    );
    let s7 = transact_with_bucket_capacity(
        MegaSpecId::REX7,
        build_db(),
        limits(MegaSpecId::REX7),
        SCALED_BUCKET_CAPACITY,
    );
    assert_outcomes_match(&scaled_label, expect_success, &s6, &s7);

    (r6, r7)
}

fn alloc_scaled_label(label: &str) -> String {
    format!("{label} (scaled SALT buckets)")
}

fn assert_outcomes_match(label: &str, expect_success: bool, r6: &Outcome, r7: &Outcome) {
    assert_eq!(
        r6.is_success(),
        expect_success,
        "{label}: REX6 success expectation mismatch; got {:?}",
        r6.result
    );
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "{label}: execution result must be identical; REX6={:?} REX7={:?}",
        r6.result,
        r7.result
    );
    assert_eq!(
        r6.compute_gas, r7.compute_gas,
        "{label}: checkpoint settlement must telescope to the per-opcode compute-gas sum; \
         REX6={} REX7={}",
        r6.compute_gas, r7.compute_gas
    );
    assert_eq!(
        r6.gas_used, r7.gas_used,
        "{label}: receipt gas_used must be unchanged; REX6={} REX7={}",
        r6.gas_used, r7.gas_used
    );
    assert_eq!(
        (r6.data_size, r6.kv_updates, r6.state_growth),
        (r7.data_size, r7.kv_updates, r7.state_growth),
        "{label}: the non-compute dimensions must be unchanged",
    );
}

/// A countdown loop of cheap opcodes with no checkpoint anywhere inside the loop body:
///
/// ```text
/// PUSH2 iterations; loop: JUMPDEST; PUSH1 1; SWAP1; SUB; DUP1; PUSH1 loop; JUMPI; STOP
/// ```
///
/// `prefix` is prepended verbatim and participates in the jump-target offset.
fn countdown_loop_code(prefix: &[u8], iterations: u16) -> Bytes {
    let mut code = prefix.to_vec();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = u8::try_from(code.len()).expect("loop target must fit in a PUSH1");
    code.push(JUMPDEST);
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(SWAP1);
    code.push(SUB);
    code.push(DUP1);
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(JUMPI);
    code.push(STOP);
    Bytes::from(code)
}

/// `pairs` PUSH1/POP pairs — plain opcodes that record nothing of their own under checkpoint
/// accounting.
fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// A pure arithmetic loop settles only at the frame-exit checkpoint, and that single settlement
/// must equal the sum the per-opcode wrappers would have recorded opcode by opcode.
#[test]
fn test_plain_arithmetic_loop_settles_to_the_per_opcode_sum() {
    let code = countdown_loop_code(&[], 500);
    let (r6, _) = assert_settlement_parity("plain loop", true, || base_db(code.clone()));
    assert!(r6.compute_gas > 10_000, "the loop must be substantial; compute={}", r6.compute_gas);
}

/// A segment that runs plain opcodes, hits a mid-code checkpoint, then runs more plain opcodes
/// before the frame exits: the two settlements must partition the frame's gas exactly.
#[test]
fn test_plain_segments_around_a_mid_code_checkpoint() {
    let code = plain_filler(BytecodeBuilder::default(), 40)
        .push_u256(U256::from(99u64))
        .push_u256(U256::from(7u64))
        .append(SSTORE);
    let code = plain_filler(code, 40).append(STOP).build();
    assert_settlement_parity("plain | SSTORE | plain", true, || base_db(code.clone()));
}

/// SSTORE and LOG both charge storage gas inside their compute window and subtract it back out.
/// Interleaving them with plain opcodes checks that the subtraction stays exact once the window
/// also spans the plain opcodes before them.
#[test]
fn test_sstore_and_log_mixed_with_plain_opcodes() {
    let code = plain_filler(BytecodeBuilder::default(), 20)
        .sstore(U256::from(1), U256::from(0x11))
        .mstore(0, [0x22u8; 32])
        .push_number(0xabcu64) // topic0
        .push_number(32u64) // len
        .push_number(0u64) // offset
        .append(LOG1);
    let code = plain_filler(code, 20).sstore(U256::from(2), U256::from(0x33)).append(STOP).build();
    let (r6, _) = assert_settlement_parity("SSTORE + LOG mix", true, || base_db(code.clone()));
    assert!(r6.data_size > 0, "the log and stores must register data size");
    assert!(r6.kv_updates > 0, "the stores must register KV updates");
}

/// A cold then warm SLOAD, each a checkpoint, with plain opcodes between them.
#[test]
fn test_sload_checkpoints_with_plain_opcodes_between() {
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .push_u256(U256::from(3))
        .append(SLOAD)
        .append(POP);
    let code = plain_filler(code, 10)
        .push_u256(U256::from(3))
        .append(SLOAD)
        .append(POP)
        .append(STOP)
        .build();
    assert_settlement_parity("SLOAD checkpoints", true, || base_db(code.clone()));
}

/// Bytecode for a CALL to `target` forwarding `gas_limit` and `value`.
fn call_code(target: Address, value: u64, gas_limit: u64) -> BytecodeBuilder {
    BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(gas_limit)
        .append(CALL)
        .append(POP)
}

/// A CALL sub-frame that succeeds: the caller's segment settles at the CALL checkpoint (before
/// `frame_init`), the callee settles its own segments, and the caller's window re-opens at the
/// resume with the callee's returned gas already merged back.
#[test]
fn test_call_subframe_success() {
    let callee = plain_filler(BytecodeBuilder::default(), 15)
        .sstore(U256::from(5), U256::from(0x77))
        .append(STOP)
        .build();
    let code = plain_filler(call_code(CALLEE, 0, 1_000_000), 15).append(STOP).build();
    assert_settlement_parity("CALL success", true, || {
        base_db(code.clone()).account_code(CALLEE, callee.clone())
    });
}

/// A CALL sub-frame that reverts: the callee's segments still settle (compute gas is persistent
/// even when the frame's state changes are dropped), and the returned gas re-opens the caller's
/// window at the resume.
#[test]
fn test_call_subframe_revert() {
    let callee = plain_filler(BytecodeBuilder::default(), 15)
        .sstore(U256::from(5), U256::from(0x77))
        .revert()
        .build();
    let code = plain_filler(call_code(CALLEE, 0, 1_000_000), 15).append(STOP).build();
    let (r6, _) = assert_settlement_parity("CALL revert", true, || {
        base_db(code.clone()).account_code(CALLEE, callee.clone())
    });
    assert!(r6.compute_gas > 0);
}

/// A value-transferring CALL to an empty account: the new-account storage gas is charged inside
/// the CALL's compute window and subtracted back out, with the window now also spanning the plain
/// opcodes ahead of it.
#[test]
fn test_call_value_transfer_to_empty_account() {
    let code = plain_filler(call_code(EMPTY_TARGET, 1, 1_000_000), 10).append(STOP).build();
    assert_settlement_parity("CALL value transfer", true, || base_db(code.clone()));
}

/// Two nested CALL frames, so the resume path runs at two depths.
#[test]
fn test_nested_call_frames() {
    let inner = plain_filler(BytecodeBuilder::default(), 10)
        .sstore(U256::from(9), U256::from(0x99))
        .append(STOP)
        .build();
    let callee = plain_filler(call_code(INNER, 0, 500_000), 10).append(STOP).build();
    let code = plain_filler(call_code(CALLEE, 0, 2_000_000), 10).append(STOP).build();
    assert_settlement_parity("nested CALL", true, || {
        base_db(code.clone())
            .account_code(CALLEE, callee.clone())
            .account_code(INNER, inner.clone())
    });
}

/// DELEGATECALL, STATICCALL and CALLCODE all reach the same checkpoint chain as CALL.
#[test]
fn test_delegatecall_staticcall_callcode_frames() {
    let callee = plain_filler(BytecodeBuilder::default(), 10).append(STOP).build();
    for (label, opcode, value_operand) in [
        ("DELEGATECALL", DELEGATECALL, false),
        ("STATICCALL", STATICCALL, false),
        ("CALLCODE", CALLCODE, true),
    ] {
        let mut builder = BytecodeBuilder::default()
            .push_number(0u64) // retSize
            .push_number(0u64) // retOffset
            .push_number(0u64) // argsSize
            .push_number(0u64); // argsOffset
        if value_operand {
            builder = builder.push_number(0u64);
        }
        let code = plain_filler(
            builder.push_address(CALLEE).push_number(500_000u64).append(opcode).append(POP),
            10,
        )
        .append(STOP)
        .build();
        assert_settlement_parity(label, true, || {
            base_db(code.clone()).account_code(CALLEE, callee.clone())
        });
    }
}

/// Initcode that deploys `runtime` as the created contract's code.
fn deploying_initcode(runtime: &[u8]) -> Vec<u8> {
    BytecodeBuilder::default().return_with_data(runtime).build_vec()
}

/// A CREATE whose child frame really runs initcode: the create frame gets its own window at entry
/// and its own tail settlement at exit, and the code-deposit accounting runs on top of both.
#[test]
fn test_create_child_frame() {
    let runtime = plain_filler(BytecodeBuilder::default(), 4).append(STOP).build_vec();
    let initcode = deploying_initcode(&runtime);
    let len = initcode.len() as u64;
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .mstore(0, &initcode)
        .push_number(len) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP);
    let code = plain_filler(code, 10).append(STOP).build();
    assert_settlement_parity("CREATE", true, || base_db(code.clone()));
}

/// CREATE2 folds its memory-expansion gas into the same single window, which under checkpoint
/// accounting also spans the plain opcodes before it.
#[test]
fn test_create2_child_frame() {
    let runtime = plain_filler(BytecodeBuilder::default(), 4).append(STOP).build_vec();
    let initcode = deploying_initcode(&runtime);
    let len = initcode.len() as u64;
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .mstore(0, &initcode)
        .push_number(0x5a5au64) // salt
        .push_number(len) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(POP);
    let code = plain_filler(code, 10).append(STOP).build();
    assert_settlement_parity("CREATE2", true, || base_db(code.clone()));
}

/// SELFDESTRUCT to an empty beneficiary charges new-account storage gas from a site that is not
/// the settlement site, so the open window's baseline has to be lowered by exactly that charge.
#[test]
fn test_selfdestruct_new_beneficiary_excludes_its_storage_gas() {
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .push_address(EMPTY_TARGET)
        .append(SELFDESTRUCT)
        .build();
    assert_settlement_parity("SELFDESTRUCT new beneficiary", true, || base_db(code.clone()));
}

/// SELFDESTRUCT to an existing beneficiary takes the other arm (no storage-gas charge, REX6+
/// account-write accounting only).
#[test]
fn test_selfdestruct_existing_beneficiary() {
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .push_address(CALLEE)
        .append(SELFDESTRUCT)
        .build();
    let callee = BytecodeBuilder::default().append(STOP).build();
    assert_settlement_parity("SELFDESTRUCT existing beneficiary", true, || {
        base_db(code.clone()).account_code(CALLEE, callee.clone())
    });
}

/// TIMESTAMP marks block-environment access and lowers the compute-gas limit to
/// `usage_at_access + cap`. With the cap comfortably above what the rest of the transaction
/// spends, detention is engaged but never binding — and the detention cap must be derived from
/// fully settled usage, so REX6 and REX7 must still agree bit for bit.
#[test]
fn test_block_env_detention_below_the_cap() {
    let code = countdown_loop_code(&[TIMESTAMP, POP], 200);
    let limits = |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = 1_000_000;
        limits
    };
    assert_settlement_parity_with_limits(
        "TIMESTAMP detention",
        true,
        || base_db(code.clone()),
        limits,
    );
}

/// The beneficiary-conditional volatile checkpoints (BALANCE / EXTCODESIZE / EXTCODEHASH) charge
/// their static gas after the raw instruction, which under checkpoint accounting lands inside the
/// settled segment rather than being added back.
#[test]
fn test_conditional_volatile_checkpoints() {
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .push_address(CALLEE)
        .append(BALANCE)
        .append(POP)
        .push_address(CALLEE)
        .append(EXTCODESIZE)
        .append(POP)
        .push_address(CALLEE)
        .append(EXTCODEHASH)
        .append(POP);
    let code = plain_filler(code, 10).append(STOP).build();
    let callee = BytecodeBuilder::default().append(STOP).build();
    assert_settlement_parity("conditional volatile", true, || {
        base_db(code.clone()).account_code(CALLEE, callee.clone())
    });
}

/// `GAS` is not a checkpoint: it runs raw and reads the interpreter's own counter. The settlement
/// must not perturb that counter, so a contract that stores its `GAS` reading must store the same
/// value under both specs.
#[test]
fn test_gas_opcode_reads_the_same_remaining_gas() {
    let code = plain_filler(BytecodeBuilder::default(), 10)
        .append(GAS)
        .push_u256(U256::from(4))
        .append(SSTORE)
        .append(STOP)
        .build();
    let (r6, r7) = assert_settlement_parity("GAS reading", true, || base_db(code.clone()));
    let slot = U256::from(4);
    assert_eq!(
        r6.storage_value(CONTRACT, slot),
        r7.storage_value(CONTRACT, slot),
        "GAS must observe the same interpreter gas under both accounting models",
    );
    assert!(!r6.storage_value(CONTRACT, slot).is_zero(), "the GAS reading must be non-zero");
}

/// A long straight run of arithmetic with no checkpoint at all, so the whole frame is one segment
/// settled once at the frame-exit checkpoint.
#[test]
fn test_single_segment_frame() {
    let mut builder = BytecodeBuilder::default().push_number(7u64);
    for _ in 0..200 {
        builder = builder.push_number(3u64).append(ADD).push_number(2u64).append(MUL);
    }
    let code = builder.append(POP).append(STOP).build();
    assert_settlement_parity("single segment", true, || base_db(code.clone()));
}

/// Pushes the operands of an `SSTORE(slot=7, value=99)` and stops before the SSTORE byte, so a run
/// measures the compute gas accumulated up to the opcode under test.
fn plain_run_then_sstore_code(pairs: usize, include_sstore: bool) -> Bytes {
    let builder = plain_filler(BytecodeBuilder::default(), pairs)
        .push_u256(U256::from(99u64))
        .push_u256(U256::from(7u64));
    let builder = if include_sstore { builder.append(SSTORE) } else { builder };
    builder.append(STOP).build()
}

/// The one enforcement difference this ticket's model has: a compute-gas crossing inside a
/// plain-opcode segment is not caught at the crossing opcode — nothing is metered there — but at
/// the next checkpoint. Both specs halt; REX7 records the whole segment up to that checkpoint,
/// which is exactly what an unconstrained run records at the same point.
#[test]
fn test_compute_limit_crossing_surfaces_at_the_next_checkpoint() {
    let code = plain_run_then_sstore_code(200, true);
    let usage_before_sstore =
        transact_default(MegaSpecId::REX7, base_db(plain_run_then_sstore_code(200, false)))
            .compute_gas;
    // Trip the limit partway through the plain run, well before the SSTORE checkpoint.
    let intrinsic =
        transact_default(MegaSpecId::REX7, base_db(plain_run_then_sstore_code(0, false)))
            .compute_gas;
    let compute_limit = intrinsic + (usage_before_sstore - intrinsic) / 2;
    let limits =
        |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(compute_limit);

    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code.clone()), limits(MegaSpecId::REX7));
    let r7_unconstrained = transact_default(MegaSpecId::REX7, base_db(code));

    assert!(!r6.is_success(), "REX6 must halt on the tight compute limit; got {:?}", r6.result);
    assert!(!r7.is_success(), "REX7 must halt on the tight compute limit; got {:?}", r7.result);
    assert!(
        r6.compute_gas <= compute_limit + 12,
        "REX6 halts at the crossing opcode; compute={} limit={compute_limit}",
        r6.compute_gas
    );
    assert_eq!(
        r7.compute_gas, r7_unconstrained.compute_gas,
        "REX7 settles the whole segment through the SSTORE checkpoint",
    );
    assert!(
        r7.compute_gas > r6.compute_gas,
        "checkpoint enforcement overshoots per-opcode enforcement; REX6={} REX7={}",
        r6.compute_gas,
        r7.compute_gas
    );
}

/// The second difference: a frame that halts out of EVM gas has its remaining budget zeroed by the
/// interpreter before the frame-exit settlement reads the counter, so the burned remainder settles
/// as compute gas. Per-opcode recording attributes nothing to the failing opcode and nothing to
/// the burn, so REX7 reports strictly more compute gas for such a frame.
///
/// The direction is the safe one — compute usage is over-reported, never under-reported — and the
/// halt itself is identical.
#[test]
fn test_out_of_gas_frame_settles_its_burned_gas_as_compute() {
    // A callee that runs out of the gas its caller forwarded.
    let callee = countdown_loop_code(&[], 10_000);
    let code = call_code(CALLEE, 0, 5_000).append(STOP).build();
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let r6 = transact_default(MegaSpecId::REX6, build_db());
    let r7 = transact_default(MegaSpecId::REX7, build_db());

    assert!(r6.is_success(), "the outer transaction survives the callee's OOG: {:?}", r6.result);
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the halt itself is unchanged",
    );
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas is unchanged");
    assert!(
        r7.compute_gas > r6.compute_gas,
        "the burned remainder settles as compute gas under REX7; REX6={} REX7={}",
        r6.compute_gas,
        r7.compute_gas
    );
}
