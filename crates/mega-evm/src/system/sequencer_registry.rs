//! The `SequencerRegistry` system contract.
//!
//! It holds the two rotating roles of the chain — the system address and the sequencer — and
//! their change schedules. It runs its own bytecode: no method of it is intercepted, so a call
//! to it is an ordinary call. Its Solidity source is
//! `crates/system-contracts/contracts/SequencerRegistry.sol`.
//!
//! Every block takes three pre-block steps on it, after the EIP calls and the other five system
//! contracts' deployment, each returning its state for the block executor to commit on its own:
//!
//! 1. the due-change read ([`is_apply_pending_changes_due`]) and, when a role change is due in the
//!    block, the `applyPendingChanges()` call ([`transact_apply_pending_changes`]), which is
//!    permissionless and applies only what is due, so a block that makes it applies the same
//!    changes whoever calls;
//! 2. its deployment ([`deploy_sequencer_registry`]): left as it is when it holds v2.0.0, migrated
//!    in place from v1.0.0, seeded from the chain configuration's bootstrap values
//!    ([`SequencerRegistryBootstrap`]) when the address holds no code, and the block refused when
//!    it holds any other code;
//! 3. the resolution of the block's live system address ([`resolve_system_address`]), the address a
//!    Mega System Transaction must be sent from, read once on the state every other step left and
//!    handed to execution.
//!
//! No transaction reads the registry for its classification, so none finds its account or slot
//! warm: nothing inside a block can change the address after the pre-block steps — a change can
//! only be scheduled for a later block — so every transaction of a block is classified against the
//! address its pre-block steps left. An EVM built outside block execution reads the address in the
//! state it runs on once, when it is built ([`live_system_address`]), without loading anything into
//! a transaction's journal.

use core::fmt;

use alloy_evm::block::BlockExecutionError;
use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use op_revm::OpHaltReason;
use revm::{
    context::result::ResultAndState,
    primitives::KECCAK_EMPTY,
    state::{Account, Bytecode, EvmState, EvmStorageSlot, TransactionId},
};

use super::MEGA_SYSTEM_ADDRESS;
use crate::{transact_pre_block_call, ExternalEnvTypes, MegaBlockExecutionError, MegaEvm};
use storage_slots::{
    ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK, INITIAL_SEQUENCER,
    INITIAL_SYSTEM_ADDRESS, MIN_ROTATION_DELAY, PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS,
    SEQUENCER_ACTIVATION_BLOCK, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
};

/// The address of the `SequencerRegistry` system contract.
pub const SEQUENCER_REGISTRY_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000006");

/// The code of the `SequencerRegistry` contract.
pub use mega_system_contracts::sequencer_registry::LATEST_CODE as SEQUENCER_REGISTRY_CODE;

/// The code hash of the `SequencerRegistry` contract.
pub use mega_system_contracts::sequencer_registry::LATEST_CODE_HASH as SEQUENCER_REGISTRY_CODE_HASH;

/// The code hash of the registry's version 1.0.0, which the deployment migrates in place.
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE_HASH as SEQUENCER_REGISTRY_V1_CODE_HASH;

pub use mega_system_contracts::sequencer_registry::{storage_slots, ISequencerRegistry};

/// The values a new `SequencerRegistry` is seeded with and a migrated one is given its minimum
/// rotation delay from: the chain configuration's bootstrap values, under the keys the Rex era
/// introduced (`rex5InitialSequencer`, `rex5InitialAdmin`, `rex6MinRotationDelay`).
///
/// The initial system address is not among them: a new registry names
/// [`MEGA_SYSTEM_ADDRESS`], which every chain's first system transactions came from. Once the
/// registry holds its roles, its storage decides them, not these values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequencerRegistryBootstrap {
    /// The sequencer a new registry starts with.
    pub initial_sequencer: Address,
    /// The admin a new registry starts with.
    pub initial_admin: Address,
    /// The least number of blocks between scheduling a sequencer change and its activation.
    pub min_rotation_delay: u64,
}

