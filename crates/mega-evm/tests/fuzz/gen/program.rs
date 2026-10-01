//! Random programs, biased towards what the engine meters: storage writes, logs, the call family
//! with value, creations, self-destruction, reads of volatile data, the Oracle's storage, the
//! system contracts and the precompiles.
//!
//! A program is a list of [`Op`]s and an [`End`]. Every op is a self-contained snippet that leaves
//! the stack as it found it, so any list assembles to valid bytecode, and a program shrinks by
//! dropping ops. The assembly is [`BytecodeBuilder`]'s.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        keyless::KEYLESS_DEPLOY_ADDRESS, IMegaAccessControl, IMegaLimitControl, IOracle,
        ISequencerRegistry, ACCESS_CONTROL_ADDRESS, LIMIT_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
    },
    test_utils::BytecodeBuilder,
};
use proptest::prelude::*;
use revm::{bytecode::opcode::*, precompile::u64_to_address};

use super::{value, who, word, Flavor, Value, Who, Word};

/// Which call opcode a call uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    Call,
    CallCode,
    DelegateCall,
    StaticCall,
}

impl Scheme {
    const fn opcode(self) -> u8 {
        match self {
            Self::Call => CALL,
            Self::CallCode => CALLCODE,
            Self::DelegateCall => DELEGATECALL,
            Self::StaticCall => STATICCALL,
        }
    }

    /// Whether the scheme carries a value operand.
    const fn carries_value(self) -> bool {
        matches!(self, Self::Call | Self::CallCode)
    }
}

fn scheme() -> impl Strategy<Value = Scheme> {
    prop_oneof![
        6 => Just(Scheme::Call),
        1 => Just(Scheme::CallCode),
        2 => Just(Scheme::DelegateCall),
        2 => Just(Scheme::StaticCall),
    ]
}

/// Where a call goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// One of the fixed accounts.
    Who(Who),
    /// The running contract itself (`ADDRESS`).
    SelfAddress,
    /// The block beneficiary (`COINBASE`): a volatile read of its account.
    Coinbase,
    /// A system contract, by address; a call runs its bytecode or its interceptor.
    System(SystemContract),
    /// A precompile.
    Precompile(Precompile),
}

impl Target {
    /// The target with what only `MegaETH` has replaced: a system contract becomes `A`.
    const fn neutralized(self) -> Self {
        match self {
            Self::System(_) => Self::Who(Who::A),
            other => other,
        }
    }
}

/// Pushes the target's address.
fn push_target(code: BytecodeBuilder, target: Target) -> BytecodeBuilder {
    match target {
        Target::Who(who) => code.push_address(who.address()),
        Target::SelfAddress => code.append(ADDRESS),
        Target::Coinbase => code.append(COINBASE),
        Target::System(contract) => code.push_address(contract.address()),
        Target::Precompile(precompile) => code.push_address(precompile.address()),
    }
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        8 => who().prop_map(Target::Who),
        1 => Just(Target::SelfAddress),
        2 => Just(Target::Coinbase),
        3 => system_contract().prop_map(Target::System),
        2 => precompile().prop_map(Target::Precompile),
    ]
}

/// One of the system contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemContract {
    Oracle,
    AccessControl,
    LimitControl,
    KeylessDeploy,
    Registry,
}

impl SystemContract {
    pub(crate) const fn address(self) -> Address {
        match self {
            Self::Oracle => ORACLE_CONTRACT_ADDRESS,
            Self::AccessControl => ACCESS_CONTROL_ADDRESS,
            Self::LimitControl => LIMIT_CONTROL_ADDRESS,
            Self::KeylessDeploy => KEYLESS_DEPLOY_ADDRESS,
            Self::Registry => SEQUENCER_REGISTRY_ADDRESS,
        }
    }
}

fn system_contract() -> impl Strategy<Value = SystemContract> {
    prop_oneof![
        Just(SystemContract::Oracle),
        Just(SystemContract::AccessControl),
        Just(SystemContract::LimitControl),
        Just(SystemContract::KeylessDeploy),
        Just(SystemContract::Registry),
    ]
}

