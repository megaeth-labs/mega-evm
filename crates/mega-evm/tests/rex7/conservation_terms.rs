//! REX7: the terms of the destroyed-remainder conservation law, in combination.
//!
//! A REX7 transaction's destroyed compute gas is not summed from the sites that destroyed it. It
//! is derived once, when the envelope is final, from what the transaction spent:
//!
//! ```text
//! destroyed = spent + minted call stipends − non-compute gas − enforced compute gas
//! ```
//!
//! The rest of the suite exercises the derivation one term at a time — a halted frame, a failing
//! precompile, a sandbox merge. This file exercises the terms **together**, because a term that is
//! individually right can still be wrong in company: a minted stipend that leaked into the
//! destroyed lane, or a second stipend that overwrote the first, would be invisible to any fixture
//! that produces only one of them.
//!
//! Two terms of the law are deliberately not covered here:
//!
//! - **The EIP-8037 reservoir.** The derivation reads `total_gas_spent`, which nets the state-gas
//!   reservoir out of the envelope. Every `MegaEVM` transaction pins the reservoir at zero — the
//!   flag is forced off at configuration time and re-forced inside the transaction, pinned by
//!   `evm::factory`'s `test_embedder_cannot_enable_amsterdam_eip8037` — so a non-zero reservoir is
//!   not a state this spec can reach, and it is not constructed here. If that pin is ever lifted,
//!   the derivation needs revisiting before the reservoir can carry gas.
//! - **A negative derivation.** Reaching it end-to-end would require breaking the conservation law
//!   first, so the guard is driven directly at the seam, in `limit::limit`'s
//!   `test_negative_derivation_is_clamped_to_zero`.

use crate::common::{
    default_envs, transact_default, transact_tx, Outcome, CALLEE, CALLER, CONTRACT, EMPTY_TARGET,
    ONE_ETH,
};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaSpecId, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{ADD, CALL, MSTORE8, POP, STOP},
    context::tx::TxEnvBuilder,
};
use std::vec::Vec;

/// Empty accounts the value-transferring calls pay into. Each one is fresh, so every call really
/// transfers value and really mints a stipend.
const VALUE_TARGETS: [Address; 3] = [
    EMPTY_TARGET,
    address!("0000000000000000000000000000000000300010"),
    address!("0000000000000000000000000000000000300011"),
];

/// Gas operand the envelope-destroying call forwards. Far below 63/64 of what the caller holds, so
/// the child's budget is exactly this number and the destroyed remainder is exactly computable.
const DESTROYING_CHILD_GAS: u64 = 1_000_000;

/// `VERYLOW`, the gas `ADD` charges before it discovers the stack is empty — the only work the
/// destroying child performs.
const ADD_GAS: u64 = 3;

/// What the destroying child leaves behind: its whole budget less the one opcode it paid for.
const EXPECTED_DESTROYED: u64 = DESTROYING_CHILD_GAS - ADD_GAS;

/// `CALL_STIPEND`: what revm mints into a value-transferring call's child frame without debiting
/// the caller.
const CALL_STIPEND: u64 = 2_300;

/// Relayer that sends the keyless-deploy transactions.
const KEYLESS_RELAYER: Address = address!("0000000000000000000000000000000000340004");

fn call_code(builder: BytecodeBuilder, target: Address, value: u64, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// A caller that makes `value_transfers` one-wei calls into fresh empty accounts and then,
/// optionally, one call into a callee that destroys its whole forwarded envelope.
///
/// The caller pops every success flag, so it survives the failing child and returns normally —
/// which keeps the destroyed remainder attributable to the child alone.
fn run_fixture(value_transfers: usize, destroy: bool) -> Outcome {
    let mut builder = BytecodeBuilder::default();
    for target in VALUE_TARGETS.iter().take(value_transfers) {
        builder = call_code(builder, *target, 1, 100_000);
    }
    if destroy {
        builder = call_code(builder, CALLEE, 0, DESTROYING_CHILD_GAS);
    }
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, builder.append(STOP).build())
        .account_balance(CONTRACT, U256::from(ONE_ETH))
        // A bare `ADD`: the child charges `VERYLOW`, finds the stack empty, and halts with its
        // budget intact. A stack underflow is not a gas shortage, so the interpreter keeps its
        // counter and the whole remainder is destroyed rather than already zero.
        .account_code(CALLEE, Bytes::from_static(&[ADD]));
    transact_default(MegaSpecId::REX7, db)
}

