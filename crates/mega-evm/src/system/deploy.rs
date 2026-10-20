//! Shared primitive for deploying `MegaETH` system contracts and the EIP-7997 factory.
//!
//! Every predeploy is one state patch: install the account's bytecode and mark the account
//! touched and created. [`SystemContractSpec`] names those differences; [`transact_deploy`]
//! applies them and never commits. The caller — the block executor today, the CLI later — commits
//! the returned [`EvmState`].
//!
//! [`system_contract_specs`] is the single list both iterate: the six `MegaETH` contracts in
//! address order, then the factory. A spec names the version the engine runs (the bytecode
//! `mega-system-contracts` ships; the Oracle and the `SequencerRegistry` are their v2.0.0) and the
//! older versions of the contract that chains hold: an address holding one of those is upgraded in
//! place, its storage kept, and an address holding any other code is an error. The
//! `SequencerRegistry` is the exception: the legacy forks created it (Rex5) and upgraded it to
//! v2.0.0 (Rex6), so the deploy only checks that it is there, at v2.0.0, and neither creates nor
//! upgrades it.
//!
//! [`check_genesis_account`] is the one check made before any block: a deploy over an account
//! that holds storage neither writes nor clears that storage, and only a genesis allocation can
//! put storage there, so a node refuses such an allocation when it loads its chain configuration.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::fmt;
use std::vec::Vec;

use alloy_primitives::{address, b256, bytes, keccak256, Address, Bytes, B256, U256};
use mega_system_contracts::{oracle, sequencer_registry};
use revm::{
    primitives::KECCAK_EMPTY,
    state::{Account, Bytecode, EvmState},
    Database,
};

use super::{
    keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_CODE_HASH},
    ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, ACCESS_CONTROL_CODE_HASH,
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
    /// When set, matching code with nonce 0 is an error rather than a read-only entry.
    ///
    /// EIP-7997 requires the factory to hold its runtime **and a nonzero nonce**. The six
    /// `MegaETH` contracts are not EIP-7997, so they leave this unset: matching code is
    /// accepted regardless of nonce, because the matching-code path must not rewrite an
    /// already-deployed contract.
    pub require_nonzero_nonce: bool,
    /// When set, an earlier fork deployed this contract and the deploy neither creates nor
    /// upgrades it: the account must already hold [`code`](Self::code), and anything else is an
    /// error.
    ///
    /// The `SequencerRegistry` sets it: the legacy forks created it (Rex5) and upgraded it to
    /// v2.0.0 (Rex6), and a chain that activates Satin has run them.
    pub require_deployed: bool,
    /// Code hashes of the older versions of this contract that a chain may hold. An account
    /// holding one of them is upgraded in place to [`code`](Self::code): the code and its hash
    /// are replaced, and the nonce, the balance and the storage stay. For a contract with
    /// [`require_deployed`](Self::require_deployed) set it is refused instead, as not deployed.
    /// Empty for a contract that has only ever had one version.
    pub older_versions: Vec<B256>,
}

impl SystemContractSpec {
    /// A spec with nonce 1, no nonce requirement on a matching account, deployed when absent, and
    /// no older version.
    pub fn new(address: Address, code: Bytes, code_hash: B256) -> Self {
        Self {
            address,
            code,
            code_hash,
            nonce: 1,
            require_nonzero_nonce: false,
            require_deployed: false,
            older_versions: Vec::new(),
        }
    }

    /// Names the older versions of the contract, by code hash.
    pub fn with_older_versions(mut self, code_hashes: impl IntoIterator<Item = B256>) -> Self {
        self.older_versions = code_hashes.into_iter().collect();
        self
    }

    /// Requires a matching existing account to hold a nonzero nonce (EIP-7997).
    pub fn require_nonzero_nonce(mut self) -> Self {
        self.require_nonzero_nonce = true;
        self
    }

    /// Requires the contract to be deployed already, by an earlier fork.
    pub fn require_deployed(mut self) -> Self {
        self.require_deployed = true;
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
    /// The `SequencerRegistry` is not deployed at v2.0.0: its address holds no account, an
    /// account without code, or an older version of the contract.
    ///
    /// The legacy forks deploy it and upgrade it to v2.0.0, and the deploy neither creates nor
    /// upgrades it ([`SystemContractSpec::require_deployed`]).
    SequencerRegistryNotDeployed {
        /// The registry's address.
        address: Address,
        /// The code hash of v2.0.0.
        expected: B256,
        /// The code hash the address holds: the empty code's for no account or no code.
        found: B256,
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
            Self::SequencerRegistryNotDeployed { address, expected, found } => {
                write_registry_not_deployed(f, *address, *expected, *found)
            }
        }
    }
}