/// A method of a system contract, with the calldata that calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemMethod {
    /// `MegaAccessControl.disableVolatileDataAccess()`.
    Disable,
    /// `MegaAccessControl.enableVolatileDataAccess()`.
    Enable,
    /// `MegaAccessControl.isVolatileDataAccessDisabled()`.
    IsDisabled,
    /// `MegaLimitControl.remainingComputeGas()`.
    RemainingCompute,
    /// `Oracle.sendHint(topic, data)` with `len` bytes of data.
    SendHint { len: u8 },
    /// `Oracle.getSlot(slot)`: a volatile read of the Oracle's storage.
    GetSlot { slot: u8 },
    /// `Oracle.setSlot(slot, value)`: refused by the contract unless the caller is the system
    /// address.
    SetSlot { slot: u8 },
    /// `SequencerRegistry.currentSystemAddress()`: bytecode, not intercepted.
    RegistryCurrent,
    /// `KeylessDeploy.keylessDeploy(...)` from a contract, which the dispatch does not take.
    KeylessFromContract,
    /// A selector no contract declares.
    Unknown,
}

impl SystemMethod {
    pub(crate) fn target(self) -> SystemContract {
        match self {
            Self::Disable | Self::Enable | Self::IsDisabled | Self::Unknown => {
                SystemContract::AccessControl
            }
            Self::RemainingCompute => SystemContract::LimitControl,
            Self::SendHint { .. } | Self::GetSlot { .. } | Self::SetSlot { .. } => {
                SystemContract::Oracle
            }
            Self::RegistryCurrent => SystemContract::Registry,
            Self::KeylessFromContract => SystemContract::KeylessDeploy,
        }
    }

    pub(crate) fn calldata(self) -> Vec<u8> {
        match self {
            Self::Disable => IMegaAccessControl::disableVolatileDataAccessCall {}.abi_encode(),
            Self::Enable => IMegaAccessControl::enableVolatileDataAccessCall {}.abi_encode(),
            Self::IsDisabled => {
                IMegaAccessControl::isVolatileDataAccessDisabledCall {}.abi_encode()
            }
            Self::RemainingCompute => IMegaLimitControl::remainingComputeGasCall {}.abi_encode(),
            Self::SendHint { len } => IOracle::sendHintCall {
                topic: alloy_primitives::B256::repeat_byte(0x11),
                data: vec![0x22; len as usize].into(),
            }
            .abi_encode(),
            Self::GetSlot { slot } => IOracle::getSlotCall { slot: U256::from(slot) }.abi_encode(),
            Self::SetSlot { slot } => IOracle::setSlotCall {
                slot: U256::from(slot),
                value: alloy_primitives::B256::repeat_byte(0x33),
            }
            .abi_encode(),
            Self::RegistryCurrent => ISequencerRegistry::currentSystemAddressCall {}.abi_encode(),
            Self::KeylessFromContract => {
                mega_evm::system::keyless::IKeylessDeploy::keylessDeployCall {
                    keylessDeploymentTransaction: Bytes::from_static(&[0xf8, 0x01]),
                    gasLimitOverride: U256::ZERO,
                }
                .abi_encode()
            }
            Self::Unknown => vec![0xde, 0xad, 0xbe, 0xef],
        }
    }
}

fn system_method() -> impl Strategy<Value = SystemMethod> {
    prop_oneof![
        3 => Just(SystemMethod::Disable),
        2 => Just(SystemMethod::Enable),
        1 => Just(SystemMethod::IsDisabled),
        2 => Just(SystemMethod::RemainingCompute),
        2 => (0u8..=64).prop_map(|len| SystemMethod::SendHint { len }),
        3 => (0u8..4).prop_map(|slot| SystemMethod::GetSlot { slot }),
        1 => (0u8..4).prop_map(|slot| SystemMethod::SetSlot { slot }),
        1 => Just(SystemMethod::RegistryCurrent),
        1 => Just(SystemMethod::KeylessFromContract),
        1 => Just(SystemMethod::Unknown),
    ]
}

/// A precompile of the Satin set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Precompile {
    EcRecover,
    Sha256,
    Ripemd160,
    Identity,
    ModExp,
    Bn254Add,
    Bn254Mul,
    Bn254Pairing,
    Blake2f,
    KzgPointEvaluation,
    BlsG1Add,
    BlsG1Msm,
    BlsPairing,
    P256Verify,
}

