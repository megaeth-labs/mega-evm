//! Shared primitive for deploying `MegaETH` system contracts and the EIP-7997 factory.
//!
//! Every predeploy is one state patch: install the account's bytecode, optionally seed storage,
//! and mark the account touched and created. [`SystemContractSpec`] names those differences;
//! [`transact_deploy`] applies them and never commits. The caller — the block executor today,
//! the CLI later — commits the returned [`EvmState`].
//!
//! [`system_contract_specs`] is the single list both iterate: the six `MegaETH` contracts in
//! address order, then the factory. There is one version of each (the bytecode
//! `mega-system-contracts` ships; the Oracle and the `SequencerRegistry` are their v2.0.0), and
//! a system address that already holds different code is an error rather than an upgrade.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::fmt;
use std::vec::Vec;

use alloy_primitives::{address, b256, bytes, keccak256, Address, Bytes, B256, U256};
use mega_system_contracts::sequencer_registry::storage_slots::{
    ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK, INITIAL_SEQUENCER,
    INITIAL_SYSTEM_ADDRESS, MIN_ROTATION_DELAY,
};
use revm::{
    primitives::KECCAK_EMPTY,
    state::{Account, Bytecode, EvmState, EvmStorageSlot, TransactionId},
    Database,
};

use super::{
    keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_CODE_HASH},
    SequencerRegistryConfig, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, ACCESS_CONTROL_CODE_HASH,
    HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
    HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
    LIMIT_CONTROL_CODE_HASH, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
    ORACLE_CONTRACT_CODE_HASH, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
    SEQUENCER_REGISTRY_CODE_HASH,
};

/// How many contracts [`system_contract_specs`] returns: the six `MegaETH` system contracts and
/// the EIP-7997 factory.
pub const SYSTEM_CONTRACT_DEPLOY_COUNT: usize = 7;

/// The EIP-7997 `CREATE2` factory address (Arachnid's deterministic deployment proxy).
pub const CREATE2_FACTORY_ADDRESS: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");

/// Runtime bytecode of the EIP-7997 factory.
pub const CREATE2_FACTORY_CODE: Bytes =
    bytes!("0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3");

/// Keccak-256 hash of [`CREATE2_FACTORY_CODE`].
pub const CREATE2_FACTORY_CODE_HASH: B256 =
    b256!("0x2fa86add0aed31f33a762c9d88e807c475bd51d0f52bd0955754b2608f7e4989");

/// Nonce a freshly created contract account starts at (EIP-161). The factory is created, so it
/// is 1; the six `MegaETH` contracts use the same number.
pub const CREATE2_FACTORY_NONCE: u64 = 1;

/// Declarative description of a single system-contract deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemContractSpec {
    /// Predeploy address the bytecode is installed at.
    pub address: Address,
    /// Runtime bytecode to install.
    pub code: Bytes,
    /// Keccak-256 hash of [`code`](Self::code). Used for the idempotence check.
    pub code_hash: B256,
    /// Nonce written on a fresh deploy. Created contract accounts start at 1.
    pub nonce: u64,
    /// Flat storage slots to seed `(slot, value)` on a fresh deploy. Empty for every contract
    /// except the `SequencerRegistry`.
    pub seed: Vec<(U256, U256)>,
    /// When set, matching code with nonce 0 is an error rather than a read-only entry.
    ///
    /// EIP-7997 requires the factory to hold its runtime **and a nonzero nonce**. The six
    /// `MegaETH` contracts are not EIP-7997, so they leave this unset: matching code is
    /// accepted regardless of nonce, because the matching-code path must not rewrite an
    /// already-deployed contract.
    pub require_nonzero_nonce: bool,
}

impl SystemContractSpec {
    /// A spec with nonce 1, no seeded storage, and no nonce requirement on a matching account.
    pub fn new(address: Address, code: Bytes, code_hash: B256) -> Self {
        Self { address, code, code_hash, nonce: 1, seed: Vec::new(), require_nonzero_nonce: false }
    }

    /// Sets the seeded storage slots applied on a fresh deploy.
    pub fn with_seed(mut self, seed: Vec<(U256, U256)>) -> Self {
        self.seed = seed;
        self
    }

