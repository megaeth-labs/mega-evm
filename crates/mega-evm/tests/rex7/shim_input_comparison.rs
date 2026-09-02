//! What the shim calls a rewrite of a frame's inputs, and what it does not.
//!
//! The comparison the entry callbacks make has to answer one question about an object upstream
//! owns: did this come back describing a different frame? A creation's inputs make that harder
//! than a call's, because two of their fields are `OnceCell` memos — the address the creation will
//! occupy and the hash of its init code — filled on demand through a *shared* reference. So the
//! object a callback was handed comes back structurally different having had a derived value
//! computed off it, and the derived equality read that as an edit.
//!
//! It is not an exotic shape. `created_address` is what a tracer calls to record where a
//! deployment landed, so every `revm-inspectors` tracer did it at every `CREATE`: an undeclared one
//! reported an intervention it never made, and a declared one failed the debug verification at the
//! first creation in the transaction. The fixture here is the one no test had — a transaction that
//! actually creates something.
//!
//! The other half is the cost of narrowing a comparison: a field left out is a field an edit to is
//! invisible. So every field a creation's frame is built from gets a case that edits it and
//! asserts the shim still books it, and the case list is checked against the same table
//! `gas_surface.rs` pins upstream's field set with.

use crate::{
    common::{base_db, transact_inspected, Outcome},
    gas_surface::{semantic_fields, Comparison, CREATE_INPUTS_COMPARISON},
    inspector_common::{ledger_intervention, limits, transact_trusted},
};
use alloy_primitives::{Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DeclaredObserver, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CREATE, CREATE2, MSTORE, POP, STOP},
    context::CreateScheme,
    interpreter::{CreateInputs, CreateOutcome, InterpreterTypes},
    Inspector,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::{vec, vec::Vec};

// --- the fixture ---------------------------------------------------------------------------

/// A second address, for the case that moves the creation's caller.
const OTHER: Address = Address::repeat_byte(0x0C);

/// The salt the fixture's `CREATE2` uses, and the one the scheme-swapping case supplies.
const SALT: u64 = 0x5A17;

/// `PUSH1 0 PUSH1 0 RETURN` — init code that deploys an empty contract.
const RETURN_EMPTY: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xf3];

/// Writes `code` into memory from offset zero, one 32-byte word at a time.
///
/// The tail word is zero-padded, which the `CREATE` that follows never reads: it is given the
/// code's true length.
fn write_to_memory(builder: BytecodeBuilder, code: &[u8]) -> BytecodeBuilder {
    let mut builder = builder;
    for (index, chunk) in code.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        builder = builder.push_bytes(word).push_number((index * 32) as u64).append(MSTORE);
    }
    builder
}