impl Precompile {
    pub(crate) fn address(self) -> Address {
        u64_to_address(match self {
            Self::EcRecover => 1,
            Self::Sha256 => 2,
            Self::Ripemd160 => 3,
            Self::Identity => 4,
            Self::ModExp => 5,
            Self::Bn254Add => 6,
            Self::Bn254Mul => 7,
            Self::Bn254Pairing => 8,
            Self::Blake2f => 9,
            Self::KzgPointEvaluation => 0x0a,
            Self::BlsG1Add => 0x0b,
            Self::BlsG1Msm => 0x0c,
            Self::BlsPairing => 0x0f,
            Self::P256Verify => 0x100,
        })
    }

    /// An input of the length the precompile accepts, cheap where the precompile has a cheap
    /// valid input and failing its own checks where it has none.
    pub(crate) fn canonical_input(self) -> Vec<u8> {
        match self {
            Self::EcRecover => {
                let mut input = vec![0u8; 128];
                input[63] = 27;
                input
            }
            Self::Sha256 | Self::Ripemd160 | Self::Identity => vec![0x5a; 40],
            Self::ModExp => {
                let mut input = vec![0u8; 99];
                input[31] = 1;
                input[63] = 1;
                input[95] = 1;
                input[96..99].copy_from_slice(&[3, 5, 7]);
                input
            }
            Self::Bn254Add => vec![0u8; 128],
            Self::Bn254Mul => vec![0u8; 96],
            Self::Bn254Pairing => Vec::new(),
            Self::Blake2f => {
                let mut input = vec![0u8; 213];
                input[3] = 1;
                input[212] = 1;
                input
            }
            Self::KzgPointEvaluation => vec![0u8; 192],
            Self::BlsG1Add => vec![0u8; 256],
            Self::BlsG1Msm | Self::P256Verify => vec![0u8; 160],
            Self::BlsPairing => vec![0u8; 384],
        }
    }
}

fn precompile() -> impl Strategy<Value = Precompile> {
    prop_oneof![
        Just(Precompile::EcRecover),
        Just(Precompile::Sha256),
        Just(Precompile::Ripemd160),
        Just(Precompile::Identity),
        Just(Precompile::ModExp),
        Just(Precompile::Bn254Add),
        Just(Precompile::Bn254Mul),
        Just(Precompile::Bn254Pairing),
        Just(Precompile::Blake2f),
        Just(Precompile::KzgPointEvaluation),
        Just(Precompile::BlsG1Add),
        Just(Precompile::BlsG1Msm),
        Just(Precompile::BlsPairing),
        Just(Precompile::P256Verify),
    ]
}

/// What a precompile call carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrecompileInput {
    /// [`Precompile::canonical_input`].
    Canonical,
    /// `len` zero bytes.
    Zeros { len: u16 },
}

fn precompile_input() -> impl Strategy<Value = PrecompileInput> {
    prop_oneof![
        3 => Just(PrecompileInput::Canonical),
        1 => (0u16..=300).prop_map(|len| PrecompileInput::Zeros { len }),
    ]
}

/// How much gas a call forwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Forward {
    /// Everything (`GAS`), which the 63/64 rule caps.
    All,
    /// A fixed amount, which the 63/64 rule may cap lower.
    Gas(u32),
}

fn forward() -> impl Strategy<Value = Forward> {
    prop_oneof![
        4 => Just(Forward::All),
        1 => Just(Forward::Gas(0)),
        1 => Just(Forward::Gas(2_300)),
        2 => Just(Forward::Gas(30_000)),
        2 => Just(Forward::Gas(200_000)),
        1 => Just(Forward::Gas(2_000_000)),
    ]
}

fn push_forward(code: BytecodeBuilder, forward: Forward) -> BytecodeBuilder {
    match forward {
        Forward::All => code.append(GAS),
        Forward::Gas(gas) => code.push_number(gas),
    }
}

