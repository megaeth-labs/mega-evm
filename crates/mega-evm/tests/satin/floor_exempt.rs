//! The EIP-7623 calldata floor of a transaction exempt from history gas.
//!
//! A transaction that pays history gas pays every byte it carries at 88 gas, above the floor's 64,
//! so the floor never decides what it uses. A transaction exempt from history gas — a deposit, and
//! the protocol's own — pays its calldata only at the intrinsic token rate, four gas a zero byte
//! and sixteen a non-zero one, so past a few bytes the floor is above what it spent, and its gas
//! used is the floor.
//!
//! The floor's base is the transaction's own EIP-2780 base without its calldata, access-list,
//! authorization and initcode costs: 12,000 for a transfer to the sender itself, 15,000 for a call
//! to another account without value, 21,000 with value, 24,000 for a creation. On top of it, every
//! byte of calldata costs 64 gas, zero or not, a creation's initcode included.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::{MEGA_SYSTEM_ADDRESS, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, ORACLE_CONTRACT_ADDRESS},
    test_utils::{op_transaction, MemoryDatabase},
    MegaEvm, MegaTransaction,
};
use revm::context::TxEnv;

use crate::common::{account_state_gas, context};

/// A depositor whose own code is [`SINK`], so that a transfer to itself runs it.
const DEPOSITOR: Address = address!("0000000000000000000000000000000000f10000");
/// A contract whose code is [`SINK`].
const TARGET: Address = address!("0000000000000000000000000000000000f10001");

/// `PUSH2 0x7fe0; MLOAD; POP; STOP`: a load of the word at 32,736, which expands the frame's
/// memory to 1,024 words. Six bytes, five of them non-zero.
const SINK: [u8; 6] = [0x61, 0x7f, 0xe0, 0x51, 0x50, 0x00];

/// What [`SINK`] costs, all of it regular gas: `PUSH2` 3, `MLOAD` 3 and the memory of 1,024 words,
/// `3 × 1,024 + 1,024² / 512`, then `POP` 2; `STOP` costs nothing.
const SINK_GAS: u64 = 3 + 3 + (3 * 1_024 + 1_024 * 1_024 / 512) + 2;

/// The calldata of every case: [`SINK`] padded with zero bytes to `len` bytes. A creation runs it
/// as its initcode, which stops on [`SINK`]'s `STOP` and deploys nothing; a call ignores it.
fn calldata(len: u64) -> Bytes {
    let mut data = SINK.to_vec();
    data.resize(len as usize, 0);
    data.into()
}

/// The intrinsic regular gas of `len` bytes of [`calldata`]: sixteen for each of [`SINK`]'s five
/// non-zero bytes and four for each zero byte.
const fn calldata_gas(len: u64) -> u64 {
    16 * 5 + 4 * (len - 5)
}

/// What a transaction is, as far as the floor and the exemption are concerned.
#[derive(Clone, Copy, Debug)]
enum Class {
    /// A deposit a user's L1 transaction made: history-exempt, priced by bucket.
    UserDeposit,
    /// A Mega System Transaction: a legacy call from the live system address to the Oracle, which
    /// the engine promotes to a deposit.
    MegaSystemTransaction,
    /// The promoted form of the protocol's own transaction: a deposit carrying the promotion's
    /// source hash, which is the protocol's whoever its caller. A Mega System Transaction can only
    /// be a call to the Oracle; this is the shape a self-transfer or a creation of the protocol's
    /// takes.
    SystemDeposit,
}

/// What the transaction does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// A call without value to another account, which runs [`SINK`].
    Call,
    /// The same call carrying one wei.
    ValueCall,
    /// One wei sent to the sender itself, whose code is [`SINK`].
    SelfTransfer,
    /// A creation whose initcode is the calldata.
    Creation,
}

impl Shape {
    /// The floor's base: the shape's EIP-2780 base without its calldata and initcode costs.
    const fn floor_base(self) -> u64 {
        match self {
            Self::SelfTransfer => 12_000,
            Self::Call => 12_000 + 3_000,
            Self::ValueCall => 12_000 + 3_000 + 6_000,
            Self::Creation => 12_000 + 12_000,
        }
    }

    /// What a transaction of this shape carrying `len` bytes spends, by hand: its EIP-2780
    /// intrinsic regular gas — the base, plus EIP-3860's two gas per initcode word for a creation,
    /// plus the calldata — then [`SINK`], and for a creation the created account's state gas. An
    /// exempt transaction pays no history gas, so that is all of it.
    fn spend(self, len: u64) -> u64 {
        let initcode_words = if self == Self::Creation { len.div_ceil(32) } else { 0 };
        let created_account = if self == Self::Creation { account_state_gas() } else { 0 };
        self.floor_base() + 2 * initcode_words + calldata_gas(len) + SINK_GAS + created_account
    }
}