/// Init code that itself creates a contract before returning, so the fixture has a creation
/// nested inside a creation.
fn nested_init_code() -> Vec<u8> {
    write_to_memory(BytecodeBuilder::default(), &RETURN_EMPTY)
        .push_number(RETURN_EMPTY.len() as u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append_many(RETURN_EMPTY)
        .build_vec()
}

/// The fixture: a `CREATE` and a `CREATE2` of init code that creates once more.
///
/// Four `create` callbacks, over both schemes and both frame depths. `CREATE2` is not decoration:
/// its address does not depend on the caller's nonce, which is what makes filling its memo
/// something a test can do without changing where the contract lands.
fn creating_code() -> Bytes {
    let init = nested_init_code();
    let size = init.len() as u64;
    write_to_memory(BytecodeBuilder::default(), &init)
        .push_number(size) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .push_number(SALT) // salt
        .push_number(size) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(POP)
        .append(STOP)
        .build()
}

fn creating_db() -> MemoryDatabase {
    base_db(creating_code())
}

/// Asserts the run really exercised the shape the module is about.
///
/// Counted off the produced state rather than inside an inspector, so that a fixture that stops
/// creating anything fails the tests that rest on it rather than passing them vacuously.
fn assert_created_four(label: &str, outcome: &Outcome) {
    assert!(
        outcome.is_success(),
        "{label}: the fixture must run to completion, got {:?}",
        outcome.result,
    );
    assert_eq!(
        outcome.state.values().filter(|account| account.is_created()).count(),
        4,
        "{label}: the fixture must create four contracts",
    );
}

// --- the inspectors ------------------------------------------------------------------------

/// Fills a creation's memo cells and changes nothing else.
///
/// The address is asked for only under `CREATE2`, where it is derived from the caller, the salt
/// and the init code and the nonce argument is ignored — so this fills the same cell the EVM
/// would have filled, with the same value. Under `CREATE` the address depends on the caller's
/// nonce, which an inspector has to look up to get right; a fill with the wrong one is a rewrite
/// the boundary cannot price, and is left to the declaration the way every other value the
/// boundary cannot read back is.
#[derive(Default)]
struct FillsTheMemo {
    fills: u32,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for FillsTheMemo {
    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        inputs.init_code_hash();
        if matches!(inputs.scheme(), CreateScheme::Create2 { .. }) {
            inputs.created_address(0);
        }
        self.fills += 1;
        None
    }
}

/// Edits one field of the first creation it is handed, and nothing else ever.
///
/// One struct rather than one per field so that what differs between the cases is the edit alone.
struct EditsOneField {
    edit: fn(&mut CreateInputs),
    fired: bool,
}

impl EditsOneField {
    fn new(edit: fn(&mut CreateInputs)) -> Self {
        Self { edit, fired: false }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for EditsOneField {
    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if !self.fired {
            self.fired = true;
            (self.edit)(inputs);
        }
        None
    }
}

/// One case: the field a rewrite moves, and the rewrite.
type Case = (&'static str, fn(&mut CreateInputs));

/// One rewrite per field a creation's frame is built from, each moving what it is named for.
///
/// Every one changes what the frame does: who is recorded as the creator, which address the
/// contract lands at, what it is funded with, what code runs, and what state-gas pool it draws
/// from. The gas limit is deliberately absent — it is booked as an amount on the envelope lane,
/// and [`test_an_edited_gas_limit_is_booked_as_an_amount`] is its case.
const CASES: [Case; 5] = [
    ("caller", |inputs| inputs.set_call(OTHER)),
    ("scheme", |inputs| inputs.set_scheme(CreateScheme::Create2 { salt: U256::from(SALT) })),
    ("value", |inputs| inputs.set_value(U256::from(1))),
    ("init_code", |inputs| inputs.set_init_code(Bytes::from_static(&RETURN_EMPTY))),
    ("reservoir", |inputs| inputs.set_reservoir(1)),
];

// --- an observation-only tracer books nothing ------------------------------------------------

/// ★ A declared tracer runs a transaction that creates contracts without booking anything.
///
/// The shape the narrowed comparison exists for, at the callback it broke at. A declared observer
/// is measured anyway in a debug build and asserted to have booked nothing, so before the fix this
/// panicked at the first `CREATE` — `mega-evme replay --trace` over any transaction that deploys
/// something, and every offline fixture that had one, which is to say none of them.
#[test]
fn test_a_declared_tracer_books_nothing_over_a_transaction_that_creates() {
    let mut tracer = DeclaredObserver(TracingInspector::new(TracingInspectorConfig::all()));
    let outcome = transact_trusted(creating_db(), &mut tracer);

    assert_created_four("declared", &outcome);
    assert!(
        outcome.inspector_ledger.is_zero(),
        "a tracer that only reads must leave every lane empty: {:?}",
        outcome.inspector_ledger,
    );
    assert_eq!(
        outcome.inspector_ledger.interventions, 0,
        "and asking a creation for the address it will occupy is not an intervention",
    );
}

/// ★ And so does the same tracer with no declaration, on the measured path.
///
/// The declared run above is measured too in a debug build, so on its own it says nothing about
/// the release path an embedder drives directly — which is the shape RPC tracing takes, and the
/// one whose outcome carried the false reading to whatever read it.
#[test]
fn test_an_undeclared_tracer_books_nothing_over_the_same_transaction() {
    let mut tracer = TracingInspector::new(TracingInspectorConfig::all());
    let outcome = transact_inspected(MegaSpecId::REX7, creating_db(), limits(), &mut tracer);

    assert_created_four("undeclared", &outcome);
    assert!(
        outcome.inspector_ledger.is_zero(),
        "the measured path must read the same: {:?}",
        outcome.inspector_ledger,
    );
}

/// ★ Filling both memo cells books nothing, and the fixture really fills them.
///
/// The tracer tests above are the shape as it occurs; this is the mechanism on its own, so that a
/// future tracer that stops calling `created_address` does not quietly take the coverage with it.
#[test]
fn test_filling_a_creations_memo_cells_books_nothing() {
    let mut filler = FillsTheMemo::default();
    let outcome = transact_inspected(MegaSpecId::REX7, creating_db(), limits(), &mut filler);

    assert_eq!(filler.fills, 4, "the fixture must hand the inspector four creations");
    assert_created_four("memo filler", &outcome);
    assert!(
        outcome.inspector_ledger.is_zero(),
        "filling a memo is a derived value being computed, not an input being changed: {:?}",
        outcome.inspector_ledger,
    );
}

// --- and every real edit is still booked -------------------------------------------------------

/// ★ Every field a creation's frame is built from is one an edit to is booked.
///
/// The bite of the narrowing. A comparison written field by field is one someone has to keep
/// complete, so each field gets a case that edits it in the `create` callback and asserts exactly
/// one intervention comes back — a field dropped from `create_inputs_rewritten` fails here by
/// name.
#[test]
fn test_every_semantic_field_of_a_creation_is_still_booked_when_edited() {
    for (name, edit) in CASES {
        let mut inspector = EditsOneField::new(edit);
        let outcome = transact_inspected(MegaSpecId::REX7, creating_db(), limits(), &mut inspector);
        assert!(inspector.fired, "{name}: the case must reach a creation");
        assert_eq!(
            outcome.inspector_ledger.interventions, 1,
            "{name}: an edited field must be booked exactly once, got {:?}",
            outcome.inspector_ledger,
        );
    }
}

/// ★ The case list is the table's semantic field set.
///
/// What closes the loop between the three places this is written down. `gas_surface.rs` pins the
/// field set against what upstream's `Debug` renders and classifies each field as semantic,
/// envelope or memo; the comparison in `inspector.rs` is written over the semantic ones; and this
/// is the list of edits that proves each of them is really compared. A field upstream adds has to
/// be classified, and a `Semantic` classification with no case fails here.
#[test]
fn test_the_case_list_is_the_tables_semantic_field_set() {
    let mut cases: Vec<&str> = CASES.iter().map(|(name, _)| *name).collect();
    let declared = cases.len();
    cases.sort_unstable();
    cases.dedup();
    assert_eq!(cases.len(), declared, "no field may be listed twice");

    let mut semantic = semantic_fields(&CREATE_INPUTS_COMPARISON);
    semantic.sort_unstable();
    assert_eq!(cases, semantic, "every semantic field needs a case, and every case a field");

    assert_eq!(
        CREATE_INPUTS_COMPARISON
            .iter()
            .filter(|(_, how)| *how == Comparison::Memo)
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
        vec!["cached_address", "cached_init_code_hash"],
        "the two fields the comparison leaves out are the two memo cells",
    );
}

/// ★ An edited gas limit is booked as an amount, not as an intervention.
///
/// The other field the comparison leaves out, and the reason it is a different kind of exclusion
/// from the memos': it moves the frame's budget, so the envelope lane books how much. Counting it
/// here as well would report one edit twice, and a ledger reading zero on both would be the
/// failure that matters.
#[test]
fn test_an_edited_gas_limit_is_booked_as_an_amount() {
    let mut inspector = EditsOneField::new(|inputs| inputs.set_gas_limit(inputs.gas_limit() - 1));
    let outcome = transact_inspected(MegaSpecId::REX7, creating_db(), limits(), &mut inspector);

    assert!(inspector.fired, "the case must reach a creation");
    assert_eq!(
        outcome.inspector_ledger.interventions, 0,
        "the gas limit is not part of the rewrite comparison",
    );
    assert_eq!(outcome.inspector_ledger.env.net(), -1, "the envelope lane books the amount");
}

/// ★ A memo filled beside a real edit does not hide the edit.
///
/// The two halves of the module in one run: the same callback asks the creation for its address
/// and moves its value, and what comes back is the one intervention the edit deserves.
#[test]
fn test_a_memo_filled_beside_an_edit_still_books_the_edit() {
    let mut inspector = EditsOneField::new(|inputs| {
        inputs.set_value(U256::from(1));
        inputs.init_code_hash();
    });
    let outcome = transact_inspected(MegaSpecId::REX7, creating_db(), limits(), &mut inspector);

    assert!(inspector.fired, "the case must reach a creation");
    assert_eq!(
        outcome.inspector_ledger,
        ledger_intervention(),
        "the edit must be booked, once, and the memo must add nothing to it",
    );
}
