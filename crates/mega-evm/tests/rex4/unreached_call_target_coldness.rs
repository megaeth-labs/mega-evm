//! Tests for the journal entries a CALL-family opcode leaves behind when it halts before revm's
//! body loads its target.
//!
//! `MegaETH` prices a journal entry that is resident but cold as a cold access even for an address
//! the pre-warmed sets cover, so whether a halting CALL materialized an entry decides what every
//! later access to that address costs in the transaction — 2,500 gas for an account. Two sites
//! create such entries ahead of the charges that can halt the opcode: `storage_gas_ext`'s
//! inspection of the account it meters, which runs before revm's body, and (from `Rex6`) the
//! raw-operand delegate resolution inside the body's own load.
//!
//! revm 40 charges this family's static gas before the body and its value-transfer cost before the
//! load, where the deployed implementation charged both later. The handlers therefore recreate the
//! entries at those two exits, and these tests pin the result per opcode and per spec — including
//! the two rows where the deployed implementation left *no* cold entry, which a fix that simply
//! materialized the operand everywhere would get wrong in the other direction.
//!
//! Each case is a `probe(PLAIN) - probe(IDENTITY)` difference measured inside one transaction, so
//! it reports the journal state the halting frame left rather than any absolute gas schedule.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
};

const CALLER: Address = address!("0000000000000000000000000000000000410000");
const OUTER: Address = address!("0000000000000000000000000000000000410001");
const INNER: Address = address!("0000000000000000000000000000000000410002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000410099");

/// The identity precompile, a member of revm's pre-warmed address set: a fresh load of it is warm,
/// so it is the only kind of address whose coldness reveals whether an entry was materialized.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");

/// Absent from the database and from every pre-warmed set, so its first touch is cold however the
/// journal came by its entry. The reference the precompile is measured against.
const PLAIN: Address = address!("00000000000000000000000000000000000000ff");

/// The frame left the precompile's entry resident and cold, which prices exactly like [`PLAIN`].
const LEFT_COLD: u64 = 0;

/// The frame left no cold entry, so the probe took the fresh-entry path and saved
/// `COLD_ACCOUNT_ACCESS_COST - WARM_STORAGE_READ_COST`.
const LEFT_WARM: u64 = 2_500;

/// `INNER`'s budget for the static-gas window: it reaches its call opcode with 59 gas, short of
/// the 100 the wrapper charges before revm's body runs.
const STATIC_CHARGE_BUDGET: u64 = 80;

/// `INNER`'s budget for the value-transfer window: enough for the static charge, short of the
/// 9,000 revm charges for the transfer before it loads the target.
const VALUE_TRANSFER_BUDGET: u64 = 2_000;

/// `INNER`'s budget for the control: its call opcode runs to completion.
const SUCCEEDING_BUDGET: u64 = 100_000;

/// Appends a `CALL` to `target` forwarding `gas` with the given `value` and an empty memory range.
fn append_call(builder: BytecodeBuilder, target: Address, gas: u64, value: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// `INNER` bytecode: one `CALL` to `target` with `value`.
fn inner_call(target: Address, value: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), target, 0, value).build()
}

/// `INNER` bytecode: one `CALLCODE` to `target` with `value`, returning into `ret_size` bytes of
/// memory. `ret_size` sets the cost of the second memory expansion revm's body performs before it
/// loads the target.
fn inner_call_code(target: Address, value: u64, ret_size: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(ret_size)
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(0_u64) // gas
        .append(CALLCODE)
        .build()
}

/// `INNER` bytecode: one valueless six-operand call (`STATICCALL` or `DELEGATECALL`) to `target`.
fn inner_valueless_call(target: Address, opcode: u8) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(target)
        .push_number(0_u64) // gas
        .append(opcode)
        .build()
}

/// `OUTER` bytecode: hand `INNER` `budget` gas, drop whatever it reports, then `EXTCODESIZE(probe)`
/// — an opcode that inspects nothing, so it prices the journal exactly as `INNER` left it.
fn outer_code(budget: u64, probe: Address) -> Bytes {
    append_call(BytecodeBuilder::default(), INNER, budget, 0)
        .append(POP)
        .push_address(probe)
        .append(EXTCODESIZE)
        .append(POP)
        .build()
}

/// Runs one transaction: `CALLER` calls `OUTER`, which gives `INNER` `budget` gas to run
/// `inner_code` and then probes `probe`.
fn gas_used(spec: MegaSpecId, budget: u64, inner_code: Bytes, probe: Address) -> u64 {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_u64))
        .account_code(OUTER, outer_code(budget, probe))
        .account_code(INNER, inner_code);
    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(&mut db, spec).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(OUTER).gas_limit(1_000_000).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    assert!(
        outcome.result.is_success(),
        "{spec:?}: OUTER absorbs INNER's failure, so the probe tx must succeed: {:?}",
        outcome.result
    );
    outcome.result.tx_gas_used()
}

/// How much cheaper probing [`IDENTITY`] is than probing [`PLAIN`] after `INNER` ran `inner_code`
/// with `budget` gas against [`IDENTITY`].
///
/// Everything but the probe's warm/cold surcharge is identical between the two programs, so the
/// difference is [`LEFT_COLD`] when `INNER` left a resident cold entry for the precompile and
/// [`LEFT_WARM`] when it left none (or a warm one).
fn precompile_probe_discount(spec: MegaSpecId, budget: u64, inner_code: Bytes) -> u64 {
    gas_used(spec, budget, inner_code.clone(), PLAIN) - gas_used(spec, budget, inner_code, IDENTITY)
}

