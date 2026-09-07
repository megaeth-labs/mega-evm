//! Tests for the EIP-2929 coldness of the first access to a pre-warmed address.
//!
//! A journal answers "is this account cold?" on two different paths. When it has to materialize a
//! fresh entry, the pre-warmed address sets decide, so the first touch of a precompile is warm.
//! When the entry is already resident, the entry's own transaction id / cold flag decides — and a
//! resident-but-cold entry is a cold access even for a precompile.
//!
//! `MegaETH` makes the second path routine from `Rex` on: the CALL-family opcodes run through a
//! wrapper that inspects the target account to price a possible new-account storage-gas charge, and
//! that inspection materializes the journal entry *without* warming it. So from `Rex` on, the first
//! CALL / CALLCODE / DELEGATECALL / STATICCALL to a precompile in a transaction pays the cold
//! surcharge, while `Equivalence` and `MiniRex` — which run the unwrapped mainnet instruction table
//! — pay the warm price. `EXTCODESIZE` and friends never inspect, so they take the
//! fresh-entry path and stay warm on every spec.
//!
//! These tests pin that full matrix. It is easy to break in either direction: dropping the
//! resident-entry rule makes every precompile CALL 2500 gas cheaper, and over-applying it makes
//! `EXTCODESIZE` on a precompile 2500 gas more expensive.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, TxEnv},
};

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const CONTRACT: Address = address!("0000000000000000000000000000000000600001");

/// Identity precompile: pre-warmed, and with empty calldata its execution costs a flat 15 gas, so
/// two touches of it differ by nothing but their warm/cold surcharge.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");

/// A non-precompile address absent from the database — the control target, cold on first touch
/// under every spec.
const PLAIN: Address = address!("00000000000000000000000000000000000000ff");

/// `COLD_ACCOUNT_ACCESS_COST - WARM_STORAGE_READ_COST`: what a cold first touch costs on top of the
/// warm price the interpreter charges for every access.
const COLD_ACCOUNT_SURCHARGE: u64 = 2_500;

/// Every spec `MegaETH` runs. The matrix below is asserted for all of them, so a new spec that
/// changes the instruction table has to state which side of the boundary it lands on.
const ALL_SPECS: [MegaSpecId; 9] = [
    MegaSpecId::EQUIVALENCE,
    MegaSpecId::MINI_REX,
    MegaSpecId::REX,
    MegaSpecId::REX1,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
];

/// Specs whose instruction table is the unwrapped mainnet one, so nothing inspects a CALL's target
/// before the opcode loads it.
const UNWRAPPED_CALL_SPECS: [MegaSpecId; 2] = [MegaSpecId::EQUIVALENCE, MegaSpecId::MINI_REX];

/// `COLD_ACCOUNT_ACCESS_COST`. `SELFDESTRUCT` has no warm baseline to subtract — the whole cold
/// access cost is added to its flat price — so its surcharge is 2600, not
/// [`COLD_ACCOUNT_SURCHARGE`].
const COLD_ACCOUNT_ACCESS_COST: u64 = 2_600;

/// Specs the `SELFDESTRUCT` case is asserted over: `Rex5` introduced the wrapper that inspects the
/// beneficiary, and `Rex4` is the contrast that has no such wrapper. The earlier specs are left out
/// because the probe transaction halts on their tighter per-transaction limits, which would measure
/// the limit rather than the access.
const SELFDESTRUCT_SPECS: [MegaSpecId; 3] = [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

// ============================================================================
// HELPERS
// ============================================================================

fn tx() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill()
}

/// Runs `code` as `CONTRACT` under `spec` and returns the transaction's gas used.
fn gas_used(spec: MegaSpecId, code: Bytes) -> u64 {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx());
    tx.enveloped_tx = Some(Bytes::new());

    let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(
        outcome.result.is_success(),
        "{spec:?}: probe tx must succeed, got {:?}",
        outcome.result
    );
    outcome.result.tx_gas_used()
}

/// `n` back-to-back `STATICCALL`s to `target` with empty calldata and no return data.
fn repeated_static_call(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..n {
        code = code
            .push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(0_u64) // argsSize
            .push_number(0_u64) // argsOffset
            .push_address(target)
            .push_number(10_000_u64) // gas
            .append(STATICCALL)
            .append(POP);
    }
    code.stop().build()
}

/// `n` back-to-back `EXTCODESIZE`s of `target`.
fn repeated_extcodesize(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..n {
        code = code.push_address(target).append(EXTCODESIZE).append(POP);
    }
    code.stop().build()
}

/// One `EXTCODESIZE` of `target` — which materializes its journal entry warm — followed by `n`
/// `STATICCALL`s to it.
fn extcodesize_then_static_calls(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default().push_address(target).append(EXTCODESIZE).append(POP);
    for _ in 0..n {
        code = code
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_address(target)
            .push_number(10_000_u64)
            .append(STATICCALL)
            .append(POP);
    }
    code.stop().build()
}

/// `n` `EXTCODESIZE`s of `target` — each warming it through the fresh-entry path — followed by a
/// `SELFDESTRUCT` that sends the (zero) balance to it.
fn extcodesize_then_selfdestruct(target: Address, n: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..n {
        code = code.push_address(target).append(EXTCODESIZE).append(POP);
    }
    code.push_address(target).append(SELFDESTRUCT).build()
}

