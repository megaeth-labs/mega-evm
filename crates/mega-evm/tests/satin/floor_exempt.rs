//! The calldata floor still applies to deposits and system-originated transactions.
//!
//! At the spec's prices their history costs more than the floor's per-byte rate. At lower
//! measurement prices the floor can bind, and is drawn from the whole gas limit.
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

use crate::common::{account_state_gas, body_history, context, history};

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
    /// A deposit a user's L1 transaction made, priced by bucket.
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
    /// plus the calldata — then [`SINK`], and for a creation the created account's state gas.
    /// History is added separately.
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

/// Enough calldata for the floor to bind at low history prices.
const LONG: u64 = 40_000;

/// Deposits and system-originated transactions report the larger of their total spend and
/// the calldata floor. Both ledgers include their history at the active byte price.
#[test]
fn test_the_floor_of_deposits_and_system_transactions() {
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
            for len in [SHORT, LONG] {
                let records = u64::from(matches!(shape, Shape::ValueCall | Shape::Creation));
                let history_gas =
                    body_history(len) + history(records * mega_evm::WRITE_RECORD_SIZE);
                let gas_limit = gas_limit + history_gas;
                let case = format!("{class:?}, {shape:?}, {len} bytes, gas limit {gas_limit}");
                let tx = transaction(class, shape, len, gas_limit);
                let outcome = MegaEvm::new(context(db()))
                    .execute_transaction(tx)
                    .expect("the transaction is valid");
                assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
                let gas = outcome.gas;

                let floor = shape.floor_base() + 64 * len;
                let spend = shape.spend(len) + history_gas;
                assert_eq!(gas.floor, floor, "{case}: the floor");
                assert_eq!(gas.history, history_gas, "{case}: body and start record");
                assert_eq!(
                    gas.history_bytes,
                    mega_evm::TX_BODY_SIZE + len + records * mega_evm::WRITE_RECORD_SIZE,
                    "{case}"
                );
                assert_eq!(
                    gas.regular + gas.state + gas.history,
                    spend,
                    "{case}: what it spent, the intrinsic charge, the code and the state"
                );
                assert_eq!(gas.gas_used, spend.max(floor), "{case}: the gas used");
                assert_eq!(
                    gas.block_execution_gas(),
                    gas.regular.max(floor),
                    "{case}: what the block counts on its execution ledger"
                );
            }
        }
    }
}
