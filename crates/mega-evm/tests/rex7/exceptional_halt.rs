//! REX7 exceptional-halt frame settlement.
//!
//! A frame that ends in an exceptional halt never returns its remaining budget: the top-level
//! frame's whole envelope is spent by the transaction's final gas accounting, and an inner frame's
//! remainder is simply not handed back to its caller. REX7 settles that burned remainder as compute
//! gas at frame exit, so the recorded compute total covers the entire budget the sender's gas paid
//! for. Per-opcode recording through REX6 attributes neither the failing opcode nor the burn, so it
//! reports strictly less.
//!
//! The interpreter only zeroes its own counter for a plain `OutOfGas`; every other exceptional halt
//! keeps the loop-exit reading and has its remainder burned later, by the frame-return rules. The
//! settlement therefore cannot be read off the counter — it has to be driven by the halt
//! classification, which is what these tests sweep.
//!
//! The invariant each case pins is the same one in both frame positions. These transactions spend
//! EVM gas on exactly two things: compute, and the transaction-intrinsic storage gas that is
//! excluded from compute accounting by definition. So "the whole burned budget settled as compute"
//! is `compute_gas == gas_used − intrinsic storage gas`. At the top level that covers the entire
//! transaction envelope; in the nested shape it covers the caller's own consumption plus the whole
//! budget it forwarded, because the halting callee returns none of it.
//!
//! `MemoryLimitOOG` is not in the sweep: it needs revm's `memory_limit` cfg, which this workspace
//! does not enable, so no bytecode can reach it.

use crate::common::{
    transact, transact_with_gas_limit, Outcome, CALLEE, CALLER, CONTRACT, ONE_ETH,
};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{
    ADD, CALL, DUP1, JUMP, JUMPDEST, JUMPI, MSTORE, POP, STOP, SUB, SWAP1,
};

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// The two transaction-intrinsic readings every case below calibrates against, measured from a
/// transaction that runs a single `STOP`: the total EVM gas the receipt charges before the frame
/// does anything (`gas`), and the part of it that counts as compute (`compute`).
///
/// The difference is the intrinsic storage gas — charged to EVM gas but excluded from compute
/// accounting. Measuring it keeps the budget identity exact rather than pinned to a constant.
struct Intrinsic {
    gas: u64,
    compute: u64,
}

impl Intrinsic {
    fn measure(spec: MegaSpecId) -> Self {
        let code = BytecodeBuilder::default().append(STOP).build();
        let outcome = transact_with_gas_limit(
            spec,
            base_db(code),
            EvmTxRuntimeLimits::from_spec(spec),
            1_000_000,
        );
        Self { gas: outcome.gas_used, compute: outcome.compute_gas }
    }

    /// The storage gas the receipt carries that compute accounting never sees.
    fn storage_gas(&self) -> u64 {
        self.gas - self.compute
    }
}

/// A countdown loop of cheap plain opcodes, sized to outrun any budget below its own cost.
fn countdown_loop_code(iterations: u16) -> Vec<u8> {
    let mut code = vec![0x61]; // PUSH2
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
    code
}

/// One exceptional-halt shape.
struct HaltCase {
    /// Case name, used in assertion messages.
    name: &'static str,
    /// Bytecode that ends the frame it runs in with an exceptional halt.
    code: Vec<u8>,
    /// The budget the halting frame is given — large enough for the shape to reach its halt with
    /// gas to spare, so the burned remainder is substantial.
    frame_gas: u64,
}

/// Every exceptional-halt classification a plain-opcode segment can produce, other than the
/// `memory_limit` cfg-gated one.
fn halt_cases() -> Vec<HaltCase> {
    vec![
        // Plain out-of-gas: the interpreter zeroes its own counter here, so this is the one shape
        // that already settled its burn before the classification-driven settlement existed.
        HaltCase { name: "plain OOG", code: countdown_loop_code(10_000), frame_gas: 60_000 },
        // Memory out-of-gas: expanding to one MiB costs ~2.2M gas, far past the budget.
        HaltCase {
            name: "memory OOG",
            code: BytecodeBuilder::default()
                .push_number(0u64)
                .push_number(0x10_0000u64)
                .append(MSTORE)
                .append(STOP)
                .build_vec(),
            frame_gas: 60_000,
        },
        // Stack underflow: ADD with nothing on the stack.
        HaltCase {
            name: "stack underflow",
            code: BytecodeBuilder::default().append(ADD).append(STOP).build_vec(),
            frame_gas: 60_000,
        },
        // Stack overflow: each iteration pushes one word and jumps back, so the stack passes 1024
        // long before the budget runs out.
        HaltCase {
            name: "stack overflow",
            code: vec![JUMPDEST, 0x60, 0x01, 0x60, 0x00, JUMP],
            frame_gas: 60_000,
        },
        // Invalid jump: a destination that is not a JUMPDEST.
        HaltCase {
            name: "invalid jump",
            code: BytecodeBuilder::default().push_number(0xffu64).append(JUMP).build_vec(),
            frame_gas: 60_000,
        },
        // Unknown opcode: 0x0c is unassigned on every spec this table covers.
        HaltCase { name: "unknown opcode", code: vec![0x0c], frame_gas: 60_000 },
    ]
}

