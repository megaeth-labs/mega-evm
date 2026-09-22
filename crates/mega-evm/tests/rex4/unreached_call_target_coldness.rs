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
//! load, where the deployed implementation charged both later. Moving the static charge ahead of
//! the body also moves it ahead of the body's two memory expansions, so a frame can now fail an
//! expansion the deployed implementation would have paid for and reached the load through. The
//! handlers therefore recreate the entries at those exits, and these tests pin the result per
//! opcode and per spec — including every row where the deployed implementation left *no* cold
//! entry, which a fix that simply materialized the operand everywhere would get wrong in the
//! other direction.
//!
//! Each case is a `probe(PLAIN) - probe(IDENTITY)` difference measured inside one transaction, so
//! it reports the journal state the halting frame left rather than any absolute gas schedule.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    EVMError, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
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

/// What `INNER`'s seven `PUSH`es cost, so a budget can be written as "gas at the opcode + this".
const PUSH_GAS: u64 = 21;

/// `INNER`'s budget for the memory-expansion window: it reaches its `CALLCODE` with 150 gas, pays
/// the 100-gas static charge and is then 48 short of the 98-gas return-range expansion — which it
/// could have paid for before the charge.
const MEMORY_WINDOW_BUDGET: u64 = 150 + PUSH_GAS;

/// The bottom of that window: 100 gas at the opcode, exactly the static charge, nothing left for
/// the expansion.
const MEMORY_WINDOW_LOW_BUDGET: u64 = 100 + PUSH_GAS;

/// The top of it: 197 gas at the opcode, one short of clearing both the charge and the expansion.
const MEMORY_WINDOW_HIGH_BUDGET: u64 = 197 + PUSH_GAS;

/// An account holding an `EIP-7702` delegation to [`DELEGATOR_TARGET`], for the rows that pin a
/// delegated operand leaving its delegate alone.
const DELEGATOR: Address = address!("00000000000000000000000000000000004100aa");

/// An address with no code and no entry, used by the rows that make its load fail.
const UNREACHABLE_TARGET: Address = address!("00000000000000000000000000000000004100bb");

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

/// `INNER` bytecode: one `CALLCODE` to `target` with `value` and the given
/// `[argsOffset, argsSize, retOffset, retSize]` operands, which drive the two memory expansions
/// revm's body performs before it loads the target.
///
/// Seven `PUSH`es at 3 gas each, so `INNER` reaches the opcode with `budget - 21`.
fn inner_call_code_ranges(target: Address, value: u64, ranges: [U256; 4]) -> Bytes {
    let [args_offset, args_size, ret_offset, ret_size] = ranges;
    BytecodeBuilder::default()
        .push_u256(ret_size)
        .push_u256(ret_offset)
        .push_u256(args_size)
        .push_u256(args_offset)
        .push_number(value)
        .push_address(target)
        .push_number(0_u64) // gas
        .append(CALLCODE)
        .build()
}

/// `INNER` bytecode: one `CALLCODE` to `target` with `value`, returning into `ret_size` bytes of
/// memory at offset zero and passing no input.
fn inner_call_code(target: Address, value: u64, ret_size: u64) -> Bytes {
    inner_call_code_ranges(target, value, ret_range(ret_size))
}

/// Memory operands for a call that passes no input and returns into `[0, ret_size)`.
fn ret_range(ret_size: u64) -> [U256; 4] {
    [U256::ZERO, U256::ZERO, U256::ZERO, U256::from(ret_size)]
}

/// The gas one memory expansion to `bytes` costs from an empty memory: `3 * words + words^2 / 512`.
const fn memory_cost(bytes: u64) -> u64 {
    let words = bytes.div_ceil(32);
    3 * words + words * words / 512
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
/// `inner_code` and then probes `probe`. `extra_accounts` seeds any further code the case needs.
fn gas_used(
    spec: MegaSpecId,
    budget: u64,
    inner_code: Bytes,
    probe: Address,
    extra_accounts: &[(Address, Bytes)],
) -> u64 {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_u64))
        .account_code(OUTER, outer_code(budget, probe))
        .account_code(INNER, inner_code);
    for (address, code) in extra_accounts {
        db = db.account_code(*address, code.clone());
    }
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
    probe_discount_with_accounts(spec, budget, inner_code, &[])
}

