//! Shared primitive for deploying `MegaETH` system (predeploy) contracts.
//!
//! Every system contract is materialized by the same state patch: set the
//! predeploy account's bytecode (idempotently, keyed on code hash), optionally
//! seed flat storage slots, and mark the account touched (and created on a fresh
//! deploy). The only things that differ per contract are the address, the
//! bytecode (which can vary by spec, e.g. the Oracle's v1.0.0 → v1.1.0 → v2.0.0
//! line), whether an in-place upgrade should still mark the account created, and
//! whether any storage is seeded.
//!
//! [`SystemContractSpec`] captures those differences declaratively and
//! [`transact_deploy`] applies them uniformly, so the per-contract
//! `transact_deploy_*` wrappers and external tooling (foundry, `mega-evme`) share
//! one deployment path instead of re-implementing the pattern.
//!
//! Like the per-contract functions, `transact_deploy` does NOT commit to the
//! database — it returns the [`EvmState`] witness for the caller to commit.

#[cfg(not(feature = "std"))]
use alloc as std;

use alloy_evm::Database;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState, EvmStorageSlot},
};
use std::vec::Vec;

use crate::MegaHardforks;

/// Declarative description of a single system-contract deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemContractSpec {
    /// Predeploy address the bytecode is installed at.
    pub address: Address,
    /// Runtime bytecode to install (already version-selected for the spec).
    pub code: Bytes,
    /// Keccak-256 hash of `code`. Used for the idempotence check.
    pub code_hash: B256,
    /// Flat storage slots to seed `(slot, value)` on a fresh deploy. Empty for
    /// every contract except the `SequencerRegistry`.
    pub seed: Vec<(U256, U256)>,
    /// Whether an in-place bytecode upgrade of an already-existing account should
    /// still mark it `created` (which clears its storage on commit).
    ///
    /// - `false` for the standard contracts (an upgrade preserves storage).
    /// - For the Oracle this is `true` pre-Rex5 and `false` on Rex5+, preserving the canonical
    ///   mainnet state at the Rex2 boundary.
    /// - `true` for the `SequencerRegistry` (always a fresh, seeded deploy).
    pub force_create_on_upgrade: bool,
}

impl SystemContractSpec {
    /// A spec with no seeded storage and standard (storage-preserving) upgrade
    /// semantics.
    pub fn new(address: Address, code: Bytes, code_hash: B256) -> Self {
        Self { address, code, code_hash, seed: Vec::new(), force_create_on_upgrade: false }
    }

    /// Sets the seeded storage slots applied on a fresh deploy.
    pub fn with_seed(mut self, seed: Vec<(U256, U256)>) -> Self {
        self.seed = seed;
        self
    }

    /// Sets whether an in-place upgrade marks the account created.
    pub fn with_force_create_on_upgrade(mut self, force: bool) -> Self {
        self.force_create_on_upgrade = force;
        self
    }
}

/// Deploys a system contract described by `spec`, returning the [`EvmState`]
/// witness (account + any seeded storage). The caller commits it.
///
/// If the contract is already deployed with the matching code hash, the account
/// is returned read-only (marked neither touched nor created, no seeding) so it
/// is recorded in the witness without changing state.
pub fn transact_deploy<DB: Database>(
    db: &mut State<DB>,
    spec: &SystemContractSpec,
) -> Result<EvmState, DB::Error> {
    // The spec's `code_hash` must be the hash of its `code`. The per-contract
    // `*_spec` builders derive both from one bytecode constant, so this only
    // guards against a future hand-built spec installing code under a wrong hash.
    debug_assert_eq!(
        keccak256(spec.code.as_ref()),
        spec.code_hash,
        "SystemContractSpec code_hash does not match code for {:?}",
        spec.address
    );

    let acc = db.load_cache_account(spec.address)?;

    // Already deployed with the correct code — record the read, change nothing.
    let existing_info = acc.account_info();
    if let Some(account_info) = &existing_info {
        if account_info.code_hash == spec.code_hash {
            return Ok(EvmState::from_iter([(
                spec.address,
                Account { info: account_info.clone(), ..Default::default() },
            )]));
        }
    }

    let account_existed = existing_info.is_some();
    let mut acc_info = existing_info.unwrap_or_default();
    acc_info.code_hash = spec.code_hash;
    acc_info.code = Some(Bytecode::new_raw(spec.code.clone()));

    let mut revm_acc: Account = acc_info.into();
    revm_acc.mark_touch();
    // Mark created on a fresh deploy, or when the contract opts to clear storage
    // on an in-place upgrade. Not marking created preserves existing storage.
    if !account_existed || spec.force_create_on_upgrade {
        revm_acc.mark_created();
    }

    for (slot, value) in &spec.seed {
        revm_acc.storage.insert(*slot, EvmStorageSlot::new_changed(U256::ZERO, *value, 0));
    }

    Ok(EvmState::from_iter([(spec.address, revm_acc)]))
}