/// A read of volatile data, which gas detention caps the transaction on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VolatileRead {
    Timestamp,
    Number,
    Coinbase,
    PrevRandao,
    GasLimit,
    BaseFee,
    BlobBaseFee,
    /// An Amsterdam opcode Satin runs on its Osaka base.
    SlotNum,
    /// `BLOCKHASH` of the previous block.
    BlockHash,
    /// The beneficiary's balance.
    CoinbaseBalance,
    /// The beneficiary's code size.
    CoinbaseCodeSize,
    /// The beneficiary's code hash.
    CoinbaseCodeHash,
}

impl VolatileRead {
    fn assemble(self, code: BytecodeBuilder) -> BytecodeBuilder {
        match self {
            Self::Timestamp => code.append_many([TIMESTAMP, POP]),
            Self::Number => code.append_many([NUMBER, POP]),
            Self::Coinbase => code.append_many([COINBASE, POP]),
            Self::PrevRandao => code.append_many([DIFFICULTY, POP]),
            Self::GasLimit => code.append_many([GASLIMIT, POP]),
            Self::BaseFee => code.append_many([BASEFEE, POP]),
            Self::BlobBaseFee => code.append_many([BLOBBASEFEE, POP]),
            Self::SlotNum => code.append_many([SLOTNUM, POP]),
            Self::BlockHash => code.push_number(1u8).append_many([NUMBER, SUB, BLOCKHASH, POP]),
            Self::CoinbaseBalance => code.append_many([COINBASE, BALANCE, POP]),
            Self::CoinbaseCodeSize => code.append_many([COINBASE, EXTCODESIZE, POP]),
            Self::CoinbaseCodeHash => code.append_many([COINBASE, EXTCODEHASH, POP]),
        }
    }
}

fn volatile_read() -> impl Strategy<Value = VolatileRead> {
    prop_oneof![
        2 => Just(VolatileRead::Timestamp),
        2 => Just(VolatileRead::Number),
        1 => Just(VolatileRead::Coinbase),
        1 => Just(VolatileRead::PrevRandao),
        1 => Just(VolatileRead::GasLimit),
        1 => Just(VolatileRead::BaseFee),
        1 => Just(VolatileRead::BlobBaseFee),
        1 => Just(VolatileRead::SlotNum),
        1 => Just(VolatileRead::BlockHash),
        2 => Just(VolatileRead::CoinbaseBalance),
        1 => Just(VolatileRead::CoinbaseCodeSize),
        1 => Just(VolatileRead::CoinbaseCodeHash),
    ]
}

/// An account read that is not volatile unless its target is the beneficiary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountRead {
    Balance,
    CodeSize,
    CodeHash,
    /// `EXTCODECOPY` of 32 bytes.
    CodeCopy,
}

fn account_read() -> impl Strategy<Value = AccountRead> {
    prop_oneof![
        Just(AccountRead::Balance),
        Just(AccountRead::CodeSize),
        Just(AccountRead::CodeHash),
        Just(AccountRead::CodeCopy),
    ]
}

/// The init code of a creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InitCode {
    /// Returns nothing: an account with no code.
    Empty,
    /// Deploys `len` bytes of `STOP`.
    Deploys { len: u8 },
    /// Deploys code starting with `0xEF`, which the deposit refuses.
    DeploysEf,
    /// Writes a slot and emits a log, then deploys a byte.
    WritesThenDeploys,
    /// Reverts.
    Reverts,
    /// Halts on `INVALID`.
    Invalid,
    /// Destroys the account it is creating, to `to`.
    Selfdestructs { to: Who },
    /// Runs a program of its own; what it returns is deployed.
    Runs(Box<Program>),
}

impl InitCode {
    pub(crate) fn assemble(&self) -> Vec<u8> {
        match self {
            Self::Empty => vec![STOP],
            Self::Deploys { len } => constructor(&[], &vec![STOP; *len as usize]),
            Self::DeploysEf => constructor(&[], &[0xEF, 0x00]),
            Self::WritesThenDeploys => {
                let prefix = BytecodeBuilder::default()
                    .sstore(U256::from(1), U256::from(7))
                    .push_number(32u8)
                    .append_many([PUSH0, LOG0])
                    .build_vec();
                constructor(&prefix, &[STOP])
            }
            Self::Reverts => BytecodeBuilder::default().revert().build_vec(),
            Self::Invalid => vec![INVALID],
            Self::Selfdestructs { to } => {
                BytecodeBuilder::default().selfdestruct(to.address()).build_vec()
            }
            Self::Runs(program) => program.assemble(),
        }
    }