/// A transaction that both mints a stipend and destroys an envelope must report exactly the
/// envelope it destroyed.
///
/// The two terms pull the derivation in opposite directions — the mint raises recorded work above
/// what the envelope funded, the halt leaves envelope unspent — and they meet for the first time
/// here. If the mint leaked into the destroyed lane the reported remainder would be one stipend
/// too high; if it were dropped, one stipend too low.
#[test]
fn test_minted_stipend_and_destroyed_envelope_in_one_transaction() {
    let alone = run_fixture(0, true);
    let together = run_fixture(1, true);

    assert!(alone.is_success(), "the caller must survive its failing child: {:?}", alone.result);
    assert!(
        together.is_success(),
        "the caller must survive its failing child: {:?}",
        together.result,
    );

    // Both terms are live, and each is live only where it should be.
    assert_eq!(alone.minted_call_stipend, 0, "no value transfer, no mint");
    assert_eq!(together.minted_call_stipend, CALL_STIPEND, "one value transfer mints one stipend");
    assert!(together.gas_used > alone.gas_used, "the value transfer must really have happened");

    assert_eq!(
        together.destroyed, EXPECTED_DESTROYED,
        "the destroyed remainder is the child's forwarded budget less the one opcode it paid for",
    );
    assert_eq!(
        together.destroyed, alone.destroyed,
        "a minted stipend must not move the destroyed remainder in either direction",
    );
    assert_eq!(
        together.destroyed, together.booked_destroyed,
        "the derived remainder and the per-site bookings must agree",
    );
}

/// A minted stipend on its own destroys nothing.
///
/// This is the other half of the composition: the mint makes recorded compute exceed the envelope,
/// and a derivation that took that overshoot for unspent budget would report it as destroyed.
#[test]
fn test_minted_stipend_alone_destroys_nothing() {
    for transfers in 1..=VALUE_TARGETS.len() {
        let outcome = run_fixture(transfers, false);
        assert!(outcome.is_success(), "{transfers} transfers: {:?}", outcome.result);
        assert_eq!(
            outcome.destroyed, 0,
            "{transfers} transfers: a transaction that never halts destroys nothing",
        );
    }
}

/// Several value transfers in one transaction each mint their own stipend, and the derivation has
/// to account for all of them.
///
/// A term that latched the first mint instead of accumulating would leave the derived remainder
/// short by one stipend per extra call — which the invariance below is exactly sensitive to.
#[test]
fn test_several_minted_stipends_in_one_transaction() {
    for transfers in 1..=VALUE_TARGETS.len() {
        let outcome = run_fixture(transfers, true);

        assert!(outcome.is_success(), "{transfers} transfers: {:?}", outcome.result);
        assert_eq!(
            outcome.minted_call_stipend,
            CALL_STIPEND * transfers as u64,
            "{transfers} transfers: every value-transferring call mints its own stipend",
        );
        assert_eq!(
            outcome.destroyed, EXPECTED_DESTROYED,
            "{transfers} transfers: the destroyed remainder must not drift with the mint count",
        );
        assert_eq!(
            outcome.destroyed, outcome.booked_destroyed,
            "{transfers} transfers: the derived remainder and the per-site bookings must agree",
        );
    }
}

/// A caller with no balance at all, so its value-transferring call cannot be funded.
///
/// Everything else about the fixture matches [`run_fixture`]: the caller pops the call's success
/// flag and returns normally, so the transaction succeeds and the only destroyed remainder is the
/// one the destroying child leaves behind.
fn run_unfunded_fixture(destroy: bool) -> Outcome {
    let mut builder = call_code(BytecodeBuilder::default(), EMPTY_TARGET, 1, 100_000);
    if destroy {
        builder = call_code(builder, CALLEE, 0, DESTROYING_CHILD_GAS);
    }
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, builder.append(STOP).build())
        // No balance for CONTRACT: revm turns the value call away at frame init.
        .account_code(CALLEE, Bytes::from_static(&[ADD]));
    transact_default(MegaSpecId::REX7, db)
}

