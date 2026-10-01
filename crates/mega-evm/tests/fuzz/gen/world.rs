//! The world a transaction runs in: the accounts, the SALT capacities, the Oracle's answers, the
//! block and the runtime limits.

use alloy_primitives::{Address, U256};
use mega_evm::{
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, MAX_TX_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS},
    satin_gas_params, EvmTxRuntimeLimits, ProtocolLimits, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use proptest::prelude::*;
use revm::context_interface::cfg::GasId;

use super::{word, Who, Word, BENEFICIARY, SYSTEM_ADDRESS};

/// A balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Balance {
    Zero,
    /// Ten million wei: a few values, no fees.
    Small,
    /// An ether: any fee the cases charge.
    Rich,
}

impl Balance {
    pub(crate) fn wei(self) -> U256 {
        match self {
            Self::Zero => U256::ZERO,
            Self::Small => U256::from(10_000_000u64),
            Self::Rich => U256::from(10u64.pow(18)),
        }
    }
}

fn balance() -> impl Strategy<Value = Balance> {
    prop_oneof![2 => Just(Balance::Zero), 2 => Just(Balance::Small), 4 => Just(Balance::Rich)]
}

/// An account of the pre-state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountShape {
    pub(crate) balance: Balance,
    pub(crate) nonce: u8,
    /// Slots set before the transaction, so a write can restore one to its original value.
    pub(crate) slots: Vec<(u8, Word)>,
}

fn account() -> impl Strategy<Value = AccountShape> {
    account_with(balance())
}

/// The sender's account: rich nearly always, so most transactions pay their way in.
fn sender() -> impl Strategy<Value = AccountShape> {
    account_with(
        prop_oneof![1 => Just(Balance::Zero), 2 => Just(Balance::Small), 21 => Just(Balance::Rich)],
    )
}

fn account_with(balance: impl Strategy<Value = Balance>) -> impl Strategy<Value = AccountShape> {
    (balance, 0u8..3, proptest::collection::vec((0u8..4, word()), 0..=2))
        .prop_map(|(balance, nonce, slots)| AccountShape { balance, nonce, slots })
}

/// Who the block beneficiary is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeneficiaryIs {
    /// The distinct beneficiary account.
    Distinct,
    /// `A`, so a call to `A` reads the beneficiary's account.
    A,
    /// The sender, so the transaction is detained from its start.
    Caller,
    /// The contract the transaction calls.
    Contract,
    /// An address with no account.
    Fresh,
}

fn beneficiary() -> impl Strategy<Value = BeneficiaryIs> {
    prop_oneof![
        4 => Just(BeneficiaryIs::Distinct),
        2 => Just(BeneficiaryIs::A),
        1 => Just(BeneficiaryIs::Caller),
        1 => Just(BeneficiaryIs::Contract),
        1 => Just(BeneficiaryIs::Fresh),
    ]
}

/// An EIP-7702 delegation in the pre-state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Delegation {
    pub(crate) delegator: Who,
    pub(crate) delegate: Who,
}

fn delegation() -> impl Strategy<Value = Option<Delegation>> {
    proptest::option::weighted(
        0.25,
        (
            prop_oneof![Just(Who::Caller), Just(Who::A), Just(Who::B)],
            prop_oneof![Just(Who::Contract), Just(Who::A), Just(Who::B), Just(Who::Beneficiary)],
        )
            .prop_map(|(delegator, delegate)| Delegation { delegator, delegate }),
    )
}

/// The SALT capacities: a default multiplier for every bucket, and the buckets of a few accounts
/// and slots crowded further.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Salt {
    /// The multiplier of every bucket not named below, in minimum buckets.
    pub(crate) default_multiplier: u8,
    /// Accounts whose own bucket is crowded to the multiplier.
    pub(crate) crowded_accounts: Vec<(Who, u8)>,
    /// Slots whose bucket is crowded to the multiplier.
    pub(crate) crowded_slots: Vec<(Who, u8, u8)>,
    /// An account whose bucket cannot be read: a charge landing in it fails the transaction.
    pub(crate) failing_account: Option<Who>,
}

fn salt() -> impl Strategy<Value = Salt> {
    (
        prop_oneof![6 => Just(1u8), 2 => Just(2), 1 => Just(3), 1 => Just(7)],
        proptest::collection::vec((super::who(), 2u8..=9), 0..=2),
        proptest::collection::vec((super::who(), 0u8..4, 2u8..=9), 0..=2),
        proptest::option::weighted(0.1, super::who()),
    )
        .prop_map(|(default_multiplier, crowded_accounts, crowded_slots, failing_account)| {
            Salt { default_multiplier, crowded_accounts, crowded_slots, failing_account }
        })
}

