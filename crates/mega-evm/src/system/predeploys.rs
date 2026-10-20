//! The predeploys Satin runs on, and the check its activation block makes of them.
//!
//! [`system_contract_specs`] lists them: the six `MegaETH` system contracts in address order, then
//! the EIP-7997 `CREATE2` factory, each with the code Satin expects (the bytecode
//! `mega-system-contracts` ships; the Oracle and the `SequencerRegistry` are their v2.0.0). A chain
//! reaches Satin through the legacy forks, which install the six, and OP's genesis tooling, which
//! preinstalls the factory, so Satin deploys none of them.
//!
//! The block in which Satin activates checks once that each address holds that code
//! ([`check_system_contract`]); no other block reads them. The check returns its read as a state
//! and never commits; the block executor hands it to its pre-block observer, so the activation
//! block's witness carries the seven accounts.

use core::fmt;

use alloy_primitives::{address, b256, bytes, Address, Bytes, B256};
use revm::{
    primitives::KECCAK_EMPTY,
    state::{Account, EvmState},
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

/// How many predeploys [`system_contract_specs`] returns: the six `MegaETH` system contracts and
/// the EIP-7997 factory.
pub const SYSTEM_CONTRACT_COUNT: usize = 7;

/// The EIP-7997 `CREATE2` factory address (Arachnid's deterministic deployment proxy).
pub const CREATE2_FACTORY_ADDRESS: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");

/// Runtime bytecode of the EIP-7997 factory.
pub const CREATE2_FACTORY_CODE: Bytes =
    bytes!("0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3");

/// Keccak-256 hash of [`CREATE2_FACTORY_CODE`].
pub const CREATE2_FACTORY_CODE_HASH: B256 =
    b256!("0x2fa86add0aed31f33a762c9d88e807c475bd51d0f52bd0955754b2608f7e4989");

/// A predeploy: the address and the code Satin expects there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemContractSpec {
    /// The predeploy's address.
    pub address: Address,
    /// The runtime bytecode the address holds, which a test or a tool installs for a state of
    /// its own.
    pub code: Bytes,
    /// Keccak-256 hash of [`code`](Self::code): what the activation check compares.
    pub code_hash: B256,
}

impl SystemContractSpec {
    /// The predeploy at `address` holding `code`, whose hash is `code_hash`.
    pub const fn new(address: Address, code: Bytes, code_hash: B256) -> Self {
        Self { address, code, code_hash }
    }
}

/// Why [`check_system_contract`] refused a predeploy.
#[derive(Debug)]
pub enum SystemContractCheckError<DbError> {
    /// The database could not load the account at the predeploy's address.
    Database(DbError),
    /// The address does not hold the code Satin expects: it holds other code, an older version
    /// of the contract, no code, or no account.
    UnexpectedCode {
        /// The predeploy's address.
        address: Address,
        /// The code hash Satin expects there.
        expected: B256,
        /// The code hash the address holds: the empty code's for no code or no account.
        found: B256,
    },
}

impl<DbError> From<DbError> for SystemContractCheckError<DbError> {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

impl<DbError: fmt::Display> fmt::Display for SystemContractCheckError<DbError> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(f, "{error}"),
            Self::UnexpectedCode { address, expected, found } => {
                write_unexpected_code(f, *address, *expected, *found)
            }
        }
    }
}

/// The refusal of a predeploy at `address` that holds the code hash `found` where `expected` must
/// be when Satin activates.
pub(crate) fn write_unexpected_code(
    f: &mut fmt::Formatter<'_>,
    address: Address,
    expected: B256,
    found: B256,
) -> fmt::Result {
    write!(f, "the predeploy at {address} must hold code hash {expected} when Satin activates; ")?;
    if found == KECCAK_EMPTY {
        f.write_str("it holds no code")
    } else {
        write!(f, "it holds code hash {found}")
    }
}

impl<DbError: core::error::Error + 'static> core::error::Error
    for SystemContractCheckError<DbError>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::UnexpectedCode { .. } => None,
        }
    }
}

/// Checks that `spec.address` holds `spec.code_hash`, returning the account as a read-only entry
/// (neither touched nor created, no storage) for the witness. Nothing is written or committed.
///
/// The code alone is checked: the nonce, the balance and the storage are whatever the chain holds.
///
/// # Errors
///
/// [`SystemContractCheckError::Database`] when the account cannot be loaded, and
/// [`SystemContractCheckError::UnexpectedCode`] when the address holds no account, no code, or
/// any other code than the expected one.
pub fn check_system_contract<DB: Database>(
    db: &mut DB,
    spec: &SystemContractSpec,
) -> Result<EvmState, SystemContractCheckError<DB::Error>> {
    let existing = db.basic(spec.address).map_err(SystemContractCheckError::Database)?;
    match existing {
        // `Account::from` copies the info and leaves the status empty: neither touched nor
        // created, which is the read-only witness entry.
        Some(info) if info.code_hash == spec.code_hash => {
            Ok(EvmState::from_iter([(spec.address, Account::from(info))]))
        }
        existing => Err(SystemContractCheckError::UnexpectedCode {
            address: spec.address,
            expected: spec.code_hash,
            found: existing.map_or(KECCAK_EMPTY, |info| info.code_hash),
        }),
    }
}

/// The seven predeploys in the order the activation block checks them: Oracle, high-precision
/// timestamp, `KeylessDeploy`, `MegaAccessControl`, `MegaLimitControl`, `SequencerRegistry`, then
/// the EIP-7997 factory.
pub fn system_contract_specs() -> [SystemContractSpec; SYSTEM_CONTRACT_COUNT] {
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
        ),
        SystemContractSpec::new(
            CREATE2_FACTORY_ADDRESS,
            CREATE2_FACTORY_CODE,
            CREATE2_FACTORY_CODE_HASH,
        ),
    ]
}

/// The addresses of [`system_contract_specs`], in its order: the six `MegaETH` contracts, then the
/// EIP-7997 factory.
pub const SYSTEM_CONTRACT_ADDRESSES: [Address; SYSTEM_CONTRACT_COUNT] = [
    ORACLE_CONTRACT_ADDRESS,
    HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
    KEYLESS_DEPLOY_ADDRESS,
    ACCESS_CONTROL_ADDRESS,
    LIMIT_CONTROL_ADDRESS,
    SEQUENCER_REGISTRY_ADDRESS,
    CREATE2_FACTORY_ADDRESS,
];