/// A value-transferring call whose child frame never runs still mints a stipend, and the law needs
/// it booked.
///
/// The stipend is created by the CALL opcode, before the callee is entered: the inherited EVM adds
/// it to the child's budget without debiting the caller. A call that is then turned away at frame
/// init — here for want of balance, equally for exceeding the call depth — hands that whole budget
/// back to the caller, mint included, so the envelope shrinks against recorded work by exactly one
/// stipend, the same way a child that ran and returned it would.
///
/// Booking the mint on "the child frame ran" instead would leave the term short by 2,300 on this
/// shape, which any contract can produce, and the derived remainder short by the same amount:
/// [`EXPECTED_DESTROYED`] − [`CALL_STIPEND`] rather than [`EXPECTED_DESTROYED`].
#[test]
fn test_stipend_is_minted_by_a_value_call_whose_child_frame_never_runs() {
    let alone = run_unfunded_fixture(false);
    let with_destroyed_envelope = run_unfunded_fixture(true);

    assert!(alone.is_success(), "the caller must survive its failed call: {:?}", alone.result);
    assert!(
        with_destroyed_envelope.is_success(),
        "the caller must survive both children: {:?}",
        with_destroyed_envelope.result,
    );

    // The child frame really never ran: an unfunded value call materialises nothing at the target.
    assert!(
        alone.state.get(&EMPTY_TARGET).is_none_or(|account| account.is_empty()),
        "the value call must have been turned away before the target was touched",
    );

    assert_eq!(
        alone.minted_call_stipend, CALL_STIPEND,
        "the mint is created by the CALL opcode, not by the child frame",
    );
    assert_eq!(
        alone.destroyed, 0,
        "a refunded child budget is not a destroyed one: nothing here was thrown away",
    );

    assert_eq!(
        with_destroyed_envelope.minted_call_stipend, CALL_STIPEND,
        "the failed value call still mints, with a destroying sibling alongside it",
    );
    assert_eq!(
        with_destroyed_envelope.destroyed,
        EXPECTED_DESTROYED,
        "only the destroying child's remainder is destroyed; dropping the mint would report {}",
        EXPECTED_DESTROYED - CALL_STIPEND,
    );
    assert_eq!(
        with_destroyed_envelope.destroyed, with_destroyed_envelope.booked_destroyed,
        "the derived remainder and the per-site bookings must agree",
    );
}

/// Builds a deterministic pre-EIP-155 keyless deployment transaction.
fn keyless_tx_bytes(init_code: Bytes, gas_limit: u64) -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(buf)
}

/// Runs one `KeylessDeploy` whose sandbox executes `init_code` with `gas_limit`.
///
/// [`CALLEE`] is preloaded with a bare `ADD` so a constructor can open a sub-frame that halts
/// exceptionally without failing the constructor itself.
fn keyless_deploy(init_code: Bytes, gas_limit: u64) -> Outcome {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes(init_code, gas_limit),
        gasLimitOverride: U256::from(gas_limit),
    }
    .abi_encode();
    let tx = TxEnvBuilder::default()
        .caller(KEYLESS_RELAYER)
        .call(KEYLESS_DEPLOY_ADDRESS)
        .gas_limit(30_000_000)
        .chain_id(Some(1))
        .data(Bytes::from(call_data))
        .build_fill();
    let db = MemoryDatabase::default()
        .account_balance(KEYLESS_RELAYER, U256::from(10 * ONE_ETH))
        .account_code(CALLEE, Bytes::from_static(&[ADD]));
    transact_tx(
        MegaSpecId::REX7,
        db,
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
        tx,
        &default_envs(),
    )
}

/// Set/clear `SSTORE` pairs a constructor runs to earn the EIP-3529 refund.
///
/// Twenty pairs is what it takes to outgrow the sandbox's own storage gas by a clear margin: each
/// pair returns 19,900, and the sandbox pays a flat transaction storage-gas intrinsic plus the
/// per-token storage gas on the initcode it carries.
const REFUND_PAIRS: u64 = 20;

/// Memory offset the refunding constructors touch. The expansion cost is quadratic, so one
/// `MSTORE8` this far out buys millions of gas from a handful of bytecode bytes — enough for
/// EIP-3529's one-fifth-of-gas-spent cap to stop binding on the refunds above.
const REFUND_MEM_OFFSET: u64 = 1_500_000;