/// The transaction's data-size limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataLimit {
    /// Exactly the body: the least a chain may set.
    Body,
    /// The body and one record.
    BodyPlusRecord,
    /// A few hundred bytes past the body.
    Small(u16),
    /// 100,000 bytes.
    Medium,
    Unlimited,
}

impl DataLimit {
    fn value(self) -> u64 {
        match self {
            Self::Body => TX_BODY_SIZE,
            Self::BodyPlusRecord => TX_BODY_SIZE + WRITE_RECORD_SIZE,
            Self::Small(extra) => TX_BODY_SIZE + extra as u64,
            Self::Medium => 100_000,
            Self::Unlimited => u64::MAX,
        }
    }
}

/// The transaction's state-gas limit, in slots at the byte prices in effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateLimit {
    /// One gas: crossed by the first charge, unless a state byte costs nothing.
    One,
    /// Half a fresh slot.
    HalfSlot,
    /// One fresh slot.
    Slot,
    /// Three fresh slots.
    ThreeSlots,
    /// A hundred.
    HundredSlots,
    Unlimited,
}

impl StateLimit {
    fn value(self) -> u64 {
        let slot = satin_gas_params().get(GasId::sstore_set_state_gas());
        match self {
            Self::One => 1,
            Self::HalfSlot => (slot / 2).max(1),
            Self::Slot => slot.max(1),
            Self::ThreeSlots => (3 * slot).max(1),
            Self::HundredSlots => (100 * slot).max(1),
            Self::Unlimited => u64::MAX,
        }
    }
}

/// A gas detention cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetentionCap {
    /// 50,000: crossed by a few thousand instructions after the read.
    Tiny,
    /// 300,000.
    Small,
    /// 2,000,000.
    Medium,
    /// The spec's.
    Default,
    /// The most a chain may set: one below what no transaction's compute reaches.
    Max,
}

impl DetentionCap {
    fn value(self, spec: u64) -> u64 {
        match self {
            Self::Tiny => 50_000,
            Self::Small => 300_000,
            Self::Medium => 2_000_000,
            Self::Default => spec,
            Self::Max => MAX_TX_COMPUTE_GAS - 1,
        }
    }
}

fn detention_cap() -> impl Strategy<Value = DetentionCap> {
    prop_oneof![
        3 => Just(DetentionCap::Tiny),
        2 => Just(DetentionCap::Small),
        1 => Just(DetentionCap::Medium),
        3 => Just(DetentionCap::Default),
        1 => Just(DetentionCap::Max),
    ]
}

/// The runtime limits a transaction is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limits {
    /// The spec's: `ProtocolLimits::DEFAULT`.
    Default,
    /// The loosest a chain may carry.
    Loosest,
    /// A valid set of tight limits.
    Custom {
        data: DataLimit,
        frame_data: Option<u16>,
        kv: Option<u8>,
        frame_kv: Option<u8>,
        state: StateLimit,
        block_env_cap: DetentionCap,
        oracle_cap: DetentionCap,
    },
}

impl Limits {
    /// The limits, which a chain may carry (`ProtocolLimits::validate` accepts them).
    pub(crate) fn runtime(self) -> EvmTxRuntimeLimits {
        match self {
            Self::Default => ProtocolLimits::DEFAULT.tx_runtime_limits,
            Self::Loosest => ProtocolLimits::loosest().tx_runtime_limits,
            Self::Custom { data, frame_data, kv, frame_kv, state, block_env_cap, oracle_cap } => {
                EvmTxRuntimeLimits {
                    tx_data_size_limit: data.value(),
                    frame_data_size_limit: frame_data.map_or(u64::MAX, u64::from),
                    tx_kv_update_limit: kv.map_or(u64::MAX, u64::from),
                    frame_kv_update_limit: frame_kv.map_or(u64::MAX, u64::from),
                    tx_state_gas_limit: state.value(),
                    block_env_access_compute_gas_limit: block_env_cap
                        .value(BLOCK_ENV_ACCESS_COMPUTE_GAS),
                    oracle_access_compute_gas_limit: oracle_cap.value(ORACLE_ACCESS_COMPUTE_GAS),
                }
            }
        }
    }
}