impl SequencerRegistryBootstrap {
    /// The bootstrap values `initial_sequencer`, `initial_admin` and `min_rotation_delay`.
    pub const fn new(
        initial_sequencer: Address,
        initial_admin: Address,
        min_rotation_delay: u64,
    ) -> Self {
        Self { initial_sequencer, initial_admin, min_rotation_delay }
    }

    /// Checks that no value is zero: neither address, which would seed a role nobody holds, nor the
    /// delay, which would leave the network no window to react to a scheduled rotation.
    ///
    /// The one check of the values, made by the chain configuration's parser
    /// ([`SatinChainConfig::from_genesis_config`](crate::SatinChainConfig::from_genesis_config)),
    /// by a schedule's validation
    /// ([`validate_schedule`](crate::MegaHardforks::validate_schedule)) and before a block seeds or
    /// migrates the registry.
    ///
    /// # Errors
    ///
    /// The first zero value, in the order of [`SEQUENCER_REGISTRY_BOOTSTRAP_KEYS`], named by its
    /// chain-configuration key.
    ///
    /// [`SEQUENCER_REGISTRY_BOOTSTRAP_KEYS`]: crate::SEQUENCER_REGISTRY_BOOTSTRAP_KEYS
    pub fn validate(&self) -> Result<(), ZeroBootstrapValue> {
        let zero = [
            (self.initial_sequencer.is_zero(), crate::REX5_INITIAL_SEQUENCER_KEY),
            (self.initial_admin.is_zero(), crate::REX5_INITIAL_ADMIN_KEY),
            (self.min_rotation_delay == 0, crate::REX6_MIN_ROTATION_DELAY_KEY),
        ];
        match zero.into_iter().find(|(is_zero, _)| *is_zero) {
            Some((_, key)) => Err(ZeroBootstrapValue { key }),
            None => Ok(()),
        }
    }

    /// The storage a registry deployed new at `block_number` is seeded with, slot by slot: the
    /// current and initial system address [`MEGA_SYSTEM_ADDRESS`], the current and initial
    /// sequencer, the admin, the block it starts at and the minimum rotation delay. The pending
    /// slots and the two histories start empty.
    pub fn seed(&self, block_number: U256) -> [(U256, U256); 7] {
        let system_address = address_word(MEGA_SYSTEM_ADDRESS);
        let sequencer = address_word(self.initial_sequencer);
        [
            (CURRENT_SYSTEM_ADDRESS, system_address),
            (CURRENT_SEQUENCER, sequencer),
            (ADMIN, address_word(self.initial_admin)),
            (INITIAL_SYSTEM_ADDRESS, system_address),
            (INITIAL_SEQUENCER, sequencer),
            (INITIAL_FROM_BLOCK, block_number),
            (MIN_ROTATION_DELAY, U256::from(self.min_rotation_delay)),
        ]
    }
}

/// A `SequencerRegistry` bootstrap value that is zero, which no chain configuration may carry
/// ([`SequencerRegistryBootstrap::validate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZeroBootstrapValue {
    /// The chain-configuration key of the value.
    pub key: &'static str,
}

impl fmt::Display for ZeroBootstrapValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the SequencerRegistry's bootstrap value `{}` is zero", self.key)
    }
}

impl core::error::Error for ZeroBootstrapValue {}

/// An address as the word a Solidity `address` slot holds.
fn address_word(address: Address) -> U256 {
    U256::from_be_bytes(address.into_word().0)
}