    /// Requires a matching existing account to hold a nonzero nonce (EIP-7997).
    pub fn require_nonzero_nonce(mut self) -> Self {
        self.require_nonzero_nonce = true;
        self
    }
}

/// Why [`transact_deploy`] could not produce a witness.
#[derive(Debug)]
pub enum SystemContractDeployError<DbError> {
    /// The database could not load the account at the spec's address.
    Database(DbError),
    /// `address` already holds bytecode whose hash is `found`, not `expected`.
    ///
    /// A system address holding foreign code is a broken chain, not something to overwrite.
    ForeignCode {
        /// The system-contract address that already has code.
        address: Address,
        /// The hash this engine deploys.
        expected: B256,
        /// The hash already in state.
        found: B256,
    },
    /// The EIP-7997 factory already holds the right runtime but nonce 0, which the EIP
    /// forbids. The matching-code path does not rewrite a nonce, so this cannot be repaired
    /// by deploying again.
    ZeroFactoryNonce {
        /// The factory address.
        address: Address,
    },
    /// The address has empty code but a used nonce. Prefunding with balance alone is the
    /// bootstrap path; a used account at a system address is not overwritten silently.
    UsedEmptyAccount {
        /// The system-contract address.
        address: Address,
        /// The nonce already on the account.
        nonce: u64,
    },
}

impl<DbError> From<DbError> for SystemContractDeployError<DbError> {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl<DbError: fmt::Display> fmt::Display for SystemContractDeployError<DbError> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(f, "{error}"),
            Self::ForeignCode { address, expected, found } => {
                write!(
                    f,
                    "system contract at {address} has unexpected code hash {found}, expected {expected}; refusing to overwrite"
                )
            }
            Self::ZeroFactoryNonce { address } => {
                write!(
                    f,
                    "EIP-7997 factory at {address} has matching code but nonce 0; refusing to accept a zero-nonce factory"
                )
            }
            Self::UsedEmptyAccount { address, nonce } => {
                write!(
                    f,
                    "system contract at {address} has empty code but nonce {nonce}; refusing to overwrite a used account"
                )
            }
        }
    }
}

impl<DbError: core::error::Error + 'static> core::error::Error
    for SystemContractDeployError<DbError>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::ForeignCode { .. } |
            Self::ZeroFactoryNonce { .. } |
            Self::UsedEmptyAccount { .. } => None,
        }
    }
}

/// Deploys the contract `spec` describes, returning the [`EvmState`] witness. Nothing is
/// committed.
///
/// - Already deployed with [`SystemContractSpec::code_hash`]: the account as a read-only entry
///   (neither touched nor created, no seeding), so the witness records the read. A nonce of 7 stays
///   7. If [`SystemContractSpec::require_nonzero_nonce`] is set (the EIP-7997 factory) and the
///   nonce is 0, this is [`SystemContractDeployError::ZeroFactoryNonce`] instead: the matching-code
///   path does not rewrite a nonce, so a genesis factory with the right code and nonce 0 would
///   otherwise stay invalid forever. The six `MegaETH` contracts leave that flag unset; they
///   are not EIP-7997, and matching code is accepted regardless of nonce.
/// - Absent, or present with empty code and nonce 0: the created account with its bytecode, nonce
///   and every seeded slot, all marked. An existing balance is kept. Marking the account created
///   clears any storage it had; that is the accepted bootstrap path (prefunding with balance
///   alone). An empty-code account with a nonzero nonce is
///   [`SystemContractDeployError::UsedEmptyAccount`]: a used account at a system address is not
///   overwritten silently.
/// - Present with different non-empty code: [`SystemContractDeployError::ForeignCode`].
///
/// # Errors
///
/// [`SystemContractDeployError::Database`] when the account cannot be loaded;
/// [`SystemContractDeployError::ForeignCode`] when the address already holds other bytecode;
/// [`SystemContractDeployError::ZeroFactoryNonce`] when the factory holds matching code at
/// nonce 0;
/// [`SystemContractDeployError::UsedEmptyAccount`] when the address has empty code and a
/// used nonce.
pub fn transact_deploy<DB: Database>(
    db: &mut DB,
    spec: &SystemContractSpec,
) -> Result<EvmState, SystemContractDeployError<DB::Error>> {
    debug_assert_eq!(
        keccak256(spec.code.as_ref()),
        spec.code_hash,
        "SystemContractSpec code_hash does not match code for {:?}",
        spec.address
    );

    let existing = db.basic(spec.address).map_err(SystemContractDeployError::Database)?;

    if let Some(info) = &existing {
        if info.code_hash == spec.code_hash {
            if spec.require_nonzero_nonce && info.nonce == 0 {
                return Err(SystemContractDeployError::ZeroFactoryNonce { address: spec.address });
            }
            // `Account::from` copies the info and leaves status empty: neither touched nor
            // created, which is the read-only witness entry. An existing nonce is kept.
            return Ok(EvmState::from_iter([(spec.address, Account::from(info.clone()))]));
        }
        if info.code_hash != KECCAK_EMPTY {
            return Err(SystemContractDeployError::ForeignCode {
                address: spec.address,
                expected: spec.code_hash,
                found: info.code_hash,
            });
        }
        if info.nonce != 0 {
            return Err(SystemContractDeployError::UsedEmptyAccount {
                address: spec.address,
                nonce: info.nonce,
            });
        }
    }

    let mut info = existing.unwrap_or_default();
    info.code_hash = spec.code_hash;
    info.code = Some(Bytecode::new_raw(spec.code.clone()));
    info.nonce = spec.nonce;

    let mut account: Account = info.into();
    account.mark_touch();
    account.mark_created();
    for (slot, value) in &spec.seed {
        account
            .storage
            .insert(*slot, EvmStorageSlot::new_changed(U256::ZERO, *value, TransactionId::ZERO));
    }

    Ok(EvmState::from_iter([(spec.address, account)]))
}