/// How much more the first touch of `target` costs than a repeat of the identical opcode sequence.
///
/// Measured by differencing four programs that run the touch 0..=3 times, so every per-touch cost
/// other than the warm/cold surcharge — the operand pushes, the opcode's own static gas, the
/// callee's execution — cancels out. The two repeat touches are cross-checked against each other so
/// a program whose marginal cost is not constant (memory growth, an unintended state change) fails
/// loudly instead of silently skewing the difference.
fn first_touch_surcharge(
    spec: MegaSpecId,
    target: Address,
    program: fn(Address, usize) -> Bytes,
) -> u64 {
    let gas: [u64; 4] = core::array::from_fn(|n| gas_used(spec, program(target, n)));
    assert_eq!(
        gas[2] - gas[1],
        gas[3] - gas[2],
        "{spec:?}: repeat touches of {target} must cost the same, got {gas:?}",
    );
    (gas[1] - gas[0]) - (gas[2] - gas[1])
}

// ============================================================================
// TESTS
// ============================================================================

/// From `Rex` on, the CALL-family target inspection leaves the precompile's journal entry resident
/// but cold, so the opcode's own load pays the cold surcharge. `Equivalence` and `MiniRex` run the
/// unwrapped table, never materialize the entry early, and pay the warm price.
#[test]
fn test_call_family_first_touch_of_precompile_is_cold_from_rex() {
    for spec in ALL_SPECS {
        let surcharge = first_touch_surcharge(spec, IDENTITY, repeated_static_call);
        let expected =
            if UNWRAPPED_CALL_SPECS.contains(&spec) { 0 } else { COLD_ACCOUNT_SURCHARGE };
        assert_eq!(
            surcharge, expected,
            "{spec:?}: first STATICCALL to the identity precompile must cost {expected} more \
             than a repeat, got {surcharge}",
        );
    }
}

/// `EXTCODESIZE` does not inspect its operand, so it always takes the fresh-entry path where the
/// pre-warmed precompile set applies: warm on the first touch under every spec.
#[test]
fn test_extcodesize_first_touch_of_precompile_is_warm_on_every_spec() {
    for spec in ALL_SPECS {
        let surcharge = first_touch_surcharge(spec, IDENTITY, repeated_extcodesize);
        assert_eq!(
            surcharge, 0,
            "{spec:?}: EXTCODESIZE of the identity precompile must cost the same on every touch, \
             got a first-touch surcharge of {surcharge}",
        );
    }
}

/// Control: an ordinary address belongs to no pre-warmed set, so its first touch is cold under
/// every spec and through either opcode. This is what pins the surcharge measured above to the
/// warm/cold difference rather than to some other per-touch cost.
#[test]
fn test_first_touch_of_plain_address_is_cold_on_every_spec() {
    for spec in ALL_SPECS {
        for (program, name) in [
            (repeated_static_call as fn(Address, usize) -> Bytes, "STATICCALL"),
            (repeated_extcodesize as fn(Address, usize) -> Bytes, "EXTCODESIZE"),
        ] {
            let surcharge = first_touch_surcharge(spec, PLAIN, program);
            assert_eq!(
                surcharge, COLD_ACCOUNT_SURCHARGE,
                "{spec:?}: first {name} of a plain address must cost {COLD_ACCOUNT_SURCHARGE} \
                 more than a repeat, got {surcharge}",
            );
        }
    }
}

/// `SELFDESTRUCT` gets the same treatment from `Rex5`, where its wrapper started inspecting the
/// beneficiary to price a possible account creation: the beneficiary's entry is resident but cold
/// by the time revm loads it, so a precompile beneficiary costs the cold access. `Rex4` has no such
/// wrapper and finds the precompile warm.
///
/// Measured by prefixing the `SELFDESTRUCT` with 0, 1 and 2 `EXTCODESIZE`s of the same address. The
/// first `EXTCODESIZE` both adds its own (warm) cost and flips the `SELFDESTRUCT` to warm; the
/// second only adds its cost. The difference between those two steps is therefore exactly what the
/// cold beneficiary access was worth — written as `gas[0] + gas[2] - 2 * gas[1]` because the first
/// step is *negative* whenever the beneficiary access was cold (adding an `EXTCODESIZE` then makes
/// the program cheaper, not dearer).
#[test]
fn test_selfdestruct_to_precompile_is_cold_from_rex5() {
    for spec in SELFDESTRUCT_SPECS {
        let gas: [u64; 3] =
            core::array::from_fn(|n| gas_used(spec, extcodesize_then_selfdestruct(IDENTITY, n)));
        let surcharge = (gas[0] + gas[2]) - 2 * gas[1];
        let expected = if spec == MegaSpecId::REX4 { 0 } else { COLD_ACCOUNT_ACCESS_COST };
        assert_eq!(
            surcharge, expected,
            "{spec:?}: SELFDESTRUCT to the identity precompile must cost {expected} more when it \
             is the first touch, got {surcharge} (gas={gas:?})",
        );
    }
}

/// The resident-entry rule prices the entry, not the address: once an `EXTCODESIZE` has warmed the
/// precompile, a following `STATICCALL` to it costs the same as any repeat touch. Without this the
/// fix would read as "precompile CALLs are always cold", which is not what any spec does.
#[test]
fn test_call_after_extcodesize_finds_the_precompile_warm() {
    for spec in ALL_SPECS {
        let surcharge = first_touch_surcharge(spec, IDENTITY, extcodesize_then_static_calls);
        assert_eq!(
            surcharge, 0,
            "{spec:?}: a STATICCALL that follows an EXTCODESIZE of the same precompile must find \
             it warm, got a surcharge of {surcharge}",
        );
    }
}
