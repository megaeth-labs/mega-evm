//! Random transactions: a call, a creation, an EIP-7702 transaction with its authorizations, an
//! access-list transaction, a deposit, a system-address transaction and a keyless deployment,
//! each at a gas limit below or above the execution cap.

use alloy_op_evm::OpTx;
use alloy_primitives::{hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    constants::TX_GAS_LIMIT_CAP,
    system::{
        keyless::{IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS},
        IOracle, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::op_transaction,
    MegaTransaction,
};
use proptest::prelude::*;
use revm::{
    context::{
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    context_interface::{
        either::Either,
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    },
};

use super::{
    program::InitCode, value, who, world::World, Flavor, Value, Who, CALLER, CHAIN_ID,
    SYSTEM_ADDRESS,
};

/// A gas limit, by where it stands to the execution cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GasTier {
    /// 60,000: room for little beyond the intrinsic cost.
    Tiny,
    /// 400,000.
    Small,
    /// 4,000,000.
    Medium,
    /// 30,000,000: past gas detention's default cap.
    Large,
    /// 50,000,000 above the execution cap: a reservoir of that much.
    AboveCap,
}

impl GasTier {
    pub(crate) const fn limit(self) -> u64 {
        match self {
            Self::Tiny => 60_000,
            Self::Small => 400_000,
            Self::Medium => 4_000_000,
            Self::Large => 30_000_000,
            Self::AboveCap => TX_GAS_LIMIT_CAP + 50_000_000,
        }
    }
}

fn gas_tier() -> impl Strategy<Value = GasTier> {
    prop_oneof![
        1 => Just(GasTier::Tiny),
        3 => Just(GasTier::Small),
        4 => Just(GasTier::Medium),
        2 => Just(GasTier::Large),
        1 => Just(GasTier::AboveCap),
    ]
}

/// A gas price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Price {
    Free,
    One,
    Dear,
}

impl Price {
    pub(crate) const fn wei(self) -> u128 {
        match self {
            Self::Free => 0,
            Self::One => 1,
            Self::Dear => 1_000,
        }
    }
}

fn price() -> impl Strategy<Value = Price> {
    prop_oneof![5 => Just(Price::Free), 2 => Just(Price::One), 1 => Just(Price::Dear)]
}

/// Where an authority delegates to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delegate {
    Contract,
    A,
    B,
    /// Clears the delegation.
    Zero,
    Beneficiary,
}

impl Delegate {
    fn address(self) -> Address {
        match self {
            Self::Contract => Who::Contract.address(),
            Self::A => Who::A.address(),
            Self::B => Who::B.address(),
            Self::Zero => Address::ZERO,
            Self::Beneficiary => Who::Beneficiary.address(),
        }
    }
}

/// One EIP-7702 authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Auth {
    pub(crate) authority: Who,
    /// Whether the nonce is the authority's; otherwise one too high.
    pub(crate) nonce_ok: bool,
    /// Whether the chain id is this chain's or zero; otherwise another chain's.
    pub(crate) chain_ok: bool,
    /// Whether the signature recovers; otherwise the authorization is skipped.
    pub(crate) recovers: bool,
    pub(crate) delegate: Delegate,
}

fn auth() -> impl Strategy<Value = Auth> {
    (
        who(),
        prop_oneof![4 => Just(true), 1 => Just(false)],
        prop_oneof![5 => Just(true), 1 => Just(false)],
        prop_oneof![5 => Just(true), 1 => Just(false)],
        prop_oneof![
            3 => Just(Delegate::Contract),
            3 => Just(Delegate::A),
            2 => Just(Delegate::B),
            1 => Just(Delegate::Zero),
            1 => Just(Delegate::Beneficiary),
        ],
    )
        .prop_map(|(authority, nonce_ok, chain_ok, recovers, delegate)| Auth {
            authority,
            nonce_ok,
            chain_ok,
            recovers,
            delegate,
        })
}

/// What a system-address transaction calls on the Oracle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SysCall {
    SetSlot { slot: u8 },
    GetSlot { slot: u8 },
    EmitLog { len: u8 },
    MultiCallEmpty,
}