/// The seven predeploys in deploy order: Oracle, high-precision timestamp, `KeylessDeploy`,
/// `MegaAccessControl`, `MegaLimitControl`, `SequencerRegistry`, then the EIP-7997 factory.
///
/// The registry's seeded slots come from `config`. The factory has nonce 1 and no storage.
pub fn system_contract_specs(
    config: &SequencerRegistryConfig,
) -> [SystemContractSpec; SYSTEM_CONTRACT_DEPLOY_COUNT] {
    [
        SystemContractSpec::new(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_CONTRACT_CODE,
            ORACLE_CONTRACT_CODE_HASH,
        ),
        SystemContractSpec::new(
            HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
            HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
            HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH,
        ),
        SystemContractSpec::new(
            KEYLESS_DEPLOY_ADDRESS,
            KEYLESS_DEPLOY_CODE,
            KEYLESS_DEPLOY_CODE_HASH,
        ),
        SystemContractSpec::new(
            ACCESS_CONTROL_ADDRESS,
            ACCESS_CONTROL_CODE,
            ACCESS_CONTROL_CODE_HASH,
        ),
        SystemContractSpec::new(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE, LIMIT_CONTROL_CODE_HASH),
        SystemContractSpec::new(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_REGISTRY_CODE,
            SEQUENCER_REGISTRY_CODE_HASH,
        )
        .with_seed(registry_seed(config)),
        SystemContractSpec::new(
            CREATE2_FACTORY_ADDRESS,
            CREATE2_FACTORY_CODE,
            CREATE2_FACTORY_CODE_HASH,
        )
        .require_nonzero_nonce(),
    ]
}

/// Encodes an address into its `U256` storage representation (standard Solidity address-in-slot).
fn address_to_storage_value(address: Address) -> U256 {
    U256::from_be_bytes(address.into_word().0)
}

/// The seven bootstrap slots the `SequencerRegistry` is created with.
fn registry_seed(config: &SequencerRegistryConfig) -> Vec<(U256, U256)> {
    Vec::from([
        (CURRENT_SYSTEM_ADDRESS, address_to_storage_value(config.initial_system_address)),
        (CURRENT_SEQUENCER, address_to_storage_value(config.initial_sequencer)),
        (ADMIN, address_to_storage_value(config.initial_admin)),
        (INITIAL_SYSTEM_ADDRESS, address_to_storage_value(config.initial_system_address)),
        (INITIAL_SEQUENCER, address_to_storage_value(config.initial_sequencer)),
        (INITIAL_FROM_BLOCK, U256::from(config.initial_from_block)),
        (MIN_ROTATION_DELAY, U256::from(config.min_rotation_delay)),
    ])
}