    /// Whether the init code, or a program inside it, destroys an account.
    fn destroys(&self) -> bool {
        match self {
            Self::Selfdestructs { .. } => true,
            Self::Runs(program) => program.destroys(),
            _ => false,
        }
    }

    /// The init code with what only `MegaETH` has replaced: a program inside is neutralized.
    fn neutralized(self) -> Self {
        match self {
            Self::Runs(program) => Self::Runs(Box::new(program.neutralized())),
            other => other,
        }
    }

    /// The init code with no way left to destroy the account it is creating: a self-destruction
    /// becomes an empty deployment, and a program inside is rewritten by
    /// [`Program::as_init_without_destruction`].
    pub(crate) fn without_destruction(self) -> Self {
        match self {
            Self::Selfdestructs { .. } => Self::Empty,
            Self::Runs(program) => Self::Runs(Box::new(program.as_init_without_destruction())),
            other => other,
        }
    }
}

/// Init code that runs `prefix`, then deploys `runtime`, copied from its own tail.
fn constructor(prefix: &[u8], runtime: &[u8]) -> Vec<u8> {
    let len = u8::try_from(runtime.len()).expect("a short runtime");
    let tail = u16::try_from(prefix.len() + 11).expect("a short prefix").to_be_bytes();
    let mut code = prefix.to_vec();
    code.extend_from_slice(&[PUSH1, len, PUSH2, tail[0], tail[1], PUSH0, CODECOPY]);
    code.extend_from_slice(&[PUSH1, len, PUSH0, RETURN]);
    code.extend_from_slice(runtime);
    code
}

fn init_code() -> impl Strategy<Value = InitCode> {
    let leaf = program_with(Flavor::Satin, 0, 6);
    prop_oneof![
        3 => Just(InitCode::Empty),
        3 => (0u8..=200).prop_map(|len| InitCode::Deploys { len }),
        1 => Just(InitCode::DeploysEf),
        2 => Just(InitCode::WritesThenDeploys),
        1 => Just(InitCode::Reverts),
        1 => Just(InitCode::Invalid),
        2 => who().prop_map(|to| InitCode::Selfdestructs { to }),
        3 => leaf.prop_map(|program| InitCode::Runs(Box::new(program))),
    ]
}

/// One self-contained snippet of a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    /// `SSTORE`.
    Sstore { slot: u8, value: Word },
    /// `SLOAD`.
    Sload { slot: u8 },
    /// `TSTORE`.
    Tstore { slot: u8, value: Word },
    /// `LOGn` of `len` bytes of memory.
    Log { topics: u8, len: u16 },
    /// A call.
    Call { scheme: Scheme, target: Target, value: Value, forward: Forward, args_len: u8 },
    /// A call to a system contract's method, by its calldata.
    System { method: SystemMethod, scheme: Scheme, value: Value },
    /// A call to a precompile.
    Precompile { which: Precompile, input: PrecompileInput, forward: Forward },
    /// `CREATE`, or `CREATE2` with a salt.
    Create { value: Value, salt: Option<u8>, init: InitCode },
    /// A read of volatile data.
    Volatile(VolatileRead),
    /// A read of an account.
    Account { target: Target, read: AccountRead },
    /// A loop of `rounds` rounds copying 32 KiB of memory: about 3,100 gas a round, cheap to run.
    Work { rounds: u16 },
    /// A loop of `rounds` counting rounds: 26 gas a round.
    Burn { rounds: u16 },
    /// `KECCAK256` of `len` bytes of memory.
    Keccak { len: u32 },
    /// An `MSTORE` at `offset`: memory expansion, or an out-of-gas for a large one.
    Mstore { offset: u32 },
    /// A copy of return data past its end, which halts the frame.
    ReturnDataCopyPastEnd,
    /// An opcode that reads the frame's environment.
    Env(u8),
}