/// Why the registry's deployment or the resolution of the live system address could not be made
/// or refused the block.
#[derive(Debug)]
pub enum SequencerRegistryError<DbError> {
    /// The database could not serve a read of the registry: a fault of the node, never a verdict
    /// on the block.
    Database(DbError),
    /// The registry has to be seeded or migrated, and the chain configuration carries no usable
    /// bootstrap values for it: none, or one of them zero. A fault of the node's configuration,
    /// which its validation refuses when it is loaded
    /// ([`validate_schedule`](crate::MegaHardforks::validate_schedule)), so only a schedule that
    /// skipped it reaches this; never a verdict on the block.
    UnusableBootstrap,
    /// The registry makes the block invalid.
    Refused(SequencerRegistryRefusal),
}

impl<DbError: fmt::Display> fmt::Display for SequencerRegistryError<DbError> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(f, "{error}"),
            Self::UnusableBootstrap => f.write_str(
                "the SequencerRegistry must be seeded or migrated, and the chain configuration \
                 carries no usable bootstrap values for it",
            ),
            Self::Refused(refusal) => write!(f, "{refusal}"),
        }
    }
}

impl<DbError: core::error::Error + 'static> core::error::Error for SequencerRegistryError<DbError> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::UnusableBootstrap | Self::Refused(_) => None,
        }
    }
}

/// What the `SequencerRegistry` holds that makes a block invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequencerRegistryRefusal {
    /// The deployment found code it cannot migrate: neither version 2.0.0, nor version 1.0.0, nor
    /// none. A stateful contract's storage is never overwritten by code it was not migrated to.
    UnmigratableCode {
        /// The code hash the registry's address holds.
        found: B256,
    },
    /// The resolution found no account at the registry's address.
    Absent,
    /// The resolution found code other than version 2.0.0.
    UnexpectedCode {
        /// The code hash the registry's address holds.
        found: B256,
    },
    /// The resolution found `_currentSystemAddress` zero: the registry names no system address.
    NoSystemAddress,
}

impl fmt::Display for SequencerRegistryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnmigratableCode { found } => write!(
                f,
                "the SequencerRegistry at {SEQUENCER_REGISTRY_ADDRESS} holds code hash {found}, \
                 which is neither version 2.0.0 ({SEQUENCER_REGISTRY_CODE_HASH}) nor version \
                 1.0.0 ({SEQUENCER_REGISTRY_V1_CODE_HASH}) nor no code: its storage is not \
                 overwritten"
            ),
            Self::Absent => write!(
                f,
                "the SequencerRegistry at {SEQUENCER_REGISTRY_ADDRESS} has no account when the \
                 block resolves its live system address"
            ),
            Self::UnexpectedCode { found } => write!(
                f,
                "the SequencerRegistry at {SEQUENCER_REGISTRY_ADDRESS} must hold code hash \
                 {SEQUENCER_REGISTRY_CODE_HASH} when the block resolves its live system address; \
                 it holds code hash {found}"
            ),
            Self::NoSystemAddress => write!(
                f,
                "the SequencerRegistry at {SEQUENCER_REGISTRY_ADDRESS} names no system address: \
                 its _currentSystemAddress is zero"
            ),
        }
    }
}

/// Whether a role change the registry holds is due at `block_number`: a pending system address
/// or sequencer whose activation block has come.
///
/// Also answers the reads that decided it, as read-only entries of the registry's account: the
/// witness a stateless client replays the decision from, which the caller commits whether or
/// not a change is due. Both roles' pending slots are read, and a role's activation block only
/// when it has a change pending. A registry that does not exist has nothing due, and its absence
/// is what the witness records: the block's deployment of the registry, which follows, seeds it.
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
/// ([`PRE_BLOCK_CALL_GAS_LIMIT`](crate::constants::PRE_BLOCK_CALL_GAS_LIMIT)): 30M of regular gas,
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