/// The three specs whose CALL family runs through the volatile wrapper that charges static gas
/// ahead of revm's body.
const WRAPPED_CALL_SPECS: [MegaSpecId; 3] = [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// `CALL`, `STATICCALL` and `DELEGATECALL` meter against their stack operand on every spec, so the
/// storage-gas inspection materializes exactly the address the opcode was about to load — and a
/// frame too poor for the static charge still leaves that entry resident and cold.
#[test]
fn test_static_charge_window_leaves_operand_cold_for_operand_metered_opcodes() {
    for spec in WRAPPED_CALL_SPECS {
        for (name, code) in [
            ("CALL", inner_call(IDENTITY, 0)),
            ("STATICCALL", inner_valueless_call(IDENTITY, STATICCALL)),
            ("DELEGATECALL", inner_valueless_call(IDENTITY, DELEGATECALL)),
        ] {
            let discount = precompile_probe_discount(spec, STATIC_CHARGE_BUDGET, code);
            assert_eq!(
                discount, LEFT_COLD,
                "{spec:?}: a {name} that cannot afford its static charge must still leave the \
                 precompile resident and cold, got a discount of {discount}",
            );
        }
    }
}

/// `CALLCODE` meters against the current frame from `Rex5`, so from there its storage-gas
/// inspection no longer touches the stack operand. `Rex4` still meters against the operand, and
/// `Rex6` materializes it again through the raw-operand delegate resolution inside revm's load —
/// so the operand's coldness tracks the metered address, not the opcode.
#[test]
fn test_static_charge_window_callcode_tracks_its_metered_address() {
    for (spec, expected) in [
        (MegaSpecId::REX4, LEFT_COLD),
        (MegaSpecId::REX5, LEFT_WARM),
        (MegaSpecId::REX6, LEFT_COLD),
    ] {
        let discount =
            precompile_probe_discount(spec, STATIC_CHARGE_BUDGET, inner_call_code(IDENTITY, 0, 0));
        assert_eq!(
            discount, expected,
            "{spec:?}: a CALLCODE that cannot afford its static charge must leave the precompile \
             with a probe discount of {expected}, got {discount}",
        );
    }
}

/// The same matrix one charge later: the frame affords the static gas, enters revm's body and
/// halts on the value-transfer cost that revm 40 charges before the load.
#[test]
fn test_value_transfer_window_matches_the_static_charge_window() {
    for spec in WRAPPED_CALL_SPECS {
        let discount =
            precompile_probe_discount(spec, VALUE_TRANSFER_BUDGET, inner_call(IDENTITY, 1));
        assert_eq!(
            discount, LEFT_COLD,
            "{spec:?}: a CALL that cannot afford its value-transfer cost must still leave the \
             precompile resident and cold, got a discount of {discount}",
        );
    }

    for (spec, expected) in [
        (MegaSpecId::REX4, LEFT_COLD),
        (MegaSpecId::REX5, LEFT_WARM),
        (MegaSpecId::REX6, LEFT_COLD),
    ] {
        let discount =
            precompile_probe_discount(spec, VALUE_TRANSFER_BUDGET, inner_call_code(IDENTITY, 1, 0));
        assert_eq!(
            discount, expected,
            "{spec:?}: a CALLCODE that cannot afford its value-transfer cost must leave the \
             precompile with a probe discount of {expected}, got {discount}",
        );
    }
}

/// The `Rex6` operand entry is created by revm's load, which the body reaches only after two
/// memory expansions — so a frame that cannot afford them never created it. `INNER` reaches its
/// `CALLCODE` with 59 gas: a 32-byte return range costs 3 gas and a 1,024-byte one costs 98.
///
/// Under debug assertions the unaffordable range comes out cold anyway: the frozen-window
/// tripwire resolves the operand's `EIP-7702` delegation to decide whether to fire, and on `Rex6`
/// that resolution materializes the very entry this gate withholds. That journal side effect is
/// the tripwire's and predates this test, so both profiles are pinned to what they actually do.
#[test]
fn test_static_charge_window_skips_operand_when_memory_expansion_is_unaffordable() {
    let unaffordable = if cfg!(debug_assertions) { LEFT_COLD } else { LEFT_WARM };
    for (ret_size, expected) in [(0x20_u64, LEFT_COLD), (0x400, unaffordable)] {
        let discount = precompile_probe_discount(
            MegaSpecId::REX6,
            STATIC_CHARGE_BUDGET,
            inner_call_code(IDENTITY, 0, ret_size),
        );
        assert_eq!(
            discount, expected,
            "Rex6: a CALLCODE with a {ret_size:#x}-byte return range must leave the precompile \
             with a probe discount of {expected}, got {discount}",
        );
    }
}

/// Control: a call that runs to completion leaves the precompile *warm*, so every case above is
/// reporting the halting frame's own effect and not some property of the measurement.
#[test]
fn test_succeeding_call_leaves_the_precompile_warm() {
    for spec in WRAPPED_CALL_SPECS {
        let discount = precompile_probe_discount(spec, SUCCEEDING_BUDGET, inner_call(IDENTITY, 0));
        assert_eq!(
            discount, LEFT_WARM,
            "{spec:?}: a CALL that completes must leave the precompile warm, got a discount of \
             {discount}",
        );
    }
}
