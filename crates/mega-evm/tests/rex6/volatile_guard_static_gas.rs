//! Every volatile-guarded opcode costs exactly its EVM price, on every spec.
//!
//! A guarded opcode's static gas is charged in two coupled places: the spec's static gas table
//! stops pre-charging it, and the `volatile_data_ext` wrapper charges it itself once its
//! `disableVolatileDataAccess` guard has declined to fire. Both halves are per-spec — the guarded
//! set grows across Mini-Rex → Rex3 → Rex4 — so either half drifting on one spec silently changes
//! that spec's gas schedule: an opcode zeroed without a guard to charge it becomes free, and a
//! guard whose table entry was left in place charges its opcode twice.
//!
//! Each test measures what *one more occurrence* of a guarded opcode costs, by running two programs
//! that differ by exactly one copy of the opcode and its operand pushes. Differencing cancels the
//! transaction's fixed costs (intrinsic gas, the per-transaction storage gas `MegaETH` charges from
//! Rex onwards) and leaves the opcode's own price, which is pinned against the EVM schedule in both
//! EVM gas and recorded compute gas. Under-charging shows up as a delta below the schedule and a
//! compute reading above it; double-charging as the reverse.
//!
//! The tests live with the newest spec because the table they have to cover is the newest one, and
//! sweep every spec that has a custom instruction table.

use crate::common::{transact_default, CALLER, CONTRACT};
use alloy_primitives::{address, Address, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaSpecId,
};
use revm::bytecode::opcode::*;

/// Every spec whose instruction table is `MegaETH`'s own. `EQUIVALENCE` runs revm's unwrapped
/// mainnet table and is deliberately excluded — it has no guards and no zeroed entries.
const CUSTOM_TABLE_SPECS: [MegaSpecId; 8] = [
    MegaSpecId::MINI_REX,
    MegaSpecId::REX,
    MegaSpecId::REX1,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
];

/// A `STOP` contract with code and balance: an account-read / CALL / `SELFDESTRUCT` target that is
/// neither empty (so no new-account charge applies) nor able to charge storage gas.
const TARGET: Address = address!("0000000000000000000000000000000000200003");

/// Cost of any `PUSHn` the [`BytecodeBuilder`] emits — it never emits `PUSH0`.
const PUSH: u64 = 3;
/// Cost of `POP`.
const POP_GAS: u64 = 2;
/// EIP-2929 warm account / storage access cost.
const WARM: u64 = 100;

/// Runs `code` at [`CONTRACT`] under `spec`, returning `(gas_used, compute_gas)`.
fn run(spec: MegaSpecId, code: BytecodeBuilder, child_code: Option<BytecodeBuilder>) -> (u64, u64) {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000u64))
        .account_code(CONTRACT, code.stop().build())
        .account_code(TARGET, BytecodeBuilder::default().stop().build())
        .account_balance(TARGET, U256::from(1u64));
    if let Some(child_code) = child_code {
        db = db.account_code(CHILD, child_code.build());
    }
    let outcome = transact_default(spec, db);
    assert!(outcome.is_success(), "{spec}: tx should succeed, got {:?}", outcome.result);
    (outcome.gas_used, outcome.compute_gas)
}

/// A frame that runs the opcode under test when it must not be the transaction's own frame
/// (`SELFDESTRUCT` ends its frame, so it cannot simply be repeated).
const CHILD: Address = address!("0000000000000000000000000000000000200004");

/// Asserts that appending one more `fragment` to `prelude` costs exactly `expected` gas, both in
/// EVM gas and in recorded compute gas.
///
/// The prelude already contains one copy, so the measured occurrence acts on warm state — the
/// price pinned below is therefore the warm one.
fn assert_one_more_occurrence_costs(
    expected: u64,
    label: &str,
    specs: &[MegaSpecId],
    fragment: impl Fn(BytecodeBuilder) -> BytecodeBuilder,
) {
    for &spec in specs {
        let once = fragment(BytecodeBuilder::default());
        let twice = fragment(fragment(BytecodeBuilder::default()));

        let (gas_once, compute_once) = run(spec, once, None);
        let (gas_twice, compute_twice) = run(spec, twice, None);

        assert_eq!(
            gas_twice - gas_once,
            expected,
            "{spec} {label}: one more occurrence must cost exactly {expected} EVM gas",
        );
        assert_eq!(
            compute_twice - compute_once,
            expected,
            "{spec} {label}: one more occurrence must record exactly {expected} compute gas",
        );
    }
}

/// `TIMESTAMP` (2 static gas), standing for the unconditionally volatile block-environment reads.
#[test]
fn test_extra_timestamp_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(2 + POP_GAS, "TIMESTAMP", &CUSTOM_TABLE_SPECS, |b| {
        b.append(TIMESTAMP).append(POP)
    });
}

/// `BLOCKHASH` (20 static gas) — the block-environment read whose static gas differs from the
/// others, so a guard charging a single shared constant would fail here.
#[test]
fn test_extra_blockhash_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(PUSH + 20 + POP_GAS, "BLOCKHASH", &CUSTOM_TABLE_SPECS, |b| {
        b.push_number(0_u64).append(BLOCKHASH).append(POP)
    });
}

