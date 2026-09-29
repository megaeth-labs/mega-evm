//! The generators every property draws its cases from.
//!
//! A case is a small world — a handful of accounts at fixed addresses, the system contracts, a
//! SALT environment, the Oracle's answers, a block and a set of runtime limits — a transaction
//! into it, and the programs of the three contracts the transaction can reach. Every part is a
//! plain value with a `Debug` rendering, so a minimal failing case prints as something a
//! regression test can be written from.
//!
//! The address universe is fixed on purpose: with the same few accounts in every case a random
//! program hits the interesting collisions — a call to the block beneficiary, a value transfer to
//! an account another frame destroyed, a creation onto an address a delegation points at — far
//! more often than random addresses would.
//!
//! [`Flavor`] narrows the space for the neutral differential, which compares `MegaEvm` with
//! op-revm and revm's mainnet EVM: what only `MegaETH` has — the system contracts, the Oracle's
//! answers, the `SLOTNUM` opcode on the Osaka base — is left out there, because the references
//! cannot express it. Nothing else is narrowed.

pub(crate) mod case;
pub(crate) mod program;
pub(crate) mod tx;
pub(crate) mod world;

use alloy_primitives::{address, Address, U256};
use mega_evm::system::MEGA_SYSTEM_ADDRESS;
use proptest::prelude::*;

/// Which space the generators draw from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// Everything Satin runs: the whole space.
    Satin,
    /// What op-revm and revm's mainnet EVM run too: no system contract, no Oracle read, no
    /// `SLOTNUM`, no self-destruction inside init code (see the neutral differential).
    Neutral,
}

/// The sender of every user transaction.
pub(crate) const CALLER: Address = address!("00000000000000000000000000000000000fc000");
/// A second sender, for blocks.
pub(crate) const CALLER2: Address = address!("00000000000000000000000000000000000fc002");
/// The contract a call transaction targets: it runs the case's main program.
pub(crate) const CONTRACT: Address = address!("00000000000000000000000000000000000fc001");
/// A contract the programs can call, running its own program.
pub(crate) const A: Address = address!("00000000000000000000000000000000000fca01");
/// Another.
pub(crate) const B: Address = address!("00000000000000000000000000000000000fcb01");
/// An address no account exists at before the transaction.
pub(crate) const FRESH: Address = address!("00000000000000000000000000000000000fcf01");
/// The block beneficiary of a case that names a distinct one.
pub(crate) const BENEFICIARY: Address = address!("00000000000000000000000000000000000fbef0");

/// An account of the fixed universe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Who {
    Caller,
    Contract,
    A,
    B,
    Fresh,
    Beneficiary,
}

impl Who {
    pub(crate) const ALL: [Self; 6] =
        [Self::Caller, Self::Contract, Self::A, Self::B, Self::Fresh, Self::Beneficiary];

    /// The account's address; the beneficiary's is the distinct one, whoever the case's block
    /// names (see `World::beneficiary_address`).
    pub(crate) const fn address(self) -> Address {
        match self {
            Self::Caller => CALLER,
            Self::Contract => CONTRACT,
            Self::A => A,
            Self::B => B,
            Self::Fresh => FRESH,
            Self::Beneficiary => BENEFICIARY,
        }
    }
}

pub(crate) fn who() -> impl Strategy<Value = Who> {
    proptest::sample::select(Who::ALL.as_slice())
}

/// An amount of wei a case moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Zero,
    One,
    /// A billion wei: below every funded balance.
    Some,
    /// More than any account holds.
    Huge,
}

impl Value {
    pub(crate) fn wei(self) -> U256 {
        match self {
            Self::Zero => U256::ZERO,
            Self::One => U256::from(1),
            Self::Some => U256::from(1_000_000_000u64),
            Self::Huge => U256::from(10u128.pow(30)),
        }
    }
}

pub(crate) fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        5 => Just(Value::Zero),
        3 => Just(Value::One),
        2 => Just(Value::Some),
        1 => Just(Value::Huge),
    ]
}

/// A storage word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Word {
    Zero,
    One,
    Max,
    Other(u64),
}

impl Word {
    pub(crate) fn u256(self) -> U256 {
        match self {
            Self::Zero => U256::ZERO,
            Self::One => U256::from(1),
            Self::Max => U256::MAX,
            Self::Other(v) => U256::from(v),
        }
    }
}

pub(crate) fn word() -> impl Strategy<Value = Word> {
    prop_oneof![
        3 => Just(Word::Zero),
        3 => Just(Word::One),
        1 => Just(Word::Max),
        2 => (2u64..1_000_000).prop_map(Word::Other),
    ]
}

/// The live system address every case's registry names.
pub(crate) const SYSTEM_ADDRESS: Address = MEGA_SYSTEM_ADDRESS;

/// The chain id every case runs on: revm's default, so a transaction that names none is valid.
pub(crate) const CHAIN_ID: u64 = 1;