/// How a program ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    Stop,
    Return {
        len: u16,
    },
    Revert {
        len: u16,
    },
    Invalid,
    Selfdestruct {
        to: Target,
    },
    /// Loops until the frame runs out of gas.
    Spin,
    /// No terminator: the code runs off its end.
    Fallthrough,
}

fn end() -> impl Strategy<Value = End> {
    prop_oneof![
        6 => Just(End::Stop),
        3 => (0u16..=100).prop_map(|len| End::Return { len }),
        3 => (0u16..=100).prop_map(|len| End::Revert { len }),
        1 => Just(End::Invalid),
        2 => target().prop_map(|to| End::Selfdestruct { to }),
        1 => Just(End::Spin),
        1 => Just(End::Fallthrough),
    ]
}

/// A program: its ops, in order, and how it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Program {
    pub(crate) ops: Vec<Op>,
    pub(crate) end: End,
}

impl Program {
    /// The program's bytecode.
    pub(crate) fn assemble(&self) -> Vec<u8> {
        let mut code = BytecodeBuilder::default();
        for op in &self.ops {
            code = op.assemble(code);
        }
        match self.end {
            End::Stop => code.stop(),
            End::Return { len } => code.push_number(len).append_many([PUSH0, RETURN]),
            End::Revert { len } => code.push_number(len).append_many([PUSH0, REVERT]),
            End::Invalid => code.append(INVALID),
            End::Selfdestruct { to } => push_target(code, to).append(SELFDESTRUCT),
            End::Spin => {
                let dest = code.len() as u32;
                code.append(JUMPDEST)
                    .push_number(0x8000_u16)
                    .append_many([PUSH0, PUSH0, MCOPY])
                    .push_number(dest)
                    .append(JUMP)
            }
            End::Fallthrough => code,
        }
        .build_vec()
    }

    /// Whether the program, or any init code it creates with, destroys an account.
    pub(crate) fn destroys(&self) -> bool {
        matches!(self.end, End::Selfdestruct { .. }) ||
            self.ops.iter().any(|op| match op {
                Op::Create { init, .. } => init.destroys(),
                _ => false,
            })
    }

    /// The program with every init code it creates with rewritten so that it cannot destroy the
    /// account it is creating ([`InitCode::without_destruction`]). The program's own ending
    /// stands: run as a contract's code it destroys an account that existed before the
    /// transaction.
    pub(crate) fn without_destruction_in_creations(self) -> Self {
        let ops = self
            .ops
            .into_iter()
            .map(|op| match op {
                Op::Create { value, salt, init } => {
                    Op::Create { value, salt, init: init.without_destruction() }
                }
                other => other,
            })
            .collect();
        Self { ops, end: self.end }
    }

    /// The program as init code that cannot destroy the account it is creating: an ending
    /// `SELFDESTRUCT` becomes a `STOP`, and a `CALLCODE` or `DELEGATECALL`, which would run
    /// another contract's code, and its ending, as the account being created, becomes a `CALL`.
    /// The init codes it creates with are rewritten the same way.
    pub(crate) fn as_init_without_destruction(self) -> Self {
        let end = match self.end {
            End::Selfdestruct { .. } => End::Stop,
            other => other,
        };
        let ops = self
            .ops
            .into_iter()
            .map(|op| match op {
                Op::Call {
                    scheme: Scheme::CallCode | Scheme::DelegateCall,
                    target,
                    value,
                    forward,
                    args_len,
                } => Op::Call { scheme: Scheme::Call, target, value, forward, args_len },
                other => other,
            })
            .collect();
        Self { ops, end }.without_destruction_in_creations()
    }

    /// The program with what only `MegaETH` has replaced, op by op ([`Op::neutralized`]), so that
    /// op-revm and revm's mainnet EVM run the same thing.
    pub(crate) fn neutralized(self) -> Self {
        let end = match self.end {
            End::Selfdestruct { to } => End::Selfdestruct { to: to.neutralized() },
            other => other,
        };
        Self { ops: self.ops.into_iter().map(Op::neutralized).collect(), end }
    }
}