impl SysCall {
    fn calldata(self) -> Bytes {
        match self {
            Self::SetSlot { slot } => IOracle::setSlotsCall {
                slots: vec![U256::from(slot)],
                values: vec![B256::repeat_byte(0x55)],
            }
            .abi_encode(),
            Self::GetSlot { slot } => IOracle::getSlotCall { slot: U256::from(slot) }.abi_encode(),
            Self::EmitLog { len } => IOracle::emitLogCall {
                topic: B256::repeat_byte(0x66),
                data: vec![0x77; len as usize].into(),
            }
            .abi_encode(),
            Self::MultiCallEmpty => IOracle::multiCallCall { data: vec![] }.abi_encode(),
        }
        .into()
    }
}

fn sys_call() -> impl Strategy<Value = SysCall> {
    prop_oneof![
        3 => (0u8..4).prop_map(|slot| SysCall::SetSlot { slot }),
        2 => (0u8..4).prop_map(|slot| SysCall::GetSlot { slot }),
        2 => (0u8..=64).prop_map(|len| SysCall::EmitLog { len }),
        1 => Just(SysCall::MultiCallEmpty),
    ]
}

/// The `gasLimitOverride` of a keyless deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Override {
    /// Zero: no override.
    None,
    /// The signed gas limit.
    Signed,
    /// Above what the call can forward.
    Large,
    /// Below the signed gas limit.
    Short,
}

/// The shape of a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A call to `to` carrying `data_len` bytes, with an access list of `(account, slots)`.
    Call { to: Who, data_len: u16, access_list: Vec<(Who, u8)> },
    /// A creation transaction whose init code is the case's main program.
    Create,
    /// An EIP-7702 transaction to `to`.
    Eip7702 { to: Who, auths: Vec<Auth> },
    /// A deposit to `to` (or a creation, with `create`), minting `mint` to its caller. A
    /// `system` deposit is one Regolith refuses, which halts as a failed deposit.
    Deposit { to: Who, mint: Value, data_len: u16, system: bool, create: bool },
    /// A legacy call from the system address to the Oracle: promoted to a deposit by the
    /// engine when the registry names its caller. Off the whitelist it is an ordinary
    /// transaction.
    SystemAddress { call: SysCall, nonce_ok: bool, chain_ok: bool, off_whitelist: bool },
    /// A `keylessDeploy` call deploying `init` signed at `signed_nonce`.
    Keyless {
        init: InitCode,
        signed_nonce: u8,
        signed_value: Value,
        gas_override: Override,
        signer_funded: bool,
    },
}

fn access_list() -> impl Strategy<Value = Vec<(Who, u8)>> {
    proptest::collection::vec((who(), 0u8..=3), 0..=2)
}

fn shape(flavor: Flavor) -> impl Strategy<Value = Shape> {
    let init = super::program::program_with(Flavor::Satin, 0, 6);
    let mega_only = match flavor {
        Flavor::Satin => 2,
        Flavor::Neutral => 1,
    };
    let keyless = (
        prop_oneof![
            3 => init.prop_map(|p| InitCode::Runs(Box::new(p))),
            2 => (0u8..=100).prop_map(|len| InitCode::Deploys { len }),
            1 => Just(InitCode::Reverts),
            1 => Just(InitCode::WritesThenDeploys),
        ],
        prop_oneof![5 => Just(0u8), 2 => Just(1), 1 => Just(2)],
        value(),
        prop_oneof![
            3 => Just(Override::None),
            2 => Just(Override::Signed),
            2 => Just(Override::Large),
            1 => Just(Override::Short),
        ],
        prop_oneof![3 => Just(true), 1 => Just(false)],
    )
        .prop_map(|(init, signed_nonce, signed_value, gas_override, signer_funded)| {
            Shape::Keyless { init, signed_nonce, signed_value, gas_override, signer_funded }
        });
    let system_address = (
        sys_call(),
        prop_oneof![5 => Just(true), 1 => Just(false)],
        prop_oneof![5 => Just(true), 1 => Just(false)],
        prop_oneof![5 => Just(false), 1 => Just(true)],
    )
        .prop_map(|(call, nonce_ok, chain_ok, off_whitelist)| Shape::SystemAddress {
            call,
            nonce_ok,
            chain_ok,
            off_whitelist,
        });
    let shapes = prop_oneof![
        10 => (who(), 0u16..=200, access_list())
            .prop_map(|(to, data_len, access_list)| Shape::Call { to, data_len, access_list }),
        3 => Just(Shape::Create),
        3 => (who(), proptest::collection::vec(auth(), 0..=3))
            .prop_map(|(to, auths)| Shape::Eip7702 { to, auths }),
        3 => (who(), value(), 0u16..=100, prop_oneof![7 => Just(false), 1 => Just(true)], prop_oneof![4 => Just(false), 1 => Just(true)])
            .prop_map(|(to, mint, data_len, system, create)| Shape::Deposit { to, mint, data_len, system, create }),
        mega_only => system_address,
        mega_only => keyless,
    ];
    shapes.prop_map(move |shape| match flavor {
        Flavor::Satin => shape,
        Flavor::Neutral => shape.neutralized(),
    })
}

