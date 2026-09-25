//! The `SequencerRegistry` system contract.
//!
//! It holds the two rotating roles of the chain — the system address and the sequencer — and
//! their change schedules. It runs its own bytecode: no method of it is intercepted, so a call
//! to it is an ordinary call. Its Solidity source is
//! `crates/system-contracts/contracts/SequencerRegistry.sol`.
//!
//! The contract is deployed by [`transact_deploy`](crate::system::transact_deploy) with the
//! bootstrap slots [`SequencerRegistryConfig`] names, before every block. A role change the
//! registry schedules takes effect at its activation block through a pre-block call: the block
//! executor reads whether one is due ([`is_apply_pending_changes_due`]) and, when one is, calls
//! `applyPendingChanges()` ([`transact_apply_pending_changes`]). The call is permissionless and
//! applies only what is due, so a block that makes it applies the same changes whoever calls.
//! Then it reads the live system address out of the registry ([`resolve_system_address`]), which
//! the block's system-address transactions must come from.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::format;

use alloy_evm::block::BlockExecutionError;
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use op_revm::OpHaltReason;
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        Block, ContextTr,
    },
    state::{Account, EvmState, EvmStorageSlot, TransactionId},
};

use super::MEGA_SYSTEM_ADDRESS;
use crate::{
    pre_block_call_gas_limit, ExternalEnvTypes, HardforkParams, HardforkParamsError,
    MegaBlockExecutionError, MegaEvm, MegaHardfork,
};
use storage_slots::{
    CURRENT_SYSTEM_ADDRESS, PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS, SEQUENCER_ACTIVATION_BLOCK,
    SYSTEM_ADDRESS_ACTIVATION_BLOCK,
};

/// The address of the `SequencerRegistry` system contract.
pub const SEQUENCER_REGISTRY_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000006");

/// The code of the `SequencerRegistry` contract.
pub use mega_system_contracts::sequencer_registry::LATEST_CODE as SEQUENCER_REGISTRY_CODE;

/// The code hash of the `SequencerRegistry` contract.
pub use mega_system_contracts::sequencer_registry::LATEST_CODE_HASH as SEQUENCER_REGISTRY_CODE_HASH;

pub use mega_system_contracts::sequencer_registry::{storage_slots, ISequencerRegistry};

/// Delay the unknown-chain placeholder seeds, in blocks.
///
/// A zero delay would disable the reaction window the field exists to guarantee. Ten matches
/// the contract's own tests: long enough that a rotation cannot activate in the same block.
pub const PLACEHOLDER_MIN_ROTATION_DELAY: u64 = 10;

/// Bootstrap configuration for the `SequencerRegistry`, attached to Satin via [`HardforkParams`].
///
/// These values seed the registry's storage on a fresh deploy. After that the live roles are
/// whatever the contract holds, rotated by the `applyPendingChanges()` pre-block call. The contract
/// has no setter for `_minRotationDelay`, so a matching-code registry cannot be repaired later
/// through this helper: the delay must be seeded here, and it must not be zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerRegistryConfig {
    /// Seeded into `_currentSystemAddress` and `_initialSystemAddress`.
    pub initial_system_address: Address,
    /// Seeded into `_currentSequencer` and `_initialSequencer`.
    pub initial_sequencer: Address,
    /// Seeded into `_admin`.
    pub initial_admin: Address,
    /// Seeded into `_initialFromBlock`: the first block at which historical lookups are valid.
    pub initial_from_block: u64,
    /// Seeded into `_minRotationDelay`. Must be nonzero: a zero delay disables the reaction
    /// window the field exists to guarantee.
    pub min_rotation_delay: u64,
}

impl SequencerRegistryConfig {
    /// Placeholder roles for a chain that has none published: [`MEGA_SYSTEM_ADDRESS`] for every
    /// address, block zero, and [`PLACEHOLDER_MIN_ROTATION_DELAY`].
    ///
    /// The unknown-chain fallback schedule uses this so a local chain can start. A real network
    /// attaches the roles governance chose.
    pub const fn placeholder() -> Self {
        Self {
            initial_system_address: MEGA_SYSTEM_ADDRESS,
            initial_sequencer: MEGA_SYSTEM_ADDRESS,
            initial_admin: MEGA_SYSTEM_ADDRESS,
            initial_from_block: 0,
            min_rotation_delay: PLACEHOLDER_MIN_ROTATION_DELAY,
        }
    }
}