pub(crate) fn limits() -> impl Strategy<Value = Limits> {
    let custom = (
        // The data size is checked before the records wherever both cross, so a KV stop needs a
        // data-size limit with room: the roomy ones are drawn as often as the tight ones, and the
        // KV limit is mostly one or two records.
        prop_oneof![
            1 => Just(DataLimit::Body),
            2 => Just(DataLimit::BodyPlusRecord),
            3 => (1u16..=2_000).prop_map(DataLimit::Small),
            3 => Just(DataLimit::Medium),
            3 => Just(DataLimit::Unlimited),
        ],
        proptest::option::weighted(0.3, prop_oneof![Just(1u16), Just(40), Just(200), Just(2_000)]),
        proptest::option::weighted(0.5, prop_oneof![3 => 1u8..=2, 2 => 3u8..=6]),
        proptest::option::weighted(0.3, 1u8..=3),
        prop_oneof![
            1 => Just(StateLimit::One),
            1 => Just(StateLimit::HalfSlot),
            2 => Just(StateLimit::Slot),
            2 => Just(StateLimit::ThreeSlots),
            1 => Just(StateLimit::HundredSlots),
            3 => Just(StateLimit::Unlimited),
        ],
        detention_cap(),
        detention_cap(),
    )
        .prop_map(|(data, frame_data, kv, frame_kv, state, block_env_cap, oracle_cap)| {
            Limits::Custom { data, frame_data, kv, frame_kv, state, block_env_cap, oracle_cap }
        });
    prop_oneof![3 => Just(Limits::Default), 2 => Just(Limits::Loosest), 5 => custom]
}

/// The block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockShape {
    /// The base fee: zero, or seven, which a free transaction is refused under.
    pub(crate) basefee: u64,
    /// The slot number `SLOTNUM` reads, zero for a block that leaves it unset.
    pub(crate) slot_num: u64,
    /// Whether the block carries a blob price; one that does not refuses every transaction but a
    /// deposit on its header.
    pub(crate) blob: bool,
}

fn block() -> impl Strategy<Value = BlockShape> {
    // A base fee refuses every transaction priced below it, and a block with no blob price every
    // transaction but a deposit: each is drawn often enough to be covered and no more.
    (
        prop_oneof![15 => Just(0u64), 1 => Just(7)],
        prop_oneof![1 => Just(0u64), 1 => Just(9)],
        prop_oneof![31 => Just(true), 1 => Just(false)],
    )
        .prop_map(|(basefee, slot_num, blob)| BlockShape { basefee, slot_num, blob })
}

/// The world a case runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct World {
    pub(crate) caller: AccountShape,
    pub(crate) contract: AccountShape,
    pub(crate) a: AccountShape,
    pub(crate) b: AccountShape,
    /// The distinct beneficiary's balance.
    pub(crate) beneficiary_balance: Balance,
    pub(crate) beneficiary: BeneficiaryIs,
    pub(crate) delegation: Option<Delegation>,
    pub(crate) salt: Salt,
    /// The Oracle's answers, by slot.
    pub(crate) oracle: Vec<(u8, Word)>,
    pub(crate) limits: Limits,
    pub(crate) block: BlockShape,
    /// The system address's nonce.
    pub(crate) system_nonce: u8,
}

impl World {
    /// The nonce `address` has before the transaction.
    pub(crate) fn nonce_of(&self, address: Address) -> u64 {
        if address == Who::Caller.address() {
            self.caller.nonce as u64
        } else if address == Who::Contract.address() {
            self.contract.nonce as u64
        } else if address == Who::A.address() {
            self.a.nonce as u64
        } else if address == Who::B.address() {
            self.b.nonce as u64
        } else if address == SYSTEM_ADDRESS {
            self.system_nonce as u64
        } else {
            0
        }
    }

    /// The block beneficiary's address.
    pub(crate) fn beneficiary_address(&self) -> Address {
        match self.beneficiary {
            BeneficiaryIs::Distinct => BENEFICIARY,
            BeneficiaryIs::A => Who::A.address(),
            BeneficiaryIs::Caller => Who::Caller.address(),
            BeneficiaryIs::Contract => Who::Contract.address(),
            BeneficiaryIs::Fresh => Who::Fresh.address(),
        }
    }
}

pub(crate) fn world() -> impl Strategy<Value = World> {
    (
        (sender(), account(), account(), account()),
        balance(),
        beneficiary(),
        delegation(),
        salt(),
        proptest::collection::vec((0u8..4, word()), 0..=2),
        limits(),
        block(),
        0u8..2,
    )
        .prop_map(
            |(
                (caller, contract, a, b),
                beneficiary_balance,
                beneficiary,
                delegation,
                salt,
                oracle,
                limits,
                block,
                system_nonce,
            )| World {
                caller,
                contract,
                a,
                b,
                beneficiary_balance,
                beneficiary,
                delegation,
                salt,
                oracle,
                limits,
                block,
                system_nonce,
            },
        )
}
