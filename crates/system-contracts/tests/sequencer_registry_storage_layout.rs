//! Layout-equivalence guard for `SequencerRegistry`'s flat storage-slot constants.
//!
//! The Rust slot constants in
//! `mega_system_contracts::sequencer_registry::storage_slots` are hand-maintained and must mirror
//! the *actual* storage layout of `contracts/SequencerRegistry.sol`. Nothing else re-derives them:
//! the bytecode self-consistency test (`tests/generated_self_consistency.rs`) hashes bytecode, not
//! storage layout, and the Solidity `test/SequencerRegistry.t.sol` check is tautological. A future
//! Solidity field reorder + a missed manual constant update would otherwise silently desync.
//!
//! This test closes that gap: it asks Foundry for the canonical layout
//! (`forge inspect SequencerRegistry storageLayout --json`) and asserts that each *imported*
//! constant equals the slot Foundry reports for the corresponding Solidity field. The constants are
//! the single source of truth — the slot numbers are not re-hardcoded here.
//!
//! When `forge` is not on `PATH` (forge-less local `cargo test`), the check skips gracefully. CI
//! has Foundry installed, so the guard is enforced there.

use std::{io::ErrorKind, path::Path, process::Command};

use mega_system_contracts::{alloy_primitives::U256, sequencer_registry::storage_slots};

/// The Solidity field name ↔ Rust slot constant pairs that must agree with Foundry's reported
/// layout. These cover the flat (non-array) fields, i.e. Solidity slots 0..=10.
const FIELD_CONSTANTS: &[(&str, U256)] = &[
    ("_currentSystemAddress", storage_slots::CURRENT_SYSTEM_ADDRESS),
    ("_currentSequencer", storage_slots::CURRENT_SEQUENCER),
    ("_admin", storage_slots::ADMIN),
    ("_pendingAdmin", storage_slots::PENDING_ADMIN),
    ("_initialSystemAddress", storage_slots::INITIAL_SYSTEM_ADDRESS),
    ("_initialSequencer", storage_slots::INITIAL_SEQUENCER),
    ("_initialFromBlock", storage_slots::INITIAL_FROM_BLOCK),
    ("_pendingSystemAddress", storage_slots::PENDING_SYSTEM_ADDRESS),
    ("_systemAddressActivationBlock", storage_slots::SYSTEM_ADDRESS_ACTIVATION_BLOCK),
    ("_pendingSequencer", storage_slots::PENDING_SEQUENCER),
    ("_sequencerActivationBlock", storage_slots::SEQUENCER_ACTIVATION_BLOCK),
];

/// Total number of storage fields Foundry reports for `SequencerRegistry`.
///
/// This is the 11 flat fields in [`FIELD_CONSTANTS`] (slots 0..=10) plus the two dynamic-array
/// history fields `_systemAddressHistory` (slot 11) and `_sequencerHistory` (slot 12). The arrays
/// have no flat slot constant — Rust never reads them directly (see `storage_slots`) — but they are
/// still part of the layout, so counting them lets this test catch an added or removed Solidity
/// field of any kind.
const EXPECTED_FIELD_COUNT: usize = FIELD_CONSTANTS.len() + 2;

#[test]
fn test_sequencer_registry_slot_constants_match_forge_layout() {
    // Repo-only check: the packaged crate ships this test (`tests/` is not excluded) but excludes
    // the Solidity source and Foundry config (`contracts/*.sol`, `foundry.toml`), so `forge
    // inspect` cannot run there. Skip when those inputs are absent — same rationale as the
    // forge-not-found skip below — so a packaged-crate `cargo test` (even with Foundry
    // installed) doesn't fail.
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !manifest_dir.join("contracts/SequencerRegistry.sol").exists() ||
        !manifest_dir.join("foundry.toml").exists()
    {
        eprintln!("skipping: SequencerRegistry.sol / foundry.toml not present (packaged crate)");
        return;
    }

    let output = match Command::new("forge")
        .args(["inspect", "SequencerRegistry", "storageLayout", "--json"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
    {
        Ok(output) => output,
        // `forge` not on PATH: skip rather than fail. CI installs Foundry, so the guard still runs
        // there; this only spares a forge-less local `cargo test`.
        Err(err) if err.kind() == ErrorKind::NotFound => {
            eprintln!("skipping: forge not available");
            return;
        }
        // Any other spawn error is a real failure — fail loudly.
        Err(err) => panic!("failed to spawn `forge inspect`: {err}"),
    };

    assert!(
        output.status.success(),
        "`forge inspect SequencerRegistry storageLayout --json` failed (status {}):\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let layout: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("failed to parse `forge inspect` JSON output");

    let storage =
        layout["storage"].as_array().expect("`forge inspect` JSON has no `storage` array");

    // Build label -> slot (u64). Foundry reports the slot as a decimal string, e.g. "10".
    let mut slots = std::collections::HashMap::<String, u64>::new();
    for entry in storage {
        let label =
            entry["label"].as_str().expect("storage entry missing string `label`").to_string();
        let slot: u64 = entry["slot"]
            .as_str()
            .expect("storage entry missing string `slot`")
            .parse()
            .expect("storage `slot` is not a u64");
        slots.insert(label, slot);
    }

    // A changed field count means a Solidity field was added or removed (including the
    // dynamic-array history fields). Catch it before the per-field assertions so the message is
    // unambiguous.
    assert_eq!(
        storage.len(),
        EXPECTED_FIELD_COUNT,
        "SequencerRegistry storage field count changed: forge reports {} fields, expected {}. \
         A Solidity field was added or removed — update the Rust slot constants in \
         `sequencer_registry::storage_slots` and this test.",
        storage.len(),
        EXPECTED_FIELD_COUNT,
    );

    // Each imported constant must equal the slot Foundry reports for its Solidity field.
    for &(label, rust_slot) in FIELD_CONSTANTS {
        let forge_slot = *slots.get(label).unwrap_or_else(|| {
            panic!("SequencerRegistry storage layout has no field `{label}` (renamed or removed?)")
        });
        assert_eq!(
            rust_slot,
            U256::from(forge_slot),
            "slot mismatch for `{label}`: Rust constant is {rust_slot}, forge reports {forge_slot}",
        );
    }
}