impl HardforkParams for SequencerRegistryConfig {
    const FORK: MegaHardfork = MegaHardfork::Satin;
    const NAME: &'static str = "SequencerRegistryConfig";

    fn validate(&self) -> Result<(), HardforkParamsError> {
        if self.initial_system_address.is_zero() {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.initial_system_address must not be zero".into(),
            });
        }
        if self.initial_sequencer.is_zero() {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.initial_sequencer must not be zero".into(),
            });
        }
        if self.initial_admin.is_zero() {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.initial_admin must not be zero".into(),
            });
        }
        if self.min_rotation_delay == 0 {
            return Err(HardforkParamsError {
                message: "SequencerRegistryConfig.min_rotation_delay must not be zero".into(),
            });
        }
        Ok(())
    }
}

/// Whether a role change the registry holds is due at `block_number`: a pending system address
/// or sequencer whose activation block has come.
///
/// Also answers the reads that decided it, as read-only entries of the registry's account: the
/// witness a stateless client replays the decision from, which the caller hands on whether or
/// not a change is due. Both roles' pending slots are read, and a role's activation block only
/// when it has a change pending. A registry that does not exist has nothing due, and its absence
/// is what the witness records.
///
/// # Errors
///
/// The database's, when the registry's account or one of its slots cannot be read.
pub fn is_apply_pending_changes_due<DB: revm::Database>(
    db: &mut DB,
    block_number: u64,
) -> Result<(bool, EvmState), DB::Error> {
    let Some(info) = db.basic(SEQUENCER_REGISTRY_ADDRESS)? else {
        let absent = Account::new_not_existing(TransactionId::ZERO);
        return Ok((false, EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, absent)])));
    };
    let mut registry = Account::from(info);
    let mut due = false;
    for (pending, activation) in [
        (PENDING_SYSTEM_ADDRESS, SYSTEM_ADDRESS_ACTIVATION_BLOCK),
        (PENDING_SEQUENCER, SEQUENCER_ACTIVATION_BLOCK),
    ] {
        due |= is_role_due(db, &mut registry, pending, activation, block_number)?;
    }
    Ok((due, EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, registry)])))
}

/// Whether the role whose pending value lives at `pending` is due at `block_number`, recording
/// each slot it reads into `registry` as a read-only entry.
fn is_role_due<DB: revm::Database>(
    db: &mut DB,
    registry: &mut Account,
    pending: U256,
    activation: U256,
    block_number: u64,
) -> Result<bool, DB::Error> {
    let pending_value = read_slot(db, registry, pending)?;
    if pending_value.is_zero() {
        return Ok(false);
    }
    let activation_block = read_slot(db, registry, activation)?;
    Ok(U256::from(block_number) >= activation_block)
}

/// Reads the registry's `slot` and records it into `registry` without marking it changed.
fn read_slot<DB: revm::Database>(
    db: &mut DB,
    registry: &mut Account,
    slot: U256,
) -> Result<U256, DB::Error> {
    let value = db.storage(SEQUENCER_REGISTRY_ADDRESS, slot)?;
    registry.storage.insert(slot, EvmStorageSlot::new(value, TransactionId::ZERO));
    Ok(value)
}