/// [`precompile_probe_discount`] for a case that needs extra accounts in the database.
fn probe_discount_with_accounts(
    spec: MegaSpecId,
    budget: u64,
    inner_code: Bytes,
    extra_accounts: &[(Address, Bytes)],
) -> u64 {
    gas_used(spec, budget, inner_code.clone(), PLAIN, extra_accounts) -
        gas_used(spec, budget, inner_code, IDENTITY, extra_accounts)
}

/// The verdict for a row whose frame halts at the **static charge** without the deployed
/// implementation having reached the load: it left no entry, so the probe reads [`LEFT_WARM`].
///
/// Except under debug assertions on `Rex6`, where the frozen-window tripwire resolves the
/// operand's `EIP-7702` delegation to decide whether to fire, and that resolution materializes the
/// very entry the row expects to be absent. The side effect is the tripwire's and predates these
/// tests; it reaches only the two exits the tripwire runs on — the static charge and a plain
/// out-of-gas halt — so rows that end on a memory halt are profile-independent.
const fn static_exit_left_warm() -> u64 {
    if cfg!(debug_assertions) {
        LEFT_COLD
    } else {
        LEFT_WARM
    }
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
#[test]
fn test_static_charge_window_skips_operand_when_memory_expansion_is_unaffordable() {
    for (ret_size, expected) in [(0x20_u64, LEFT_COLD), (0x400, static_exit_left_warm())] {
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

/// The static charge moved ahead of the body's memory expansions too, so a frame can now fail an
/// expansion it could have paid for one charge earlier. The deployed implementation ran those
/// expansions with the static gas still in hand and reached the load, so these frames must leave
/// the operand cold even though their own expansion halted.
///
/// A 1,024-byte return range costs 98 gas and the static charge is 100, so every `G` in
/// `[100, 197]` fails the expansion here and cleared it on the deployed implementation.
#[test]
fn test_memory_expansion_window_leaves_operand_cold() {
    assert_eq!(memory_cost(1024), 98);
    for budget in [MEMORY_WINDOW_LOW_BUDGET, MEMORY_WINDOW_BUDGET, MEMORY_WINDOW_HIGH_BUDGET] {
        let discount =
            precompile_probe_discount(MegaSpecId::REX6, budget, inner_call_code(IDENTITY, 0, 1024));
        assert_eq!(
            discount,
            LEFT_COLD,
            "Rex6: a CALLCODE reaching its opcode with {} gas could have paid the 98-gas \
             expansion before the static charge, so it must leave the precompile resident and \
             cold, got a discount of {discount}",
            budget - PUSH_GAS,
        );
    }
}

/// The gate is the budget the frame held **before** the static charge, not after it and not the
/// residue the halting expansion left behind. A 2,048-byte return range costs 200 gas, so the
/// deployed implementation reached the load at exactly `G >= 200` — and every one of these rows
/// halts on the expansion, one gas apart across the boundary.
#[test]
fn test_memory_expansion_window_boundary_is_the_pre_charge_budget() {
    assert_eq!(memory_cost(2048), 200);
    for (gas_at_opcode, expected) in
        [(100_u64, LEFT_WARM), (199, LEFT_WARM), (200, LEFT_COLD), (299, LEFT_COLD)]
    {
        let discount = precompile_probe_discount(
            MegaSpecId::REX6,
            gas_at_opcode + PUSH_GAS,
            inner_call_code(IDENTITY, 0, 2048),
        );
        assert_eq!(
            discount, expected,
            "Rex6: a CALLCODE reaching its opcode with {gas_at_opcode} gas against a 200-gas \
             expansion must leave the precompile with a probe discount of {expected}, got \
             {discount}",
        );
    }
}

/// The input range is expanded first, so a frame can also halt on *that* expansion — and the
/// operand's fate is decided the same way. Same 98-gas expansion as
/// [`test_memory_expansion_window_leaves_operand_cold`], moved to the other operand pair.
#[test]
fn test_memory_expansion_window_covers_the_input_range() {
    let args_range = [U256::ZERO, U256::from(1024), U256::ZERO, U256::ZERO];
    for (gas_at_opcode, expected) in
        [(97_u64, static_exit_left_warm()), (98, LEFT_COLD), (150, LEFT_COLD)]
    {
        let discount = probe_discount_with_accounts(
            MegaSpecId::REX6,
            gas_at_opcode + PUSH_GAS,
            inner_call_code_ranges(IDENTITY, 0, args_range),
            &[],
        );
        assert_eq!(
            discount, expected,
            "Rex6: a CALLCODE reaching its opcode with {gas_at_opcode} gas against a 98-gas input \
             expansion must leave the precompile with a probe discount of {expected}, got \
             {discount}",
        );
    }
}

/// Two expansions cost what reaching their high-water mark costs, not the sum of what each costs
/// from an empty memory: 1,024 bytes of input then 1,024 more of output is 64 words, 200 gas —
/// not 98 + 200. A frame holding exactly 200 therefore reached the load on the deployed
/// implementation, and one holding 199 did not.
#[test]
fn test_memory_expansion_window_uses_the_high_water_mark_of_both_ranges() {
    let both_ranges = [U256::ZERO, U256::from(1024), U256::from(1024), U256::from(1024)];
    for (gas_at_opcode, expected) in [(199_u64, LEFT_WARM), (200, LEFT_COLD)] {
        let discount = precompile_probe_discount(
            MegaSpecId::REX6,
            gas_at_opcode + PUSH_GAS,
            inner_call_code_ranges(IDENTITY, 0, both_ranges),
        );
        assert_eq!(
            discount, expected,
            "Rex6: a CALLCODE reaching its opcode with {gas_at_opcode} gas against a 200-gas \
             high-water expansion must leave the precompile with a probe discount of {expected}, \
             got {discount}",
        );
    }
}

/// An operand too large for `usize` halts the body ahead of its load on the deployed
/// implementation too — its `as_usize_or_fail!` is the same one — so no entry is created however
/// much gas the frame is holding.
#[test]
fn test_oversized_memory_operand_leaves_the_operand_alone() {
    let oversized = [U256::ZERO, U256::ZERO, U256::ZERO, U256::from(u64::MAX) + U256::from(1)];
    for budget in [MEMORY_WINDOW_BUDGET, SUCCEEDING_BUDGET] {
        let discount = precompile_probe_discount(
            MegaSpecId::REX6,
            budget,
            inner_call_code_ranges(IDENTITY, 0, oversized),
        );
        assert_eq!(
            discount, LEFT_WARM,
            "Rex6: a CALLCODE with an oversized return length must leave the precompile alone at \
             a budget of {budget}, got a discount of {discount}",
        );
    }
}

/// A zero-length range neither reads its offset nor touches memory, so an enormous offset paired
/// with a zero length costs nothing and the frame reaches the load exactly as it would with an
/// empty range.
#[test]
fn test_zero_length_range_ignores_its_offset() {
    let huge_offset = [U256::ZERO, U256::ZERO, U256::from(u64::MAX) + U256::from(1), U256::ZERO];
    for (budget, expected) in [
        (STATIC_CHARGE_BUDGET, LEFT_COLD),
        (100 + PUSH_GAS, LEFT_COLD),
        (SUCCEEDING_BUDGET, LEFT_WARM),
    ] {
        let discount = precompile_probe_discount(
            MegaSpecId::REX6,
            budget,
            inner_call_code_ranges(IDENTITY, 0, huge_offset),
        );
        assert_eq!(
            discount, expected,
            "Rex6: a CALLCODE with a zero-length return range at a huge offset must leave the \
             precompile with a probe discount of {expected} at a budget of {budget}, got \
             {discount}",
        );
    }
}

/// The address whose entry gets recreated is the raw stack operand, never the account it delegates
/// to. `CALLCODE` from `Rex5` meters against the current frame, and the storage-gas wrapper
/// inspects without following delegation, so a delegator operand leaves its delegate's entry
/// untouched on the deployed implementation — and must keep leaving it untouched here.
/// Over-materializing is as wrong as under-materializing.
#[test]
fn test_delegated_callcode_operand_leaves_the_delegate_alone() {
    // Delegating to the probed precompile is what makes the probe report whether the delegate
    // hop materialized anything.
    let mut delegation_to_identity = vec![0xef, 0x01, 0x00];
    delegation_to_identity.extend_from_slice(IDENTITY.as_slice());
    let accounts = [(DELEGATOR, Bytes::from(delegation_to_identity))];

    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        for (budget, value) in [(STATIC_CHARGE_BUDGET, 0), (VALUE_TRANSFER_BUDGET, 1)] {
            let discount = probe_discount_with_accounts(
                spec,
                budget,
                inner_call_code(DELEGATOR, value, 0),
                &accounts,
            );
            assert_eq!(
                discount, LEFT_WARM,
                "{spec:?}: a CALLCODE to a delegator must leave the delegate's entry alone at a \
                 budget of {budget}, got a discount of {discount}",
            );
        }
    }
}

/// A stack too short for the memory operands underflows in revm's body ahead of its load, on both
/// implementations. The recreation must read that off the stack rather than assume the operands
/// are there — at the static charge, where the body has not run, and after it, where the body
/// raised the underflow itself.
#[test]
fn test_partial_call_stack_materializes_nothing_extra() {
    // Five operands: enough for the body's first `popn` of three, one short of the four memory
    // operands it pops next.
    let truncated = BytecodeBuilder::default()
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(IDENTITY)
        .push_number(0_u64) // gas
        .append(CALLCODE)
        .build();

    for (budget, expected) in
        [(STATIC_CHARGE_BUDGET, static_exit_left_warm()), (SUCCEEDING_BUDGET, LEFT_WARM)]
    {
        let discount = precompile_probe_discount(MegaSpecId::REX6, budget, truncated.clone());
        assert_eq!(
            discount, expected,
            "Rex6: a CALLCODE whose stack is one operand short must leave the precompile with a \
             probe discount of {expected} at a budget of {budget}, got {discount}",
        );
    }
}

/// Control: a call that runs to completion leaves the precompile *warm*, so every case above is
/// reporting the halting frame's own effect and not some property of the measurement.
#[test]
fn test_succeeding_call_leaves_the_precompile_warm() {
    for spec in WRAPPED_CALL_SPECS {
        for (name, code) in [
            ("CALL", inner_call(IDENTITY, 0)),
            ("CALLCODE", inner_call_code(IDENTITY, 0, 0)),
            ("STATICCALL", inner_valueless_call(IDENTITY, STATICCALL)),
            ("DELEGATECALL", inner_valueless_call(IDENTITY, DELEGATECALL)),
        ] {
            let discount = precompile_probe_discount(spec, SUCCEEDING_BUDGET, code);
            assert_eq!(
                discount, LEFT_WARM,
                "{spec:?}: a {name} that completes must leave the precompile warm, got a discount \
                 of {discount}",
            );
        }
    }
}

/// Runs the probe transaction against a database that fails every `basic()` for
/// [`UNREACHABLE_TARGET`], and reports the error the EVM surfaced.
fn transact_with_failing_target(budget: u64, inner_code: Bytes) -> String {
    let inner_db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_u64))
        .account_code(OUTER, outer_code(budget, PLAIN))
        .account_code(INNER, inner_code);
    let mut db = ErrorInjectingDatabase::new(inner_db);
    db.fail_on_account = Some(UNREACHABLE_TARGET);

    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(OUTER).gas_limit(1_000_000).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    match alloy_evm::Evm::transact_raw(&mut evm, tx) {
        Err(EVMError::Custom(message)) => message,
        Err(other) => panic!("expected EVMError::Custom, got {other:?}"),
        Ok(outcome) => panic!("expected a fatal external error, got {:?}", outcome.result),
    }
}

/// Each exit recreates the operand's entry by asking the host to load it, and that load can fail.
/// A database error there has to surface as a fatal external error rather than be swallowed into a
/// silently unrecreated entry — which would be the divergence this whole file is about, only
/// quieter.
#[test]
fn test_db_error_recreating_the_operand_is_fatal_at_every_exit() {
    for (exit, budget, inner_code) in [
        ("static charge", STATIC_CHARGE_BUDGET, inner_call_code(UNREACHABLE_TARGET, 0, 0)),
        ("memory expansion", MEMORY_WINDOW_BUDGET, inner_call_code(UNREACHABLE_TARGET, 0, 1024)),
        ("value transfer", VALUE_TRANSFER_BUDGET, inner_call_code(UNREACHABLE_TARGET, 1, 0)),
    ] {
        let message = transact_with_failing_target(budget, inner_code);
        assert!(
            message.contains("injected basic()"),
            "Rex6: the {exit} exit must surface the database error that stopped it from \
             recreating the operand's entry, got: {message}",
        );
    }
}