impl Op {
    /// The op with what only `MegaETH` has replaced: a system contract call becomes a load, a
    /// system contract target becomes `A`, `SLOTNUM` becomes `NUMBER`, and init code is
    /// neutralized.
    fn neutralized(self) -> Self {
        match self {
            Self::System { .. } => Self::Sload { slot: 0 },
            Self::Call { scheme, target, value, forward, args_len } => {
                Self::Call { scheme, target: target.neutralized(), value, forward, args_len }
            }
            Self::Account { target, read } => Self::Account { target: target.neutralized(), read },
            Self::Volatile(VolatileRead::SlotNum) => Self::Volatile(VolatileRead::Number),
            Self::Create { value, salt, init } => {
                Self::Create { value, salt, init: init.neutralized() }
            }
            other => other,
        }
    }

    fn assemble(&self, code: BytecodeBuilder) -> BytecodeBuilder {
        match self {
            Self::Sstore { slot, value } => code.sstore(U256::from(*slot), value.u256()),
            Self::Sload { slot } => code.push_number(*slot).append_many([SLOAD, POP]),
            Self::Tstore { slot, value } => {
                code.push_u256(value.u256()).push_number(*slot).append(TSTORE)
            }
            Self::Log { topics, len } => {
                let mut code = code;
                for topic in (1..=*topics).rev() {
                    code = code.push_number(topic);
                }
                code.push_number(*len).append_many([PUSH0, LOG0 + topics])
            }
            Self::Call { scheme, target, value, forward, args_len } => {
                let code = code.append_many([PUSH0, PUSH0]).push_number(*args_len).append(PUSH0);
                let code = if scheme.carries_value() { code.push_u256(value.wei()) } else { code };
                push_forward(push_target(code, *target), *forward)
                    .append_many([scheme.opcode(), POP])
            }
            Self::System { method, scheme, value } => {
                let data = method.calldata();
                let code = code
                    .mstore(0, &data)
                    .append_many([PUSH0, PUSH0])
                    .push_number(data.len() as u64)
                    .append(PUSH0);
                let code = if scheme.carries_value() { code.push_u256(value.wei()) } else { code };
                code.push_address(method.target().address())
                    .append(GAS)
                    .append_many([scheme.opcode(), POP])
            }
            Self::Precompile { which, input, forward } => {
                let data = match input {
                    PrecompileInput::Canonical => which.canonical_input(),
                    PrecompileInput::Zeros { len } => vec![0u8; *len as usize],
                };
                let code = if data.is_empty() { code } else { code.mstore(0, &data) };
                let code = code
                    .append_many([PUSH0, PUSH0])
                    .push_number(data.len() as u64)
                    .append_many([PUSH0, PUSH0])
                    .push_address(which.address());
                push_forward(code, *forward).append_many([CALL, POP])
            }
            Self::Create { value, salt, init } => {
                let init = init.assemble();
                match salt {
                    None => code.create(value.wei(), &init),
                    Some(salt) => code.create2(value.wei(), &init, U256::from(*salt)),
                }
                .append(POP)
            }
            Self::Volatile(read) => read.assemble(code),
            Self::Account { target, read } => {
                let code = push_target(code, *target);
                match read {
                    AccountRead::Balance => code.append_many([BALANCE, POP]),
                    AccountRead::CodeSize => code.append_many([EXTCODESIZE, POP]),
                    AccountRead::CodeHash => code.append_many([EXTCODEHASH, POP]),
                    AccountRead::CodeCopy => {
                        // EXTCODECOPY pops address, destOffset, offset, size.
                        code.append_many([PUSH0, PUSH0])
                            .push_number(32u8)
                            .append_many([SWAP3, EXTCODECOPY])
                    }
                }
            }
            Self::Work { rounds } => {
                let code = code.push_number(*rounds);
                let dest = code.len() as u32;
                code.append(JUMPDEST)
                    .push_number(0x8000_u16)
                    .append_many([PUSH0, PUSH0, MCOPY])
                    .push_number(1u8)
                    .append_many([SWAP1, SUB, DUP1])
                    .push_number(dest)
                    .append_many([JUMPI, POP])
            }
            Self::Burn { rounds } => {
                let code = code.push_number(*rounds);
                let dest = code.len() as u32;
                code.append(JUMPDEST)
                    .push_number(1u8)
                    .append_many([SWAP1, SUB, DUP1])
                    .push_number(dest)
                    .append_many([JUMPI, POP])
            }
            Self::Keccak { len } => code.push_number(*len).append_many([PUSH0, KECCAK256, POP]),
            Self::Mstore { offset } => code.append(PUSH0).push_number(*offset).append(MSTORE),
            Self::ReturnDataCopyPastEnd => {
                code.push_number(1u8).append_many([RETURNDATASIZE, PUSH0, RETURNDATACOPY])
            }
            Self::Env(opcode) => code.append_many([*opcode, POP]),
        }
    }
}