/// Runs the pre-block `applyPendingChanges()` call on the registry and answers its result and
/// state. Nothing is committed.
///
/// It is a system call from the EIP-4788 system address on the pre-block budget
/// ([`pre_block_call_gas_limit`]): at most 30M of regular gas, the rest reservoir, its state
/// priced at the minimum SALT bucket. One call applies whichever of the two roles is due; the
/// block executor makes it only when [`is_apply_pending_changes_due`] says one is.
///
/// # Errors
///
/// [`MegaBlockExecutionError::ApplyPendingChangesFailed`] when the call reverts or halts, or an
/// error other than the database's stops it: a change the registry scheduled for this block must
/// be applied in it. A database error is the database's.
pub fn transact_apply_pending_changes<DB, INSP, ExtEnvs>(
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<ResultAndState<OpHaltReason>, BlockExecutionError>
where
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
{
    let calldata = ISequencerRegistry::applyPendingChangesCall {}.abi_encode();
    let gas_limit = pre_block_call_gas_limit(evm.ctx().block().gas_limit());
    let failed = |message| MegaBlockExecutionError::ApplyPendingChangesFailed { message };
    let outcome = match evm.transact_system_call_with_gas_limit(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
        Bytes::from(calldata),
        gas_limit,
    ) {
        Ok(outcome) => outcome,
        // A read the database could not serve says nothing about the block, as for the registry
        // reads on either side of the call.
        Err(EVMError::Database(error)) => return Err(BlockExecutionError::other(error)),
        Err(error) => return Err(failed(format!("{error}")).into()),
    };
    if !outcome.result.is_success() {
        return Err(failed(format!("{:?}", outcome.result)).into());
    }
    Ok(outcome)
}

/// Reads the live system address out of the registry: the address a system-address transaction
/// must be sent from in the block. Also answers the read, as a read-only entry of the registry's
/// account, which the caller hands on as a witness.
///
/// It is read once the block's pre-block changes are committed — the registry deployed and a due
/// rotation applied — so a rotation takes effect at its activation block.
///
/// # Errors
///
/// The deploy before it guarantees what is checked here, so each of these is a state no Satin
/// block leaves behind, and the block is refused rather than run against the wrong authority:
/// [`MegaBlockExecutionError::MissingSequencerRegistry`] when the registry has no account,
/// [`MegaBlockExecutionError::SequencerRegistryCodeMismatch`] when it holds other code than
/// [`SEQUENCER_REGISTRY_CODE`], and [`MegaBlockExecutionError::ZeroSystemAddress`] when its
/// `_currentSystemAddress` is zero. A database error is the database's.
pub fn resolve_system_address<DB: alloy_evm::Database>(
    db: &mut DB,
) -> Result<(Address, EvmState), BlockExecutionError> {
    let info = db
        .basic(SEQUENCER_REGISTRY_ADDRESS)
        .map_err(BlockExecutionError::other)?
        .ok_or(MegaBlockExecutionError::MissingSequencerRegistry)?;
    if info.code_hash != SEQUENCER_REGISTRY_CODE_HASH {
        return Err(MegaBlockExecutionError::SequencerRegistryCodeMismatch {
            expected: SEQUENCER_REGISTRY_CODE_HASH,
            found: info.code_hash,
        }
        .into());
    }
    let mut registry = Account::from(info);
    let current =
        read_slot(db, &mut registry, CURRENT_SYSTEM_ADDRESS).map_err(BlockExecutionError::other)?;
    if current.is_zero() {
        return Err(MegaBlockExecutionError::ZeroSystemAddress.into());
    }
    let address = Address::from_word(current.into());
    Ok((address, EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, registry)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MegaContext, MegaSpecId};
    use alloy_primitives::address;
    use revm::{
        context::{BlockEnv, Transaction},
        database::{InMemoryDB, State},
        handler::SYSTEM_CALL_REGULAR_GAS_LIMIT,
        state::{AccountInfo, Bytecode},
        Database, DatabaseCommit,
    };
    use storage_slots::CURRENT_SEQUENCER;

    const NEXT_SYSTEM_ADDRESS: Address = address!("0x1111111111111111111111111111111111111111");
    const NEXT_SEQUENCER: Address = address!("0x2222222222222222222222222222222222222222");
    const CURRENT_SEQUENCER_ADDRESS: Address =
        address!("0xBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");

    fn word(address: Address) -> U256 {
        U256::from_be_bytes(address.into_word().0)
    }

    /// A database holding the registry's code and `slots`.
    fn registry_with(slots: &[(U256, U256)]) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        for &(slot, value) in slots {
            db.insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, slot, value).unwrap();
        }
        db
    }

    /// Asks whether a change is due at `block_number`, over a fresh cache of `db`.
    fn due_at(db: &mut InMemoryDB, block_number: u64) -> (bool, Account) {
        let mut state = State::builder().with_database(db).build();
        let (due, witness) = is_apply_pending_changes_due(&mut state, block_number).unwrap();
        assert_eq!(witness.len(), 1, "the registry is the only account read");
        (due, witness[&SEQUENCER_REGISTRY_ADDRESS].clone())
    }

    /// An EVM over `db` in a block at 1,000 whose gas limit is `gas_limit`.
    fn evm(
        db: &mut InMemoryDB,
        gas_limit: u64,
    ) -> MegaEvm<&mut InMemoryDB, revm::inspector::NoOpInspector> {
        let block = BlockEnv { number: U256::from(1_000), gas_limit, ..Default::default() };
        MegaEvm::new(MegaContext::new(db, MegaSpecId::SATIN).with_block(block))
    }

    /// No registry, nothing due: the witness records the account's absence and no slot.
    #[test]
    fn test_is_apply_pending_changes_due_no_registry() {
        let (due, registry) = due_at(&mut InMemoryDB::default(), 1_000);
        assert!(!due);
        assert!(registry.is_loaded_as_not_existing());
        assert!(registry.storage.is_empty(), "no slot of an absent registry is read");
    }

    /// A registry with nothing pending: both pending slots are read, and no activation block.
    #[test]
    fn test_is_apply_pending_changes_due_no_pending() {
        let (due, registry) = due_at(&mut registry_with(&[]), 1_000);
        assert!(!due);
        assert_eq!(registry.storage.len(), 2, "{:?}", registry.storage);
        assert!(registry.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(registry.storage.contains_key(&PENDING_SEQUENCER));
    }

    /// A pending system address is due from its activation block on, not before.
    #[test]
    fn test_is_apply_pending_changes_due_system_address_due() {
        let mut db = registry_with(&[
            (PENDING_SYSTEM_ADDRESS, word(NEXT_SYSTEM_ADDRESS)),
            (SYSTEM_ADDRESS_ACTIVATION_BLOCK, U256::from(1_000)),
        ]);

        let (due, registry) = due_at(&mut db, 999);
        assert!(!due, "not yet due");
        assert!(registry.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(registry.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));

        let (due, registry) = due_at(&mut db, 1_000);
        assert!(due, "due at its activation block");
        assert!(registry.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(registry.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));
        assert!(registry.storage.contains_key(&PENDING_SEQUENCER), "both roles are read");
    }

    /// A pending sequencer is due alone; the system address, with nothing pending, costs one read.
    #[test]
    fn test_is_apply_pending_changes_due_sequencer_due() {
        let mut db = registry_with(&[
            (PENDING_SEQUENCER, word(NEXT_SEQUENCER)),
            (SEQUENCER_ACTIVATION_BLOCK, U256::from(500)),
        ]);
        let (due, registry) = due_at(&mut db, 500);
        assert!(due);
        assert!(registry.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(registry.storage.contains_key(&PENDING_SEQUENCER));
        assert!(registry.storage.contains_key(&SEQUENCER_ACTIVATION_BLOCK));
        assert_eq!(registry.storage.len(), 3);
    }

    /// A sequencer change due in this block makes the call whether or not the system address's
    /// is, and every slot either role read is in the witness, read-only.
    #[test]
    fn test_is_apply_pending_changes_due_checks_sequencer_when_system_not_due() {
        let mut db = registry_with(&[
            (PENDING_SYSTEM_ADDRESS, word(NEXT_SYSTEM_ADDRESS)),
            (SYSTEM_ADDRESS_ACTIVATION_BLOCK, U256::from(1_001)),
            (PENDING_SEQUENCER, word(NEXT_SEQUENCER)),
            (SEQUENCER_ACTIVATION_BLOCK, U256::from(1_000)),
        ]);
        let (due, registry) = due_at(&mut db, 1_000);
        assert!(due, "the sequencer's change is due though the system address's is not");
        assert_eq!(registry.storage.len(), 4, "both roles' slots are in the witness");
        assert!(!registry.is_touched(), "a read-only entry");
        for slot in [
            PENDING_SYSTEM_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            PENDING_SEQUENCER,
            SEQUENCER_ACTIVATION_BLOCK,
        ] {
            let slot = &registry.storage[&slot];
            assert!(!slot.is_changed(), "a read-only slot is not marked changed");
            assert_eq!(slot.original_value(), slot.present_value());
        }
    }

    /// The call applies both due roles: the pending values become current, and the pending and
    /// activation slots are cleared.
    #[test]
    fn test_transact_apply_pending_changes_updates_and_clears_due_roles() {
        let mut db = registry_with(&[
            (CURRENT_SYSTEM_ADDRESS, word(MEGA_SYSTEM_ADDRESS)),
            (CURRENT_SEQUENCER, word(CURRENT_SEQUENCER_ADDRESS)),
            (PENDING_SYSTEM_ADDRESS, word(NEXT_SYSTEM_ADDRESS)),
            (SYSTEM_ADDRESS_ACTIVATION_BLOCK, U256::from(1_000)),
            (PENDING_SEQUENCER, word(NEXT_SEQUENCER)),
            (SEQUENCER_ACTIVATION_BLOCK, U256::from(1_000)),
        ]);
        let state = transact_apply_pending_changes(&mut evm(&mut db, 30_000_000))
            .expect("applyPendingChanges() succeeds")
            .state;
        db.commit(state);

        let mut slot = |slot| db.storage(SEQUENCER_REGISTRY_ADDRESS, slot).unwrap();
        assert_eq!(slot(CURRENT_SYSTEM_ADDRESS), word(NEXT_SYSTEM_ADDRESS));
        assert_eq!(slot(CURRENT_SEQUENCER), word(NEXT_SEQUENCER));
        for cleared in [
            PENDING_SYSTEM_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            PENDING_SEQUENCER,
            SEQUENCER_ACTIVATION_BLOCK,
        ] {
            assert_eq!(slot(cleared), U256::ZERO);
        }
    }

    /// The call runs on the block's gas limit, of which 30M is regular gas and the rest the
    /// reservoir: a registry with nothing due writes nothing, and all of the reservoir is left.
    #[test]
    fn test_transact_apply_pending_changes_uses_block_gas_limit() {
        let mut db = registry_with(&[]);
        let mut evm = evm(&mut db, 250_000_000);
        let outcome = transact_apply_pending_changes(&mut evm).expect("the call succeeds");
        assert_eq!(evm.ctx().tx().gas_limit(), 250_000_000);
        assert_eq!(
            outcome.result.gas().reservoir_remaining(),
            250_000_000 - SYSTEM_CALL_REGULAR_GAS_LIMIT
        );
    }

    /// A block whose gas limit is below 30M still gives the call 30M, all of it regular gas.
    #[test]
    fn test_transact_apply_pending_changes_respects_30m_floor() {
        let mut db = registry_with(&[]);
        let mut evm = evm(&mut db, 1_000_000);
        let outcome = transact_apply_pending_changes(&mut evm).expect("the call succeeds");
        assert_eq!(evm.ctx().tx().gas_limit(), SYSTEM_CALL_REGULAR_GAS_LIMIT);
        assert_eq!(outcome.result.gas().reservoir_remaining(), 0);
    }

    /// A read the database cannot serve during the call is the database's error, not a verdict on
    /// the block.
    #[test]
    fn test_transact_apply_pending_changes_reports_a_database_error_as_one() {
        use crate::test_utils::{ErrorInjectingDatabase, MemoryDatabase};
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        db.fail_on_account = Some(SEQUENCER_REGISTRY_ADDRESS);
        let block =
            BlockEnv { number: U256::from(1_000), gas_limit: 30_000_000, ..Default::default() };
        let mut evm = MegaEvm::new(MegaContext::new(&mut db, MegaSpecId::SATIN).with_block(block));
        let err = transact_apply_pending_changes(&mut evm).expect_err("the read fails");
        assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");
        assert!(err.to_string().contains("injected basic() error"), "{err}");
    }

    /// A registry whose code reverts fails the call, and the block with it.
    #[test]
    fn test_transact_apply_pending_changes_errors_when_registry_reverts() {
        let revert = Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]));
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo { code_hash: revert.hash_slow(), code: Some(revert), ..Default::default() },
        );
        let err = transact_apply_pending_changes(&mut evm(&mut db, 30_000_000))
            .expect_err("a reverting registry fails closed");
        assert!(err.to_string().contains("reverted or halted"), "{err}");
    }

    /// A database whose registry holds `code` and `current` as its system address.
    fn registry_holding(code: Bytes, current: Option<Address>) -> InMemoryDB {
        let code = Bytecode::new_raw(code);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo { code_hash: code.hash_slow(), code: Some(code), ..Default::default() },
        );
        if let Some(current) = current {
            db.insert_account_storage(
                SEQUENCER_REGISTRY_ADDRESS,
                CURRENT_SYSTEM_ADDRESS,
                word(current),
            )
            .unwrap();
        }
        db
    }

    fn resolve(db: &mut InMemoryDB) -> Result<(Address, EvmState), BlockExecutionError> {
        resolve_system_address(&mut State::builder().with_database(db).build())
    }

    /// A registry holding the code this engine deploys resolves to the address it stores.
    #[test]
    fn test_resolve_expects_the_deployed_code_hash() {
        let mut db = registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS));
        let (address, _) = resolve(&mut db).expect("the deployed registry resolves");
        assert_eq!(address, NEXT_SYSTEM_ADDRESS);
    }

    /// A registry still holding the legacy engine's first bytecode is not the one this engine
    /// deploys, and the read refuses it.
    #[test]
    fn test_resolve_rejects_the_v1_code_hash() {
        use mega_system_contracts::sequencer_registry::{V1_0_0_CODE, V1_0_0_CODE_HASH};
        let mut db = registry_holding(V1_0_0_CODE, Some(NEXT_SYSTEM_ADDRESS));
        let err = resolve(&mut db).expect_err("the v1 registry fails closed");
        assert!(err.to_string().contains("code hash mismatch"), "{err}");
        assert!(err.to_string().contains(&V1_0_0_CODE_HASH.to_string()), "{err}");
    }

    /// The read answers the stored address and a witness holding the registry's account and the
    /// one slot it read, read-only.
    #[test]
    fn test_resolve_returns_stored_system_address_with_witness() {
        let mut db = registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS));
        let (address, witness) = resolve(&mut db).unwrap();
        assert_eq!(address, NEXT_SYSTEM_ADDRESS);

        assert_eq!(witness.len(), 1);
        let registry = &witness[&SEQUENCER_REGISTRY_ADDRESS];
        assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        assert!(!registry.is_touched(), "a read-only entry");
        assert_eq!(registry.storage.len(), 1, "exactly the one slot read");
        let slot = &registry.storage[&CURRENT_SYSTEM_ADDRESS];
        assert!(!slot.is_changed());
        assert_eq!(slot.present_value(), word(NEXT_SYSTEM_ADDRESS));
        assert_eq!(slot.original_value(), slot.present_value());
    }

    /// A registry whose system address is zero is refused.
    #[test]
    fn test_resolve_zero_slot_errors() {
        let mut db = registry_holding(SEQUENCER_REGISTRY_CODE, None);
        let err = resolve(&mut db).expect_err("a zero system address fails closed");
        assert!(err.to_string().contains("zero system address"), "{err}");
    }

    /// A registry with no account is refused.
    #[test]
    fn test_resolve_missing_registry_errors() {
        let err = resolve(&mut InMemoryDB::default()).expect_err("no registry fails closed");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// A registry holding foreign code is refused, whatever it stores.
    #[test]
    fn test_resolve_wrong_code_hash_errors() {
        let mut db = registry_holding(Bytes::from_static(&[0x60, 0x00]), Some(NEXT_SYSTEM_ADDRESS));
        let err = resolve(&mut db).expect_err("foreign code fails closed");
        assert!(err.to_string().contains("code hash mismatch"), "{err}");
    }
}