/// `BALANCE` (100 warm static gas), standing for the beneficiary-conditional account reads.
#[test]
fn test_extra_balance_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(PUSH + WARM + POP_GAS, "BALANCE", &CUSTOM_TABLE_SPECS, |b| {
        b.push_address(TARGET).append(BALANCE).append(POP)
    });
}

/// `EXTCODECOPY` (100 warm static gas) — the account read with the widest operand list, copying
/// zero bytes so nothing but the account access is charged.
#[test]
fn test_extra_extcodecopy_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(
        4 * PUSH + WARM,
        "EXTCODECOPY",
        &CUSTOM_TABLE_SPECS,
        // EXTCODECOPY pops address, then destOffset, offset, len.
        |b| {
            b.push_number(0_u64)
                .push_number(0_u64)
                .push_number(0_u64)
                .push_address(TARGET)
                .append(EXTCODECOPY)
        },
    );
}

/// `SELFBALANCE` (5 static gas), guarded from Rex4.
#[test]
fn test_extra_selfbalance_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(5 + POP_GAS, "SELFBALANCE", &CUSTOM_TABLE_SPECS, |b| {
        b.append(SELFBALANCE).append(POP)
    });
}

/// `SLOAD` (100 warm static gas), guarded from Rex3.
#[test]
fn test_extra_sload_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(PUSH + WARM + POP_GAS, "SLOAD", &CUSTOM_TABLE_SPECS, |b| {
        b.push_number(0_u64).append(SLOAD).append(POP)
    });
}

/// `CALL` (100 warm static gas), guarded from Rex4. Zero value and a `STOP` callee, so the callee
/// burns nothing and every forwarded unit comes back.
#[test]
fn test_extra_call_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(7 * PUSH + WARM + POP_GAS, "CALL", &CUSTOM_TABLE_SPECS, |b| {
        b.push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(0_u64) // argsSize
            .push_number(0_u64) // argsOffset
            .push_number(0_u64) // value
            .push_address(TARGET)
            .push_number(100_000_u64) // gas
            .append(CALL)
            .append(POP)
    });
}

/// `STATICCALL` (100 warm static gas), guarded from Rex4 — one operand fewer than `CALL`, which
/// pins the guard to the right stack shape as well as the right amount.
#[test]
fn test_extra_staticcall_costs_its_static_gas_once() {
    assert_one_more_occurrence_costs(
        6 * PUSH + WARM + POP_GAS,
        "STATICCALL",
        &CUSTOM_TABLE_SPECS,
        |b| {
            b.push_number(0_u64) // retSize
                .push_number(0_u64) // retOffset
                .push_number(0_u64) // argsSize
                .push_number(0_u64) // argsOffset
                .push_address(TARGET)
                .push_number(100_000_u64) // gas
                .append(STATICCALL)
                .append(POP)
        },
    );
}

/// `SELFDESTRUCT` (5,000 static gas — the largest of the guarded opcodes), guarded from Rex4.
///
/// It ends its frame, so it cannot be repeated; instead a child frame runs it and the control runs
/// a `POP` in its place. The child holds no balance and its target is warm and non-empty, so
/// neither the cold surcharge nor the new-account charge applies and the difference is the opcode's
/// static gas alone.
#[test]
fn test_selfdestruct_costs_its_static_gas_once() {
    let parent = |b: BytecodeBuilder| {
        b.push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(0_u64) // argsSize
            .push_number(0_u64) // argsOffset
            .push_number(0_u64) // value
            .push_address(CHILD)
            .push_number(1_000_000_u64) // gas
            .append(CALL)
            .append(POP)
    };
    // Warm the target first so `SELFDESTRUCT` pays no cold surcharge.
    let child_prelude = |b: BytecodeBuilder| {
        b.push_address(TARGET).append(BALANCE).append(POP).push_address(TARGET)
    };

    for spec in CUSTOM_TABLE_SPECS {
        if !spec.is_enabled(MegaSpecId::REX2) {
            // Mini-Rex through Rex1 map SELFDESTRUCT to `control::invalid`.
            continue;
        }
        let selfdestruct_child = child_prelude(BytecodeBuilder::default()).append(SELFDESTRUCT);
        let control_child = child_prelude(BytecodeBuilder::default()).append(POP);

        let (gas_sd, compute_sd) =
            run(spec, parent(BytecodeBuilder::default()), Some(selfdestruct_child));
        let (gas_ctl, compute_ctl) =
            run(spec, parent(BytecodeBuilder::default()), Some(control_child));

        let expected = 5_000 - POP_GAS;
        assert_eq!(
            gas_sd - gas_ctl,
            expected,
            "{spec} SELFDESTRUCT: swapping a POP for a SELFDESTRUCT must cost exactly {expected} \
             EVM gas",
        );
        assert_eq!(
            compute_sd - compute_ctl,
            expected,
            "{spec} SELFDESTRUCT: swapping a POP for a SELFDESTRUCT must record exactly \
             {expected} compute gas",
        );
    }
}
