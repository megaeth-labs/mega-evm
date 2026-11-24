//! Compares the Rust flat slot constants against the compiled Solidity layout.

use std::{collections::HashMap, path::Path, process::Command};

use alloy_primitives::U256;
use anyhow::{ensure, Context, Result};
use serde::Deserialize;

#[path = "../../../crates/system-contracts/src/sequencer_registry/storage_slots.rs"]
#[expect(unreachable_pub, reason = "The shared constants are public in mega-system-contracts")]
mod storage_slots;

/// The Solidity field name ↔ Rust slot constant pairs that must agree with Foundry's reported
/// layout. These cover the flat (non-array) fields: Solidity slots 0..=10 plus the v2.0.0
/// `_minRotationDelay` at slot 13.
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
    ("_minRotationDelay", storage_slots::MIN_ROTATION_DELAY),
];

/// Total number of storage fields Foundry reports for `SequencerRegistry`.
///
/// This is the 12 flat fields in [`FIELD_CONSTANTS`] (slots 0..=10 and 13) plus the two
/// dynamic-array history fields `_systemAddressHistory` (slot 11) and `_sequencerHistory`
/// (slot 12). The arrays have no flat slot constant — Rust never reads them directly (see
/// `storage_slots`) — but they are still part of the layout, so counting them lets the check
/// catch an added or removed Solidity field of any kind.
const EXPECTED_FIELD_COUNT: usize = FIELD_CONSTANTS.len() + 2;

#[derive(Deserialize)]
struct StorageLayout {
    storage: Vec<StorageField>,
}

#[derive(Deserialize)]
struct StorageField {
    label: String,
    slot: String,
}

pub(super) fn attest(project: &Path) -> Result<()> {
    let output = Command::new("forge")
        .args(["inspect", "SequencerRegistry", "storageLayout", "--json"])
        .current_dir(project)
        .output()
        .context("Could not execute forge inspect for SequencerRegistry")?;
    ensure!(
        output.status.success(),
        "Could not inspect SequencerRegistry storage layout ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let layout: StorageLayout = serde_json::from_slice(&output.stdout)
        .context("Could not decode SequencerRegistry storage layout")?;
    ensure!(
        layout.storage.len() == EXPECTED_FIELD_COUNT,
        "SequencerRegistry storage field count changed: Foundry reports {}, expected {}",
        layout.storage.len(),
        EXPECTED_FIELD_COUNT,
    );

    let mut slots = HashMap::new();
    for field in &layout.storage {
        let slot: u64 = field.slot.parse().with_context(|| {
            format!("Invalid SequencerRegistry storage slot for `{}`", field.label)
        })?;
        ensure!(
            slots.insert(field.label.as_str(), slot).is_none(),
            "Duplicate SequencerRegistry storage field `{}`",
            field.label,
        );
    }
    for &(label, rust_slot) in FIELD_CONSTANTS {
        let forge_slot = slots
            .get(label)
            .with_context(|| format!("SequencerRegistry storage layout has no field `{label}`"))?;
        ensure!(
            U256::from(*forge_slot) == rust_slot,
            "SequencerRegistry slot mismatch for `{label}`: Rust constant is {rust_slot}, Foundry reports {forge_slot}",
        );
    }

    println!("Checked SequencerRegistry storage layout ({} fields)", layout.storage.len());
    Ok(())
}