/// Deploys the registry as a block's pre-block step, on the state the earlier steps left, and
/// answers the step's state. Nothing is committed, and nothing is charged.
///
/// What it does depends on what the registry's address holds:
///
/// - version 2.0.0: nothing; the account is answered as a read-only entry (neither touched nor
///   created, no storage), so the witness records the read;
/// - version 1.0.0: migrated in place: its code replaced by version 2.0.0 and slot 13
///   (`_minRotationDelay`) written with the bootstrap's minimum rotation delay, every other slot —
///   the roles, the pending changes, the histories — its nonce and its balance kept;
/// - no account, or an account without code: version 2.0.0 deployed new, an existing account's
///   nonce and balance kept and its storage cleared (the account is marked created), and seeded
///   with [`SequencerRegistryBootstrap::seed`] at `block_number`.
///
/// # Errors
///
/// [`SequencerRegistryError::Refused`] with [`SequencerRegistryRefusal::UnmigratableCode`] when
/// the address holds any other code, which makes the block invalid;
/// [`SequencerRegistryError::UnusableBootstrap`] when the registry must be seeded or migrated and
/// `bootstrap` is `None` or holds a zero value; [`SequencerRegistryError::Database`] when a read
/// fails.
pub fn deploy_sequencer_registry<DB: revm::Database>(
    db: &mut DB,
    block_number: U256,
    bootstrap: Option<SequencerRegistryBootstrap>,
) -> Result<EvmState, SequencerRegistryError<DB::Error>> {
    let existing =
        db.basic(SEQUENCER_REGISTRY_ADDRESS).map_err(SequencerRegistryError::Database)?;
    let found = existing.as_ref().map_or(KECCAK_EMPTY, |info| info.code_hash);
    if let Some(info) = existing.as_ref().filter(|_| found == SEQUENCER_REGISTRY_CODE_HASH) {
        return Ok(EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, Account::from(info.clone()))]));
    }
    let migrate = found == SEQUENCER_REGISTRY_V1_CODE_HASH;
    if !migrate && found != KECCAK_EMPTY {
        return Err(SequencerRegistryError::Refused(SequencerRegistryRefusal::UnmigratableCode {
            found,
        }));
    }
    let bootstrap = bootstrap
        .filter(|bootstrap| bootstrap.validate().is_ok())
        .ok_or(SequencerRegistryError::UnusableBootstrap)?;

    let mut info = existing.unwrap_or_default();
    info.code_hash = SEQUENCER_REGISTRY_CODE_HASH;
    info.code = Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE));
    let mut registry = Account::from(info);
    registry.mark_touch();
    if migrate {
        // Not marked created, which would clear the roles, the pending changes and the histories
        // on commit: the one slot written is the one version 1.0.0 never used.
        let original = db
            .storage(SEQUENCER_REGISTRY_ADDRESS, MIN_ROTATION_DELAY)
            .map_err(SequencerRegistryError::Database)?;
        let delay = U256::from(bootstrap.min_rotation_delay);
        registry.storage.insert(
            MIN_ROTATION_DELAY,
            EvmStorageSlot::new_changed(original, delay, TransactionId::ZERO),
        );
    } else {
        registry.mark_created();
        for (slot, value) in bootstrap.seed(block_number) {
            registry
                .storage
                .insert(slot, EvmStorageSlot::new_changed(U256::ZERO, value, TransactionId::ZERO));
        }
    }
    Ok(EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, registry)]))
}

/// Resolves a block's live system address, once, after every other pre-block step: the address
/// the registry's `_currentSystemAddress` holds in the state `db` holds, with the read as a
/// read-only state (the registry's account and slot 0) for the block executor to commit last.
///
/// The registry's code hash is compared and its code neither loaded nor run; nothing is charged or
/// written.
///
/// # Errors
///
/// [`SequencerRegistryError::Refused`] when the registry has no account, holds other code than
/// [`SEQUENCER_REGISTRY_CODE`], or names no system address, each of which makes the block invalid;
/// [`SequencerRegistryError::Database`] when a read fails.
pub fn resolve_system_address<DB: revm::Database>(
    db: &mut DB,
) -> Result<(Address, EvmState), SequencerRegistryError<DB::Error>> {
    let refused = SequencerRegistryError::Refused;
    let Some(info) =
        db.basic(SEQUENCER_REGISTRY_ADDRESS).map_err(SequencerRegistryError::Database)?
    else {
        return Err(refused(SequencerRegistryRefusal::Absent));
    };
    if info.code_hash != SEQUENCER_REGISTRY_CODE_HASH {
        return Err(refused(SequencerRegistryRefusal::UnexpectedCode { found: info.code_hash }));
    }
    let mut registry = Account::from(info);
    let current = read_slot(db, &mut registry, CURRENT_SYSTEM_ADDRESS)
        .map_err(SequencerRegistryError::Database)?;
    if current.is_zero() {
        return Err(refused(SequencerRegistryRefusal::NoSystemAddress));
    }
    let address = Address::from_word(current.into());
    Ok((address, EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, registry)])))
}