/// Returns the deploy descriptors for the "flat" system contracts active at the
/// given block timestamp, in deploy order: Oracle, high-precision timestamp
/// Oracle, `KeylessDeploy`, `MegaAccessControl`, `MegaLimitControl`.
///
/// This is the canonical, single-source-of-truth list both the block executor
/// and `mega-evme` iterate to materialize predeploys, instead of each hardcoding
/// the per-contract gate and bytecode selection.
///
/// The `SequencerRegistry` (Rex5) is intentionally excluded: it seeds storage
/// from chain-config parameters, guards against overwriting foreign state, and is
/// paired with a pre-block `applyPendingChanges` system call — see
/// [`transact_deploy_sequencer_registry`](crate::transact_deploy_sequencer_registry).
pub fn flat_system_contract_specs(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
) -> Vec<SystemContractSpec> {
    // Compose the per-contract spec builders (each its own single source of gate
    // + bytecode-version selection). `None` entries (inactive contracts) drop out.
    [
        super::oracle::oracle_spec(&hardforks, block_timestamp),
        super::oracle::high_precision_timestamp_oracle_spec(&hardforks, block_timestamp),
        super::keyless_deploy::keyless_deploy_spec(&hardforks, block_timestamp),
        super::control::access_control_spec(&hardforks, block_timestamp),
        super::limit_control::limit_control_spec(&hardforks, block_timestamp),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MegaHardfork, MegaHardforkConfig, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_HASH,
        ORACLE_CONTRACT_CODE_HASH_REX2, ORACLE_CONTRACT_CODE_HASH_REX5,
    };
    use alloy_hardforks::ForkCondition;

    fn addrs(specs: &[SystemContractSpec]) -> Vec<Address> {
        specs.iter().map(|s| s.address).collect()
    }

    #[test]
    fn test_registry_empty_before_mini_rex() {
        // MiniRex activates at timestamp 100; nothing is active before it.
        let hf = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100));
        assert!(flat_system_contract_specs(&hf, 99).is_empty());
    }

    #[test]
    fn test_registry_grows_with_hardforks_and_picks_oracle_version() {
        let hf = MegaHardforkConfig::default().with_all_activated();
        let specs = flat_system_contract_specs(&hf, 0);
        // Rex5 active → all five flat contracts, in deploy order, Oracle first.
        assert_eq!(specs.len(), 5);
        assert_eq!(specs[0].address, ORACLE_CONTRACT_ADDRESS);
        // Rex5 → Oracle v2.0.0 and NOT force-created on upgrade.
        assert_eq!(specs[0].code_hash, ORACLE_CONTRACT_CODE_HASH_REX5);
        assert!(!specs[0].force_create_on_upgrade);
        // SequencerRegistry is intentionally not in the flat registry.
        assert!(!addrs(&specs).contains(&crate::SEQUENCER_REGISTRY_ADDRESS));
    }

    #[test]
    fn test_registry_oracle_version_by_spec() {
        // Only MiniRex active: Oracle v1.0.0, force-created on upgrade (pre-Rex5),
        // and just Oracle + high-precision Oracle.
        let mini =
            MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
        let mini = flat_system_contract_specs(&mini, 0);
        assert_eq!(mini.len(), 2);
        assert_eq!(mini[0].code_hash, ORACLE_CONTRACT_CODE_HASH);
        assert!(mini[0].force_create_on_upgrade);

        // Rex2 active but not Rex5: Oracle v1.1.0, still force-created on upgrade.
        let rex2 = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
            .with(MegaHardfork::Rex2, ForkCondition::Timestamp(0));
        let rex2 = flat_system_contract_specs(&rex2, 0);
        assert_eq!(rex2[0].code_hash, ORACLE_CONTRACT_CODE_HASH_REX2);
        assert!(rex2[0].force_create_on_upgrade);
    }
}