impl Shape {
    /// The shape with what only `MegaETH` has replaced by a plain call.
    fn neutralized(self) -> Self {
        match self {
            Self::SystemAddress { .. } | Self::Keyless { .. } => {
                Self::Call { to: Who::Contract, data_len: 4, access_list: Vec::new() }
            }
            other => other,
        }
    }
}

/// A transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tx {
    pub(crate) shape: Shape,
    pub(crate) value: Value,
    pub(crate) gas: GasTier,
    pub(crate) price: Price,
    /// Whether the nonce is the sender's; otherwise one too high.
    pub(crate) nonce_ok: bool,
}

pub(crate) fn tx(flavor: Flavor) -> impl Strategy<Value = Tx> {
    (shape(flavor), value(), gas_tier(), price(), prop_oneof![8 => Just(true), 1 => Just(false)])
        .prop_map(|(shape, value, gas, price, nonce_ok)| Tx { shape, value, gas, price, nonce_ok })
}

/// A pre-EIP-155 signed creation as Nick's Method makes one: a fixed signature over a
/// transaction nobody holds the key of, so the signer is whatever the contents recover to.
#[derive(Clone, Debug)]
pub(crate) struct Deployment {
    /// The signed transaction, RLP-encoded.
    pub(crate) encoded: Bytes,
    pub(crate) signer: Address,
    pub(crate) gas_limit: u64,
}

impl Deployment {
    pub(crate) fn new(nonce: u64, gas_limit: u64, value: U256, init_code: Bytes) -> Self {
        let tx = TxLegacy {
            nonce,
            gas_price: 100_000_000_000,
            gas_limit,
            to: TxKind::Create,
            value,
            input: init_code,
            chain_id: None,
        };
        let word = U256::from_be_bytes(hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));
        let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
        let mut encoded = Vec::new();
        signed.rlp_encode(&mut encoded);
        let signer = signed.recover_signer().expect("a Nick's-Method signature recovers");
        Self { encoded: encoded.into(), signer, gas_limit }
    }
}

/// The gas limit a keyless deployment is signed with.
const SIGNED_GAS_LIMIT: u64 = 1_000_000;

impl Tx {
    /// The deployment a keyless transaction carries, if it is one.
    pub(crate) fn deployment(&self) -> Option<Deployment> {
        match &self.shape {
            Shape::Keyless { init, signed_nonce, signed_value, .. } => Some(Deployment::new(
                *signed_nonce as u64,
                SIGNED_GAS_LIMIT,
                signed_value.wei(),
                init.assemble().into(),
            )),
            _ => None,
        }
    }

    /// Whether the transaction is a deposit at the envelope level.
    pub(crate) fn is_deposit(&self) -> bool {
        matches!(self.shape, Shape::Deposit { .. })
    }

    /// The sender.
    pub(crate) fn sender(&self) -> Address {
        match self.shape {
            Shape::SystemAddress { .. } => SYSTEM_ADDRESS,
            _ => CALLER,
        }
    }