/// The database every case runs on: the depositor and the target run [`SINK`], the Oracle's
/// address holds it too — a Mega System Transaction is told by its own fields and the registry's
/// word, not by the code at the Oracle — and every sender exists and can send one wei.
fn db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_code(DEPOSITOR, Bytes::from_static(&SINK))
        .account_balance(DEPOSITOR, U256::from(1_000))
        .account_code(TARGET, Bytes::from_static(&SINK))
        .account_code(ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&SINK))
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1_000))
        .sequencer_registry(MEGA_SYSTEM_ADDRESS)
}

/// The transaction of `class` and `shape` carrying `len` bytes, with `gas_limit`.
fn transaction(class: Class, shape: Shape, len: u64, gas_limit: u64) -> MegaTransaction {
    let (caller, to) = match class {
        Class::MegaSystemTransaction => (MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS),
        Class::UserDeposit | Class::SystemDeposit => (DEPOSITOR, TARGET),
    };
    let (kind, value) = match shape {
        Shape::Call => (TxKind::Call(to), U256::ZERO),
        Shape::ValueCall => (TxKind::Call(to), U256::from(1)),
        Shape::SelfTransfer => (TxKind::Call(caller), U256::from(1)),
        Shape::Creation => (TxKind::Create, U256::ZERO),
    };
    let mut tx = op_transaction(TxEnv {
        caller,
        kind,
        value,
        data: calldata(len),
        gas_limit,
        ..Default::default()
    });
    match class {
        Class::UserDeposit => tx.deposit.source_hash = B256::repeat_byte(0x11),
        Class::SystemDeposit => tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
        Class::MegaSystemTransaction => {}
    }
    OpTx(tx)
}

/// Calldata short enough that the floor stays below the spend: 32 bytes put 64 × 32 − (16 × 5 +
/// 4 × 27) = 1,860 gas more in the floor than in the intrinsic charge, under what [`SINK`] costs.
const SHORT: u64 = 32;

/// Calldata long enough that the floor binds: 40,000 bytes put 2,560,000 gas in the floor, more
/// than the intrinsic charge, [`SINK`] and a created account at any byte price the grid reaches
/// together.
const LONG: u64 = 40_000;

/// An exempt transaction's gas used is the larger of what it spent and its floor, and both are
/// the spec's figures: the floor is the shape's base — 12,000 for a transfer to the sender itself,
/// 15,000 for a call without value, 21,000 with value, 24,000 for a creation — plus 64 gas a byte
/// of calldata, zero or not, a creation's initcode included; the spend is the intrinsic regular
/// gas, what the transaction ran and the state it added, and no history.
///
/// Each case runs with [`SHORT`] calldata, where the spend is above the floor and is the gas used,
/// and with [`LONG`] calldata, where the floor binds and is the gas used. Each runs below the
/// execution cap and above it: where the floor binds it is drawn from the whole gas limit, the
/// reservoir included, so the gas used is the floor at either gas limit. The figure the block
/// counts towards its execution-gas limit is the regular ledger at least the floor.
///
/// Rules: [S4.19], [S4.22], [S4.23], [S5.5], [S7.34], [S7.36], [S7.37], [S5], [S7]. Independence:
/// independent — the floor's base and rate, the intrinsic costs and the memory the code expands
/// are written out; the created account's state gas is the schedule's entry at the price in
/// effect.
#[test]
fn test_the_floor_of_an_exempt_transaction() {
    use Class::{MegaSystemTransaction, SystemDeposit, UserDeposit};
    use Shape::{Call, Creation, SelfTransfer, ValueCall};

    let cases: [(Class, Shape); 10] = [
        (UserDeposit, Call),
        (UserDeposit, ValueCall),
        (UserDeposit, SelfTransfer),
        (UserDeposit, Creation),
        (MegaSystemTransaction, Call),
        (MegaSystemTransaction, ValueCall),
        (SystemDeposit, Call),
        (SystemDeposit, ValueCall),
        (SystemDeposit, SelfTransfer),
        (SystemDeposit, Creation),
    ];
    for gas_limit in [10_000_000, TX_GAS_LIMIT_CAP + 100_000_000] {
        for (class, shape) in cases {
            for (len, binds) in [(SHORT, false), (LONG, true)] {
                let case = format!("{class:?}, {shape:?}, {len} bytes, gas limit {gas_limit}");
                let tx = transaction(class, shape, len, gas_limit);
                let outcome = MegaEvm::new(context(db()))
                    .execute_transaction(tx)
                    .expect("the transaction is valid");
                assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
                let gas = outcome.gas;

                let floor = shape.floor_base() + 64 * len;
                let spend = shape.spend(len);
                assert_eq!(gas.floor, floor, "{case}: the floor");
                assert_eq!(binds, floor > spend, "{case}: whether the floor binds");
                assert_eq!(gas.history, 0, "{case}: an exempt transaction pays no history");
                assert_eq!(
                    gas.regular + gas.state,
                    spend,
                    "{case}: what it spent, the intrinsic charge, the code and the state"
                );
                assert_eq!(gas.gas_used, spend.max(floor), "{case}: the gas used");
                assert_eq!(
                    gas.block_execution_gas(),
                    gas.regular.max(floor),
                    "{case}: what the block counts towards its execution-gas limit"
                );
            }
        }
    }
}