/// Opcodes that push one word about the frame and read nothing volatile.
const ENV_OPCODES: [u8; 12] = [
    ADDRESS,
    ORIGIN,
    CALLER,
    CALLVALUE,
    CALLDATASIZE,
    CODESIZE,
    GASPRICE,
    RETURNDATASIZE,
    CHAINID,
    SELFBALANCE,
    GAS,
    MSIZE,
];

/// One op at `depth`: a leaf (depth 0) creates nothing, so init code nests only so deep.
fn op(depth: u32) -> impl Strategy<Value = Op> {
    // A leaf creates nothing, so init code nests only so deep: the strategy of a creation
    // builds the init code's, which builds a leaf's, and a leaf must not build another.
    let creates: BoxedStrategy<Op> = if depth == 0 {
        Just(Op::Sstore { slot: 3, value: Word::One }).boxed()
    } else {
        (value(), proptest::option::of(0u8..4), init_code())
            .prop_map(|(value, salt, init)| Op::Create { value, salt, init })
            .boxed()
    };
    prop_oneof![
        8 => (0u8..4, word()).prop_map(|(slot, value)| Op::Sstore { slot, value }),
        2 => (0u8..4).prop_map(|slot| Op::Sload { slot }),
        1 => (0u8..4, word()).prop_map(|(slot, value)| Op::Tstore { slot, value }),
        4 => (0u8..=4, 0u16..=200).prop_map(|(topics, len)| Op::Log { topics, len }),
        8 => (scheme(), target(), value(), forward(), 0u8..=64).prop_map(
            |(scheme, target, value, forward, args_len)| Op::Call {
                scheme,
                target,
                value,
                forward,
                args_len,
            }
        ),
        3 => (system_method(), prop_oneof![3 => Just(Scheme::Call), 1 => Just(Scheme::StaticCall)], value())
            .prop_map(|(method, scheme, value)| Op::System { method, scheme, value }),
        2 => (precompile(), precompile_input(), forward())
            .prop_map(|(which, input, forward)| Op::Precompile { which, input, forward }),
        4 => creates,
        4 => volatile_read().prop_map(Op::Volatile),
        2 => (target(), account_read()).prop_map(|(target, read)| Op::Account { target, read }),
        1 => (1u16..=8_000).prop_map(|rounds| Op::Work { rounds }),
        1 => (1u16..=5_000).prop_map(|rounds| Op::Burn { rounds }),
        1 => prop_oneof![Just(0u32), Just(32), Just(1_000), Just(100_000), Just(50_000_000)]
            .prop_map(|len| Op::Keccak { len }),
        1 => prop_oneof![Just(0u32), Just(1_000), Just(100_000), Just(1 << 24), Just(u32::MAX)]
            .prop_map(|offset| Op::Mstore { offset }),
        1 => Just(Op::ReturnDataCopyPastEnd),
        2 => proptest::sample::select(ENV_OPCODES.as_slice()).prop_map(Op::Env),
    ]
}

/// A program of at most `max_ops` ops at `depth`.
pub(crate) fn program_with(
    flavor: Flavor,
    depth: u32,
    max_ops: usize,
) -> impl Strategy<Value = Program> {
    (proptest::collection::vec(op(depth), 0..=max_ops), end()).prop_map(move |(ops, end)| {
        let program = Program { ops, end };
        match flavor {
            Flavor::Satin => program,
            Flavor::Neutral => program.neutralized(),
        }
    })
}

/// A program of the size a case's contracts run: up to twelve ops, whose init codes may run a
/// leaf program of their own.
pub(crate) fn program(flavor: Flavor) -> impl Strategy<Value = Program> {
    program_with(flavor, 1, 12)
}