    /// The transaction as the engine takes it, over `world`, whose main program is `main`.
    pub(crate) fn build(&self, world: &World, main: &[u8]) -> MegaTransaction {
        let sender_nonce = world.nonce_of(self.sender());
        let nonce = if self.nonce_ok { sender_nonce } else { sender_nonce + 1 };
        let base = TxEnv {
            caller: self.sender(),
            gas_limit: self.gas.limit(),
            gas_price: self.price.wei(),
            value: self.value.wei(),
            nonce,
            chain_id: Some(CHAIN_ID),
            ..Default::default()
        };
        let env = match &self.shape {
            Shape::Call { to, data_len, access_list } => {
                let list = AccessList(
                    access_list
                        .iter()
                        .map(|(who, slots)| AccessListItem {
                            address: who.address(),
                            storage_keys: (0..*slots).map(B256::with_last_byte).collect(),
                        })
                        .collect(),
                );
                TxEnv {
                    tx_type: if list.0.is_empty() {
                        TransactionType::Legacy as u8
                    } else {
                        TransactionType::Eip2930 as u8
                    },
                    kind: TxKind::Call(to.address()),
                    data: vec![0xab; *data_len as usize].into(),
                    access_list: list,
                    ..base
                }
            }
            Shape::Create => TxEnv { kind: TxKind::Create, data: main.to_vec().into(), ..base },
            Shape::Eip7702 { to, auths } => TxEnv {
                tx_type: TransactionType::Eip7702 as u8,
                kind: TxKind::Call(to.address()),
                gas_priority_fee: Some(self.price.wei()),
                authorization_list: auths
                    .iter()
                    .map(|auth| {
                        let nonce =
                            world.nonce_of(auth.authority.address()) + u64::from(!auth.nonce_ok);
                        Either::Right(RecoveredAuthorization::new_unchecked(
                            Authorization {
                                chain_id: U256::from(if auth.chain_ok { CHAIN_ID } else { 9 }),
                                address: auth.delegate.address(),
                                nonce,
                            },
                            if auth.recovers {
                                RecoveredAuthority::Valid(auth.authority.address())
                            } else {
                                RecoveredAuthority::Invalid
                            },
                        ))
                    })
                    .collect(),
                ..base
            },
            Shape::Deposit { to, data_len, create, .. } => TxEnv {
                tx_type: op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
                kind: if *create { TxKind::Create } else { TxKind::Call(to.address()) },
                data: if *create {
                    main.to_vec().into()
                } else {
                    vec![0xcd; *data_len as usize].into()
                },
                gas_price: 0,
                ..base
            },
            Shape::SystemAddress { call, nonce_ok, chain_ok, off_whitelist } => {
                let system_nonce = world.nonce_of(SYSTEM_ADDRESS);
                TxEnv {
                    kind: TxKind::Call(if *off_whitelist {
                        Who::Contract.address()
                    } else {
                        ORACLE_CONTRACT_ADDRESS
                    }),
                    data: call.calldata(),
                    nonce: if *nonce_ok { system_nonce } else { system_nonce + 1 },
                    chain_id: if *chain_ok { Some(CHAIN_ID) } else { Some(CHAIN_ID + 1) },
                    ..base
                }
            }
            Shape::Keyless { gas_override, .. } => {
                let deployment = self.deployment().expect("a keyless shape");
                let gas_override = match gas_override {
                    Override::None => U256::ZERO,
                    Override::Signed => U256::from(deployment.gas_limit),
                    Override::Large => U256::from(10_000_000_000u64),
                    Override::Short => U256::from(deployment.gas_limit / 2),
                };
                TxEnv {
                    kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
                    data: IKeylessDeploy::keylessDeployCall {
                        keylessDeploymentTransaction: deployment.encoded,
                        gasLimitOverride: gas_override,
                    }
                    .abi_encode()
                    .into(),
                    ..base
                }
            }
        };
        let mut tx = op_transaction(env);
        if let Shape::Deposit { mint, system, .. } = &self.shape {
            tx.deposit.source_hash = B256::repeat_byte(0x22);
            tx.deposit.mint = Some(mint.wei().saturating_to());
            tx.deposit.is_system_transaction = *system;
        }
        OpTx(tx)
    }
}