/// Sandbox gas limit the refunding deployments run with: above what the memory expansion costs, so
/// the constructor reaches its own end rather than running out.
const REFUND_SANDBOX_GAS: u64 = 8_000_000;

/// The refund-earning prologue: `pairs` set/clear `SSTORE` pairs, then the memory expansion that
/// lifts the refund cap.
fn refunding_prologue(pairs: u64) -> BytecodeBuilder {
    let mut builder = BytecodeBuilder::default();
    for slot in 1..=pairs {
        builder = builder.sstore(U256::from(slot), U256::from(1));
        builder = builder.sstore(U256::from(slot), U256::ZERO);
    }
    builder.push_number(1u64).push_number(REFUND_MEM_OFFSET).append(MSTORE8)
}

/// A constructor that earns `pairs` refunds and then returns, deploying empty code — so no
/// per-byte code-deposit storage gas offsets the refund.
fn refunding_initcode(pairs: u64) -> Bytes {
    refunding_prologue(pairs).return_empty().build()
}

/// The non-compute lane is signed, and this is the shape that needs it.
///
/// The lane is the transaction's `MegaETH` storage gas, except at the `KeylessDeploy` sandbox
/// boundary, where the parent books a **difference**: what the sandbox cost its gas counter, less
/// what the sandbox recorded as compute work. A sandbox whose own EIP-3529 refund outgrows its own
/// storage gas hands that difference over negative, and a large enough refund drives the whole
/// lane below zero. A lane that saturated at zero would then over-report the destroyed remainder
/// by the entire overshoot.
///
/// The control run is the same deployment without the refunds, which leaves the lane positive.
#[test]
fn test_sandbox_refund_drives_the_non_compute_lane_negative() {
    let control = keyless_deploy(refunding_initcode(0), REFUND_SANDBOX_GAS);
    let refunding = keyless_deploy(refunding_initcode(REFUND_PAIRS), REFUND_SANDBOX_GAS);

    assert!(control.is_success(), "the control deployment must run: {:?}", control.result);
    assert!(refunding.is_success(), "the refunding deployment must run: {:?}", refunding.result);
    assert!(
        control.non_compute_gas > 0,
        "the control must leave the lane positive, or it proves nothing; got {}",
        control.non_compute_gas,
    );

    assert!(
        refunding.non_compute_gas < 0,
        "the sandbox's refund must drive the lane negative; got {}",
        refunding.non_compute_gas,
    );
    // The signature of a negative lane: the transaction records more compute work than its
    // envelope ever paid for.
    assert!(
        refunding.compute_gas > refunding.gas_used,
        "recorded compute {} must exceed the envelope {}",
        refunding.compute_gas,
        refunding.gas_used,
    );
    assert_eq!(
        refunding.destroyed, refunding.booked_destroyed,
        "the derivation must stay exact across the sign change",
    );
}

/// The negative lane must compose with a destroyed envelope too.
///
/// The constructor earns its refunds, opens a sub-frame that halts exceptionally, absorbs the
/// failure and returns — so one transaction carries a negative non-compute lane *and* a destroyed
/// remainder that the lane's sign is part of deriving. The constructor has to survive: EIP-3529
/// refunds only reach the receipt of a transaction that succeeds, so a constructor that halted
/// would take the refund — and the negative lane — down with it.
#[test]
fn test_negative_non_compute_lane_composes_with_a_destroyed_envelope() {
    let init_code = call_code(refunding_prologue(REFUND_PAIRS), CALLEE, 0, DESTROYING_CHILD_GAS)
        .return_empty()
        .build();

    let outcome = keyless_deploy(init_code, REFUND_SANDBOX_GAS);

    assert!(outcome.is_success(), "the deployment must succeed: {:?}", outcome.result);
    assert_eq!(
        outcome.destroyed, EXPECTED_DESTROYED,
        "the halted sub-frame's whole forwarded budget, less the one opcode it paid for, must \
         cross the sandbox boundary as destroyed",
    );
    assert!(
        outcome.non_compute_gas < 0,
        "the refunds must still drive the lane negative; got {}",
        outcome.non_compute_gas,
    );
    assert_eq!(
        outcome.destroyed, outcome.booked_destroyed,
        "the derivation must stay exact with both terms live",
    );
}
