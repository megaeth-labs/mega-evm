//! The `SequencerRegistry` system contract.
//!
//! It holds the two rotating roles of the chain — the system address and the sequencer — and
//! their change schedules. It runs its own bytecode: no method of it is intercepted, so a call
//! to it is an ordinary call. Its Solidity source is
//! `crates/system-contracts/contracts/SequencerRegistry.sol`.
//!
//! The legacy forks deployed it (Rex5) and upgraded it to v2.0.0 (Rex6); Satin neither creates nor
//! upgrades it. The block in which Satin activates checks that it holds v2.0.0
//! ([`check_system_contract`](crate::system::check_system_contract)), and is invalid
//! otherwise. A role change the registry schedules takes effect at its activation block through a
//! pre-block call: the block executor reads whether one is due
//! ([`is_apply_pending_changes_due`]) and, when one is, calls `applyPendingChanges()`
//! ([`transact_apply_pending_changes`]). The call is permissionless and applies only what is due,
//! so a block that makes it applies the same changes whoever calls.
//!
//! The live system address, which a system-address transaction must be sent from, is read out of
//! the registry by every transaction when it is validated ([`load_system_address`]): the read
//! loads the registry's account and `_currentSystemAddress` into the transaction's journal, warm,
//! as EIP-3651 starts the block beneficiary warm, so what a transaction pays never depends on its
//! envelope. Nothing inside a block can change the address after the pre-block call — a change can
//! only be scheduled for a later block — so every transaction of a block reads the address the
//! block's pre-block step left, and an EVM a node builds outside block execution reads the one in
//! the state it runs on without being told.

use alloy_evm::block::BlockExecutionError;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use op_revm::OpHaltReason;
use revm::{
    context::{result::ResultAndState, JournalTr},
    primitives::KECCAK_EMPTY,
    state::{Account, EvmState, EvmStorageSlot, TransactionId},
};

use crate::{transact_pre_block_call, ExternalEnvTypes, MegaBlockExecutionError, MegaEvm};
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

/// Whether a role change the registry holds is due at `block_number`: a pending system address
/// or sequencer whose activation block has come.
///
/// Also answers the reads that decided it, as read-only entries of the registry's account: the
/// witness a stateless client replays the decision from, which the caller hands on whether or
/// not a change is due. Both roles' pending slots are read, and a role's activation block only
/// when it has a change pending. A registry that does not exist has nothing due, and its absence
/// is what the witness records; no chain Satin runs on lacks it, its activation block refusing
/// one without the registry's code, so only a state built apart from a chain, such as a test's,
/// reaches that answer.
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
/// ([`pre_block_call_gas_limit`](crate::pre_block_call_gas_limit)): at most 30M of regular gas,
/// the rest reservoir, its state priced at the minimum SALT bucket. One call applies whichever of
/// the two roles is due; the block executor makes it only when [`is_apply_pending_changes_due`]
/// says one is.
///
/// # Errors
///
/// [`MegaBlockExecutionError::ApplyPendingChangesFailed`] when the call reverts or halts, or an
/// error other than a fatal database error stops it: a change the registry scheduled for this
/// block must be applied in it. A fatal database error is an internal error, as for every
/// pre-block call: it says nothing about the block.
pub fn transact_apply_pending_changes<DB, INSP, ExtEnvs>(
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<ResultAndState<OpHaltReason>, BlockExecutionError>
where
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
{
    let data = Bytes::from(ISequencerRegistry::applyPendingChangesCall {}.abi_encode());
    let refused = |message| MegaBlockExecutionError::ApplyPendingChangesFailed { message }.into();
    transact_pre_block_call(evm, "applyPendingChanges()", SEQUENCER_REGISTRY_ADDRESS, data, refused)
}

/// The live system address in the state `db` holds: the address a system-address transaction
/// must be sent from, as every transaction reads it when it is validated.
///
/// It is what a transaction pool gives
/// [`validate_transaction_stateless`](crate::validate_transaction_stateless), read off the state
/// the pool validates against, and it holds for the block whose pre-block changes made that
/// state: a later block's pre-block step may change it, so a pool reads it again for every block.
/// `None` when the registry names no address this engine trusts: it has no account, it holds
/// other code than [`SEQUENCER_REGISTRY_CODE`], or its `_currentSystemAddress` is zero.
///
/// It reads the registry's account and, when it holds this engine's code, the one slot, and
/// writes nothing, so any database serves: pass `&mut db` for a [`Database`](revm::Database) the
/// caller keeps, and `WrapDatabaseRef(&db)` ([`revm::database_interface::WrapDatabaseRef`]) for a
/// [`DatabaseRef`](revm::DatabaseRef), such as a read-only view of a block's state.
///
/// # Errors
///
/// The database's, when the registry's account or its slot cannot be read.
pub fn live_system_address<DB: revm::Database>(mut db: DB) -> Result<Option<Address>, DB::Error> {
    let code_hash =
        db.basic(SEQUENCER_REGISTRY_ADDRESS)?.map_or(KECCAK_EMPTY, |info| info.code_hash);
    if code_hash != SEQUENCER_REGISTRY_CODE_HASH {
        return Ok(None);
    }
    let current = db.storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS)?;
    Ok(system_address_named_by(code_hash, current))
}