/// The refusal of a chain whose `SequencerRegistry` at `address` holds the code hash `found`
/// where v2.0.0's, `expected`, must already be.
pub(crate) fn write_registry_not_deployed(
    f: &mut fmt::Formatter<'_>,
    address: Address,
    expected: B256,
    found: B256,
) -> fmt::Result {
    write!(
        f,
        "the SequencerRegistry at {address} must already be deployed at v2.0.0 (code hash \
         {expected}) by the legacy forks, and Satin neither creates nor upgrades it; "
    )?;
    if found == KECCAK_EMPTY {
        f.write_str("the address holds no code")
    } else {
        write!(f, "the address holds code hash {found}")
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
            Self::UsedEmptyAccount { .. } |
            Self::SequencerRegistryNotDeployed { .. } => None,
        }
    }
}

/// Deploys the contract `spec` describes, returning the [`EvmState`] witness. Nothing is
/// committed.
///
/// - Already deployed with [`SystemContractSpec::code_hash`]: the account as a read-only entry
///   (neither touched nor created, no storage), so the witness records the read. A nonce of 7 stays
///   7. If [`SystemContractSpec::require_nonzero_nonce`] is set (the EIP-7997 factory) and the
///   nonce is 0, this is [`SystemContractDeployError::ZeroFactoryNonce`] instead: the matching-code
///   path does not rewrite a nonce, so a genesis factory with the right code and nonce 0 would
///   otherwise stay invalid forever. The six `MegaETH` contracts leave that flag unset; they are
///   not EIP-7997, and matching code is accepted regardless of nonce.
/// - Anything else, when [`SystemContractSpec::require_deployed`] is set (the `SequencerRegistry`):
///   no account, an account without code, or one holding a version in
///   [`SystemContractSpec::older_versions`] is
///   [`SystemContractDeployError::SequencerRegistryNotDeployed`], and any other code
///   [`SystemContractDeployError::ForeignCode`]. Nothing is created or upgraded.
/// - Absent, or present with empty code and nonce 0: the created account with its bytecode and
///   nonce, marked. An existing balance is kept; prefunding with balance alone is the accepted
///   bootstrap path. Storage the account held is neither written nor cleared: the returned state
///   carries no change to those slots, so the state a block commits still holds them, while the
///   created mark has the state layer answer them as zero for the rest of the block, dropping a
///   value its cache held. Only a genesis allocation can put storage on such an account, and
///   [`check_genesis_account`] refuses one that does. An empty-code account with a nonzero nonce is
///   [`SystemContractDeployError::UsedEmptyAccount`]: a used account at a system address is not
///   overwritten silently.
/// - Present with the code of a version in [`SystemContractSpec::older_versions`]: upgraded in
///   place. The account is returned touched and not created, with the new code and its hash, its
///   nonce and balance as they were, and no storage entry: a created mark would have the state
///   layer answer the contract's storage as zero, so the slots are neither written nor cleared and
///   the contract keeps what it holds. The witness of the step is that entry, whose reads are the
///   account the block loaded.
/// - Present with any other non-empty code: [`SystemContractDeployError::ForeignCode`].
///
/// # Errors
///
/// [`SystemContractDeployError::Database`] when the account cannot be loaded;
/// [`SystemContractDeployError::ForeignCode`] when the address already holds other bytecode;
/// [`SystemContractDeployError::ZeroFactoryNonce`] when the factory holds matching code at
/// nonce 0;
/// [`SystemContractDeployError::UsedEmptyAccount`] when the address has empty code and a
/// used nonce;
/// [`SystemContractDeployError::SequencerRegistryNotDeployed`] when the registry is not there at
/// v2.0.0.
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
    }

    if spec.require_deployed {
        let found = existing.map_or(KECCAK_EMPTY, |info| info.code_hash);
        let (address, expected) = (spec.address, spec.code_hash);
        if found == KECCAK_EMPTY || spec.older_versions.contains(&found) {
            return Err(SystemContractDeployError::SequencerRegistryNotDeployed {
                address,
                expected,
                found,
            });
        }
        return Err(SystemContractDeployError::ForeignCode { address, expected, found });
    }

    if let Some(info) = &existing {
        if spec.older_versions.contains(&info.code_hash) {
            let mut upgraded = info.clone();
            upgraded.code_hash = spec.code_hash;
            upgraded.code = Some(Bytecode::new_raw(spec.code.clone()));
            let mut account: Account = upgraded.into();
            account.mark_touch();
            return Ok(EvmState::from_iter([(spec.address, account)]));
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

    Ok(EvmState::from_iter([(spec.address, account)]))
}

/// The seven predeploys in deploy order: Oracle, high-precision timestamp, `KeylessDeploy`,
/// `MegaAccessControl`, `MegaLimitControl`, `SequencerRegistry`, then the EIP-7997 factory.
///
/// The Oracle is upgraded in place from its older versions. The `SequencerRegistry` must already
/// hold v2.0.0, which the legacy forks deployed: the deploy only reads it. The factory has nonce
/// 1 and no storage.
pub fn system_contract_specs() -> [SystemContractSpec; SYSTEM_CONTRACT_DEPLOY_COUNT] {
    [
        SystemContractSpec::new(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_CONTRACT_CODE,
            ORACLE_CONTRACT_CODE_HASH,
        )
        .with_older_versions([oracle::V1_0_0_CODE_HASH, oracle::V1_1_0_CODE_HASH]),
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
        .require_deployed()
        .with_older_versions([sequencer_registry::V1_0_0_CODE_HASH]),
        SystemContractSpec::new(
            CREATE2_FACTORY_ADDRESS,
            CREATE2_FACTORY_CODE,
            CREATE2_FACTORY_CODE_HASH,
        )
        .require_nonzero_nonce(),
    ]
}

/// The addresses [`system_contract_specs`] deploys at, in its order: the six `MegaETH` contracts,
/// then the EIP-7997 factory.
pub const SYSTEM_CONTRACT_ADDRESSES: [Address; SYSTEM_CONTRACT_DEPLOY_COUNT] = [
    ORACLE_CONTRACT_ADDRESS,
    HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
    KEYLESS_DEPLOY_ADDRESS,
    ACCESS_CONTROL_ADDRESS,
    LIMIT_CONTROL_ADDRESS,
    SEQUENCER_REGISTRY_ADDRESS,
    CREATE2_FACTORY_ADDRESS,
];

/// A genesis allocation the system-contract deploy cannot run over, as
/// [`check_genesis_account`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenesisAllocationError {
    /// The allocation gives `address`, where a system contract or the factory is deployed, an
    /// account with empty code, nonce 0 and storage.
    ///
    /// The deploy installs the code over it and neither writes nor clears the storage: the state
    /// a block commits keeps the slots, while the block that deploys reads them as zero, so two
    /// nodes, or one node before and after it persists the block, would read them differently.
    StorageWithoutCode {
        /// The predeploy address.
        address: Address,
    },
}