/// Runs `code` as the transaction's direct target, with exactly `frame_gas` beyond intrinsic.
fn top_level(spec: MegaSpecId, code: &[u8], frame_gas: u64) -> Outcome {
    transact_with_gas_limit(
        spec,
        base_db(Bytes::copy_from_slice(code)),
        EvmTxRuntimeLimits::from_spec(spec),
        Intrinsic::measure(spec).gas + frame_gas,
    )
}

/// Runs `code` in an inner frame that [`CONTRACT`] calls with exactly `frame_gas` forwarded, then
/// pops the failure flag and stops — so the caller survives its callee's halt.
fn nested(spec: MegaSpecId, code: &[u8], frame_gas: u64) -> Outcome {
    let caller = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(frame_gas)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let db = base_db(caller).account_code(CALLEE, Bytes::copy_from_slice(code));
    transact(spec, db, EvmTxRuntimeLimits::from_spec(spec))
}

/// The frame's entire budget must settle as compute gas, in both frame positions, for every
/// exceptional-halt classification.
#[test]
fn test_every_exceptional_halt_settles_its_burned_budget_as_compute() {
    /// Runs one halt shape in one frame position.
    type Runner = fn(MegaSpecId, &[u8], u64) -> Outcome;

    let storage_gas = Intrinsic::measure(MegaSpecId::REX7).storage_gas();
    for case in halt_cases() {
        for (position, run) in [("top-level", top_level as Runner), ("nested", nested as Runner)] {
            let label = format!("{} ({position})", case.name);
            let r6 = run(MegaSpecId::REX6, &case.code, case.frame_gas);
            let r7 = run(MegaSpecId::REX7, &case.code, case.frame_gas);

            assert_eq!(
                format!("{:?}", r6.result),
                format!("{:?}", r7.result),
                "{label}: the halt itself must be unchanged",
            );
            assert_eq!(
                r6.gas_used, r7.gas_used,
                "{label}: receipt gas_used must be unchanged; REX6={} REX7={}",
                r6.gas_used, r7.gas_used
            );
            assert_eq!(
                r7.compute_gas,
                r7.gas_used - storage_gas,
                "{label}: REX7 must settle the whole burned budget as compute; \
                 compute={} gas_used={} intrinsic storage gas={storage_gas}",
                r7.compute_gas,
                r7.gas_used,
            );
            assert!(
                r6.compute_gas < r7.compute_gas,
                "{label}: per-opcode recording attributes neither the failing opcode nor the \
                 burn, so it must report strictly less; REX6={} REX7={}",
                r6.compute_gas,
                r7.compute_gas
            );
        }
    }
}

/// A frame that halts exceptionally while a non-zero gas clamp is outstanding burns the hidden gas
/// too, so the settlement has to cover the true remainder, not just the visible one.
///
/// A tight compute limit keeps a large amount hidden; the frame then hits a stack underflow, which
/// is not a gas shortage at all and must not be reclassified into a compute exceed.
#[test]
fn test_exceptional_halt_under_an_active_clamp_settles_the_hidden_gas_too() {
    let code = BytecodeBuilder::default().append(ADD).append(STOP).build();
    let intrinsic = Intrinsic::measure(MegaSpecId::REX7);
    let limits = |spec| {
        EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(intrinsic.compute + 1_000)
    };
    let gas_limit = intrinsic.gas + 500_000;

    let r6 = transact_with_gas_limit(
        MegaSpecId::REX6,
        base_db(code.clone()),
        limits(MegaSpecId::REX6),
        gas_limit,
    );
    let r7 = transact_with_gas_limit(
        MegaSpecId::REX7,
        base_db(code),
        limits(MegaSpecId::REX7),
        gas_limit,
    );

    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the stack underflow must not be reclassified as a resource-limit exceed",
    );
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas_used must be unchanged");
    assert_eq!(
        r7.compute_gas,
        intrinsic.compute + 500_000,
        "the clamp hides most of the frame's budget, and all of it is still burned",
    );
}

/// The burn settlement must not retroactively fail the transaction.
///
/// The burned budget is whatever the sender's gas envelope allowed, not what the compute limit
/// allowed, so the settlement can push the recorded total past the compute limit. Turning that into
/// a compute-limit halt would rescue gas the EVM already burned and change the receipt, which the
/// exceptional-halt carve-out must not do.
#[test]
fn test_burn_settlement_does_not_retroactively_halt_on_the_compute_limit() {
    let code = BytecodeBuilder::default().append(ADD).append(STOP).build();
    let intrinsic = Intrinsic::measure(MegaSpecId::REX7);
    let compute_limit = intrinsic.compute + 1_000;
    let limits =
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(compute_limit);

    let r7 =
        transact_with_gas_limit(MegaSpecId::REX7, base_db(code), limits, intrinsic.gas + 500_000);

    let reason = format!("{:?}", r7.halt_reason("REX7"));
    assert!(
        reason.contains("StackUnderflow"),
        "the halt must stay the EVM's own, not become a compute-limit exceed; got {reason}",
    );
    assert!(
        r7.compute_gas > compute_limit,
        "the settled burn is expected to exceed the compute limit here; compute={} limit={}",
        r7.compute_gas,
        compute_limit
    );
}