/// The live system address, read out of the registry in the running transaction's journal: the
/// address a system-address transaction must be sent from.
///
/// Every transaction makes the read when it is validated, whatever its shape and whoever sends it,
/// so the registry's account and its `_currentSystemAddress` slot are loaded into the journal warm,
/// as EIP-3651 starts the block beneficiary warm: a transaction's own access to either pays the
/// warm price, whatever its envelope. Both are in the transaction's state, so a stateless witness
/// carries the read; neither is written, so committing the transaction leaves the registry as it
/// was. The registry's code is not loaded: its code hash is compared, not its code.
///
/// `None` when the registry names no address this engine trusts: it has no account, it holds other
/// code than [`SEQUENCER_REGISTRY_CODE`], or its `_currentSystemAddress` is zero. Block execution
/// never runs a transaction over such a registry — Satin's activation block refuses a chain whose
/// registry does not hold this code, nothing in Satin replaces a contract's code, and the contract
/// writes no zero address — so these are states an EVM run outside block execution may meet, and
/// in none of them is any transaction a system-address transaction. The slot is loaded in each of
/// them all the same, so the transaction's gas does not depend on them.
///
/// # Errors
///
/// The database's, when the account or the slot cannot be read.
pub(crate) fn load_system_address<J: JournalTr>(
    journal: &mut J,
) -> Result<Option<Address>, <J::Database as revm::Database>::Error> {
    let code_hash = journal.load_account(SEQUENCER_REGISTRY_ADDRESS)?.data.info.code_hash;
    let current = journal.sload(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS)?.data;
    Ok(system_address_named_by(code_hash, current))
}