impl fmt::Display for GenesisAllocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageWithoutCode { address } => write!(
                f,
                "the genesis allocation gives the predeploy address {address} storage without \
                 code and at nonce 0, which the system-contract deploy neither writes nor clears"
            ),
        }
    }
}

impl core::error::Error for GenesisAllocationError {}

/// Refuses an account of a genesis allocation that the system-contract deploy would run over
/// without clearing what it holds: one at an address the deploy installs a contract at (every
/// predeploy of [`system_contract_specs`] but the `SequencerRegistry`) with empty `code`, `nonce`
/// 0 and a slot of `storage`, the values of its storage, that is not zero.
///
/// A node calls it on every account of its genesis allocation when it loads a chain
/// configuration, as it checks the configuration's Satin keys
/// ([`SatinChainConfig::from_genesis_config`](crate::SatinChainConfig::from_genesis_config)):
/// only a genesis allocation can put storage on an account with empty code and nonce 0, and a
/// deploy over one ([`transact_deploy`]) leaves the slots to the state layer. Any other account
/// passes: one holding the contract's own code is only read by the deploy, a slot holding zero is
/// no storage, and the deploy refuses every other account it cannot install the contract on —
/// other code, or a used nonce — at the block that runs it. The `SequencerRegistry`'s address
/// passes too: the deploy installs nothing there, and refuses the block unless the address
/// already holds v2.0.0.
///
/// # Errors
///
/// [`GenesisAllocationError::StorageWithoutCode`], naming the address.
pub fn check_genesis_account(
    address: Address,
    code: &[u8],
    nonce: u64,
    storage: impl IntoIterator<Item = U256>,
) -> Result<(), GenesisAllocationError> {
    if code.is_empty() &&
        nonce == 0 &&
        storage.into_iter().any(|value| !value.is_zero()) &&
        system_contract_specs()
            .iter()
            .any(|spec| spec.address == address && !spec.require_deployed)
    {
        return Err(GenesisAllocationError::StorageWithoutCode { address });
    }
    Ok(())
}