/// The live system address in the state `db` holds, read directly off the database and outside any
/// transaction: what an EVM built without an address classifies its transactions against, and
/// what a transaction pool gives
/// [`validate_transaction_stateless`](crate::validate_transaction_stateless).
///
/// It holds for the block whose pre-block steps made that state: a later block's pre-block step
/// may change it, so a reader reads it again for every block. `None` when the registry names no
/// address this engine trusts: it has no account, it holds other code than
/// [`SEQUENCER_REGISTRY_CODE`], or its `_currentSystemAddress` is zero. Block execution refuses a
/// block in each of those cases ([`resolve_system_address`]); outside it no transaction is then a
/// Mega System Transaction.
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
    // The expected hash is the registry code this engine requires, the latest version. When the
    // registry is next upgraded under a later spec, the hash compared here and in the block's
    // resolution must be chosen per spec: compared with the newer code alone, a block of this
    // spec replayed by the newer binary is refused.
    Ok((!current.is_zero()).then(|| Address::from_word(current.into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MegaContext, MegaSpecId, SYSTEM_CALL_REGULAR_GAS_LIMIT};
    use alloy_primitives::address;
    use revm::{
        context::{BlockEnv, ContextTr, Transaction},
        database::{InMemoryDB, State},
        state::AccountInfo,
        Database, DatabaseCommit,
    };

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

    /// The call runs on the pre-block budget whatever the block's gas limit, of which 30M is
    /// regular gas and the rest the reservoir: a registry with nothing due writes nothing, and all
    /// of the reservoir is left, in a block of 1M and in one of 250M.
    #[test]
    fn test_transact_apply_pending_changes_runs_on_the_pre_block_budget() {
        use crate::constants::PRE_BLOCK_CALL_GAS_LIMIT;
        for block_gas_limit in [1_000_000, 250_000_000] {
            let mut db = registry_with(&[]);
            let mut evm = evm(&mut db, block_gas_limit);
            transact_apply_pending_changes(&mut evm).expect("the call succeeds");
            assert_eq!(evm.ctx().tx().gas_limit(), PRE_BLOCK_CALL_GAS_LIMIT, "{block_gas_limit}");
            assert_eq!(
                evm.ctx().additional_limit().reservoir_remaining(),
                PRE_BLOCK_CALL_GAS_LIMIT - SYSTEM_CALL_REGULAR_GAS_LIMIT,
                "{block_gas_limit}"
            );
        }
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

    /// The bootstrap values the deployment tests seed from.
    const BOOTSTRAP: SequencerRegistryBootstrap =
        SequencerRegistryBootstrap::new(NEXT_SEQUENCER, CURRENT_SEQUENCER_ADDRESS, 7_200);

    /// The registry's entry in the state a deployment over `db` at block 1,000 answers.
    fn deployed(
        db: &mut InMemoryDB,
        bootstrap: Option<SequencerRegistryBootstrap>,
    ) -> Result<Account, SequencerRegistryError<core::convert::Infallible>> {
        let state = deploy_sequencer_registry(db, U256::from(1_000), bootstrap)?;
        assert_eq!(state.len(), 1, "the registry is the only account the deployment names");
        Ok(state[&SEQUENCER_REGISTRY_ADDRESS].clone())
    }

    /// A registry at version 2.0.0 is left as it is: a read-only entry, no slot read or written,
    /// and no bootstrap needed.
    #[test]
    fn test_the_deployment_leaves_version_2_as_a_read_only_entry() {
        let mut db = registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS));
        let registry = deployed(&mut db, None).expect("version 2.0.0 needs no bootstrap");
        assert!(!registry.is_touched() && !registry.is_created(), "a read-only entry");
        assert!(registry.storage.is_empty(), "no slot is read or written");
        assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    }

    /// Where the address has no account, the deployment installs version 2.0.0 as a new contract
    /// seeded with the seven flat slots: the system address both current and initial, the
    /// bootstrap's sequencer both current and initial, its admin, the block number and its
    /// minimum rotation delay. Where it holds an account without code, the account's nonce and
    /// balance are kept and its storage goes: the account is marked created.
    #[test]
    fn test_the_deployment_seeds_a_new_registry() {
        let expected_storage = [
            (CURRENT_SYSTEM_ADDRESS, word(MEGA_SYSTEM_ADDRESS)),
            (CURRENT_SEQUENCER, word(NEXT_SEQUENCER)),
            (ADMIN, word(CURRENT_SEQUENCER_ADDRESS)),
            (INITIAL_SYSTEM_ADDRESS, word(MEGA_SYSTEM_ADDRESS)),
            (INITIAL_SEQUENCER, word(NEXT_SEQUENCER)),
            (INITIAL_FROM_BLOCK, U256::from(1_000)),
            (MIN_ROTATION_DELAY, U256::from(7_200)),
        ];
        let mut funded = InMemoryDB::default();
        funded.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo { nonce: 3, balance: U256::from(9), ..Default::default() },
        );
        funded
            .insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, U256::from(42), U256::ONE)
            .unwrap();
        for (mut db, nonce, balance) in
            [(InMemoryDB::default(), 0, U256::ZERO), (funded, 3, U256::from(9))]
        {
            let registry = deployed(&mut db, Some(BOOTSTRAP)).expect("the registry is seeded");
            assert!(registry.is_touched() && registry.is_created(), "a new contract");
            assert_eq!((registry.info.nonce, registry.info.balance), (nonce, balance));
            assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
            let seeded: Vec<_> = registry
                .storage
                .iter()
                .map(|(slot, value)| (*slot, value.original_value(), value.present_value()))
                .collect();
            let mut expected: Vec<_> =
                expected_storage.iter().map(|&(slot, value)| (slot, U256::ZERO, value)).collect();
            expected.sort();
            let mut seeded = seeded;
            seeded.sort();
            assert_eq!(seeded, expected);

            db.commit(EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, registry)]));
            assert_eq!(db.storage(SEQUENCER_REGISTRY_ADDRESS, U256::from(42)).unwrap(), U256::ZERO);
            assert_eq!(live_system_address(&mut db).unwrap(), Some(MEGA_SYSTEM_ADDRESS));
        }
    }

    /// A registry at version 1.0.0 is migrated in place: its code becomes version 2.0.0 and slot
    /// 13 the bootstrap's minimum rotation delay, and every other slot, its nonce and its balance
    /// stay. The account is not marked created, which would clear them.
    #[test]
    fn test_the_deployment_migrates_version_1_writing_slot_13_alone() {
        use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
        let mut db = registry_holding(V1_0_0_CODE, Some(NEXT_SYSTEM_ADDRESS));
        db.insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, PENDING_SEQUENCER, U256::from(5))
            .unwrap();
        db.cache.accounts.get_mut(&SEQUENCER_REGISTRY_ADDRESS).unwrap().info.balance =
            U256::from(11);
        let registry = deployed(&mut db, Some(BOOTSTRAP)).expect("the registry is migrated");
        assert!(registry.is_touched() && !registry.is_created(), "migrated, not created");
        assert_eq!(registry.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        assert_eq!(registry.info.balance, U256::from(11));
        assert_eq!(registry.storage.keys().copied().collect::<Vec<_>>(), [MIN_ROTATION_DELAY]);
        let delay = &registry.storage[&MIN_ROTATION_DELAY];
        assert_eq!(
            (delay.original_value(), delay.present_value()),
            (U256::ZERO, U256::from(7_200))
        );

        db.commit(EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, registry)]));
        assert_eq!(live_system_address(&mut db).unwrap(), Some(NEXT_SYSTEM_ADDRESS), "kept");
        assert_eq!(
            db.storage(SEQUENCER_REGISTRY_ADDRESS, PENDING_SEQUENCER).unwrap(),
            U256::from(5)
        );
    }

    /// Any other code at the registry's address refuses the block, naming the code hash, and
    /// writes nothing; no bootstrap changes that.
    #[test]
    fn test_the_deployment_refuses_other_code() {
        let foreign = Bytes::from_static(&[0x60, 0x00]);
        let found = Bytecode::new_raw(foreign.clone()).hash_slow();
        for bootstrap in [Some(BOOTSTRAP), None] {
            let mut db = registry_holding(foreign.clone(), Some(NEXT_SYSTEM_ADDRESS));
            match deployed(&mut db, bootstrap) {
                Err(SequencerRegistryError::Refused(refusal)) => {
                    assert_eq!(refusal, SequencerRegistryRefusal::UnmigratableCode { found });
                    assert!(refusal.to_string().contains(&found.to_string()), "{refusal}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// A registry that must be seeded or migrated needs bootstrap values none of which is zero;
    /// without them the deployment cannot be made, a fault of the configuration.
    #[test]
    fn test_the_deployment_needs_a_usable_bootstrap_to_seed_or_migrate() {
        use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
        let unusable = [
            None,
            Some(SequencerRegistryBootstrap { initial_sequencer: Address::ZERO, ..BOOTSTRAP }),
            Some(SequencerRegistryBootstrap { initial_admin: Address::ZERO, ..BOOTSTRAP }),
            Some(SequencerRegistryBootstrap { min_rotation_delay: 0, ..BOOTSTRAP }),
        ];
        for bootstrap in unusable {
            for mut db in [InMemoryDB::default(), registry_holding(V1_0_0_CODE, None)] {
                assert!(
                    matches!(
                        deployed(&mut db, bootstrap),
                        Err(SequencerRegistryError::UnusableBootstrap)
                    ),
                    "{bootstrap:?}"
                );
            }
        }
        assert_eq!(BOOTSTRAP.validate(), Ok(()));
    }

    /// The resolution answers the address `_currentSystemAddress` holds, with the registry's
    /// account and that one slot as a read-only entry.
    #[test]
    fn test_the_resolution_answers_the_stored_address_with_a_read_only_entry() {
        let mut db = registry_holding(SEQUENCER_REGISTRY_CODE, Some(NEXT_SYSTEM_ADDRESS));
        let (address, state) = resolve_system_address(&mut db).expect("the registry names one");
        assert_eq!(address, NEXT_SYSTEM_ADDRESS);
        let registry = &state[&SEQUENCER_REGISTRY_ADDRESS];
        assert_eq!(state.len(), 1);
        assert!(!registry.is_touched() && !registry.is_created(), "a read-only entry");
        assert_eq!(registry.storage.keys().copied().collect::<Vec<_>>(), [CURRENT_SYSTEM_ADDRESS]);
        assert!(!registry.storage[&CURRENT_SYSTEM_ADDRESS].is_changed());
    }

    /// The resolution refuses the block for a registry that has no account, holds other code —
    /// version 1.0.0 included — or names no system address.
    #[test]
    fn test_the_resolution_refuses_an_unusable_registry() {
        use mega_system_contracts::sequencer_registry::V1_0_0_CODE;
        let foreign = Bytes::from_static(&[0x60, 0x00]);
        let cases = [
            (InMemoryDB::default(), SequencerRegistryRefusal::Absent),
            (
                registry_holding(foreign.clone(), Some(NEXT_SYSTEM_ADDRESS)),
                SequencerRegistryRefusal::UnexpectedCode {
                    found: Bytecode::new_raw(foreign).hash_slow(),
                },
            ),
            (
                registry_holding(V1_0_0_CODE, Some(NEXT_SYSTEM_ADDRESS)),
                SequencerRegistryRefusal::UnexpectedCode { found: SEQUENCER_REGISTRY_V1_CODE_HASH },
            ),
            (
                registry_holding(SEQUENCER_REGISTRY_CODE, None),
                SequencerRegistryRefusal::NoSystemAddress,
            ),
        ];
        for (mut db, expected) in cases {
            match resolve_system_address(&mut db) {
                Err(SequencerRegistryError::Refused(refusal)) => assert_eq!(refusal, expected),
                other => panic!("{expected:?}: {other:?}"),
            }
        }
    }

    /// Read off a database, the live system address is what the block's resolution reads, and
    /// none where the resolution refuses the block.
    #[test]
    fn test_live_system_address_reads_what_the_resolution_reads() {
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

    /// Each registry error says what went wrong: a zero bootstrap value names its key, a database
    /// failure is the database's error, which it also hands on as its source, an unusable bootstrap
    /// says the configuration carries no usable values, and a refusal says what the registry holds;
    /// only a database failure has a source.
    #[test]
    fn test_the_registry_errors_name_their_cause() {
        use core::error::Error as _;
        let zero = ZeroBootstrapValue { key: crate::REX6_MIN_ROTATION_DELAY_KEY };
        assert_eq!(
            zero.to_string(),
            "the SequencerRegistry's bootstrap value `rex6MinRotationDelay` is zero"
        );

        let database = SequencerRegistryError::Database(fmt::Error);
        assert_eq!(database.to_string(), fmt::Error.to_string());
        let source = database.source().map(|source| source.to_string());
        assert_eq!(source, Some(fmt::Error.to_string()), "the database's error is the source");

        let unusable = SequencerRegistryError::<fmt::Error>::UnusableBootstrap;
        assert!(unusable.to_string().contains("no usable bootstrap values"), "{unusable}");
        assert!(unusable.source().is_none());

        let refusal = SequencerRegistryRefusal::NoSystemAddress;
        let refused = SequencerRegistryError::<fmt::Error>::Refused(refusal.clone());
        assert_eq!(refused.to_string(), refusal.to_string());
        assert!(refused.source().is_none());
    }

    /// A read the database cannot serve is the database's error, in the resolution, the
    /// deployment and the direct read alike: never an absent registry or a refused block.
    #[test]
    fn test_a_failed_registry_read_is_the_databases_error() {
        use crate::test_utils::{ErrorInjectingDatabase, MemoryDatabase};
        let failing = || {
            let mut db = ErrorInjectingDatabase::new(
                MemoryDatabase::default()
                    .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE),
            );
            db.fail_on_storage = Some((SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS));
            db
        };
        let err = live_system_address(failing()).expect_err("the read fails");
        assert!(err.to_string().contains("injected storage() error"), "{err}");
        match resolve_system_address(&mut failing()) {
            Err(SequencerRegistryError::Database(err)) => {
                assert!(err.to_string().contains("injected storage() error"), "{err}")
            }
            other => panic!("{other:?}"),
        }
        let mut db = failing();
        db.fail_on_account = Some(SEQUENCER_REGISTRY_ADDRESS);
        assert!(matches!(
            deploy_sequencer_registry(&mut db, U256::ZERO, Some(BOOTSTRAP)),
            Err(SequencerRegistryError::Database(_))
        ));
    }
}