/// The system address a registry whose code hash is `code_hash` names when its
/// `_currentSystemAddress` holds `current`: none unless the registry holds this engine's code and
/// a non-zero address.
fn system_address_named_by(code_hash: B256, current: U256) -> Option<Address> {
    // The expected hash is the registry code this engine requires, the latest version. When the
    // registry is next upgraded under a later spec, the hash compared here must be chosen per
    // spec: compared with the newer code alone, a block of this spec replayed by the newer binary
    // finds no system address, and its system transactions are silently demoted to ordinary ones.
    (code_hash == SEQUENCER_REGISTRY_CODE_HASH && !current.is_zero())
        .then(|| Address::from_word(current.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{system::MEGA_SYSTEM_ADDRESS, MegaContext, MegaSpecId};
    use alloy_primitives::address;
    use revm::{
        context::{BlockEnv, ContextTr, JournalTr, Transaction},
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

    /// A registry whose code reverts fails the call, and the block with it: the refusal is a
    /// validation error, a verdict on the block, and not an internal one a node would retry.
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
        let BlockExecutionError::Validation(alloy_evm::block::BlockValidationError::Other(error)) =
            &err
        else {
            panic!("a reverting registry refuses the block: {err:?}");
        };
        assert!(
            matches!(
                error.downcast_ref(),
                Some(MegaBlockExecutionError::ApplyPendingChangesFailed { .. })
            ),
            "{err:?}"
        );
        assert!(
            err.to_string().starts_with(
                "the SequencerRegistry's pending changes could not be applied: \
                 the applyPendingChanges() pre-block call did not succeed: Revert {"
            ),
            "{err}"
        );
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

    /// Reads the live system address out of `db` through a fresh journal, and answers it with the
    /// journal the read left behind.
    fn load(db: InMemoryDB) -> (Option<Address>, revm::Journal<InMemoryDB>) {
        let mut journal = revm::Journal::new(db);
        let address = load_system_address(&mut journal).expect("the read succeeds");
        (address, journal)
    }

    /// Asserts the read left the registry's account and its system-address slot in `journal`,
    /// warm, untouched and unchanged, the slot holding `current`: a witness of the read, and what
    /// the transaction's own access to either finds warm.
    fn assert_warm_and_unchanged(journal: &revm::Journal<InMemoryDB>, current: U256) {
        let registry = &journal.inner.state[&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!registry.is_cold_transaction_id(journal.transaction_id), "the account is warm");
        assert!(!registry.is_touched(), "a read-only entry");
        assert_eq!(registry.storage.len(), 1, "exactly the one slot read");
        let slot = &registry.storage[&CURRENT_SYSTEM_ADDRESS];
        assert!(!slot.is_cold, "the slot is warm");
        assert!(!slot.is_changed());
        assert_eq!(slot.present_value(), current);
    }

    /// A registry holding the code this engine requires names the address it stores, and the read
    /// leaves the account and the slot warm, without loading the code.
    #[test]
    fn test_the_read_answers_the_stored_system_address_and_warms_the_registry() {
        let db = registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS));
        let (address, journal) = load(db);
        assert_eq!(address, Some(NEXT_SYSTEM_ADDRESS));
        assert_warm_and_unchanged(&journal, word(NEXT_SYSTEM_ADDRESS));
        assert_eq!(
            journal.inner.state[&SEQUENCER_REGISTRY_ADDRESS].info.code_hash,
            SEQUENCER_REGISTRY_CODE_HASH
        );
    }

    /// A registry still holding the legacy engine's first bytecode is not the one this engine
    /// requires: it names no address. Its slot is read all the same, so what a transaction pays
    /// does not depend on the registry's code.
    #[test]
    fn test_the_read_refuses_the_v1_code_hash() {
        use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
        let (address, journal) = load(registry_holding(V1_0_0_CODE, Some(NEXT_SYSTEM_ADDRESS)));
        assert_eq!(address, None);
        assert_warm_and_unchanged(&journal, word(NEXT_SYSTEM_ADDRESS));
    }

    /// A registry holding foreign code names no address, whatever it stores.
    #[test]
    fn test_the_read_refuses_foreign_code() {
        let db = registry_holding(Bytes::from_static(&[0x60, 0x00]), Some(NEXT_SYSTEM_ADDRESS));
        let (address, journal) = load(db);
        assert_eq!(address, None);
        assert_warm_and_unchanged(&journal, word(NEXT_SYSTEM_ADDRESS));
    }

    /// A registry whose system address is zero names none: no caller, the zero address included,
    /// is the system address of an empty registry.
    #[test]
    fn test_the_read_answers_no_address_for_a_zero_slot() {
        let (address, journal) = load(registry_holding(SEQUENCER_REGISTRY_CODE, None));
        assert_eq!(address, None);
        assert_warm_and_unchanged(&journal, U256::ZERO);
    }

    /// A state without the registry names no address, and the read records the account's
    /// absence and the slot it would hold, zero.
    #[test]
    fn test_the_read_answers_no_address_without_a_registry() {
        let (address, journal) = load(InMemoryDB::default());
        assert_eq!(address, None);
        assert!(journal.inner.state[&SEQUENCER_REGISTRY_ADDRESS].is_loaded_as_not_existing());
        assert_warm_and_unchanged(&journal, U256::ZERO);
    }

    /// Read off a database, the live system address is what a transaction's validation reads:
    /// the address the registry stores when it holds this engine's code, and none otherwise.
    #[test]
    fn test_live_system_address_reads_what_validation_reads() {
        let read = |db: InMemoryDB| live_system_address(db).expect("the read succeeds");
        assert_eq!(
            read(registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS))),
            Some(NEXT_SYSTEM_ADDRESS)
        );
        assert_eq!(read(registry_holding(SEQUENCER_REGISTRY_CODE, None)), None);
        assert_eq!(
            read(registry_holding(Bytes::from_static(&[0x60, 0x00]), Some(NEXT_SYSTEM_ADDRESS))),
            None
        );
        assert_eq!(read(InMemoryDB::default()), None);

        // A database the caller keeps, and one it can only read through `DatabaseRef`.
        let mut db = registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS));
        assert_eq!(live_system_address(&mut db).unwrap(), Some(NEXT_SYSTEM_ADDRESS));
        let wrapped = revm::database_interface::WrapDatabaseRef(&db);
        assert_eq!(live_system_address(wrapped).unwrap(), Some(NEXT_SYSTEM_ADDRESS));
    }

    /// A read the database cannot serve is the database's error, not an absent address.
    #[test]
    fn test_the_read_reports_a_database_error() {
        use crate::test_utils::{ErrorInjectingDatabase, MemoryDatabase};
        let mut db = ErrorInjectingDatabase::new(
            MemoryDatabase::default()
                .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE),
        );
        db.fail_on_storage = Some((SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS));
        let mut journal: revm::Journal<_> = revm::Journal::new(db);
        let err = load_system_address(&mut journal).expect_err("the read fails");
        assert!(err.to_string().contains("injected storage() error"), "{err}");
    }
}
