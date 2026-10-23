//! What a block skips, and what it still counts.
//!
//! The block's gas limit is the page's bound: the fixture block's 10 billion, not a 30 million
//! stand-in. Per-transaction limits stay the protocol's. The executor installs them itself.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    system::{MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{MemoryDatabase, SequencerRegistryRoles},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope, ProtocolLimits,
};
use op_alloy_consensus::TxDeposit;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::State,
    Database,
};

use super::{slots_gas, store_n_slots, CALLER, CONTRACT, TX_STATE_GROWTH_LIMIT};

const CHAIN_ID: u64 = 4_326;
const BLOCK_GAS: u64 = 10_000_000_000;

/// A second sender, so the transaction the block should skip is not the one that filled it.
const OTHER: Address = alloy_primitives::address!("0000000000000000000000000000000000ab1008");
/// The deposit's sender.
const DEPOSITOR: Address = alloy_primitives::address!("0000000000000000000000000000000000ab1009");

/// The database a block runs on: Rex6 predeploys, the registry naming the system address, and
/// every sender funded. `CONTRACT` runs `code`.
pub(super) fn database(code: Bytes) -> MemoryDatabase {
    let mut db = MemoryDatabase::default()
        .rex6_predeploys(SequencerRegistryRoles::from(MEGA_SYSTEM_ADDRESS))
        .account_code(CONTRACT, code)
        .account_balance(CALLER, U256::from(10u128.pow(24)))
        .account_balance(OTHER, U256::from(10u128.pow(24)))
        .account_balance(DEPOSITOR, U256::from(10u128.pow(24)))
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(10u128.pow(24)));
    db.set_account_code(DEPOSITOR, Bytes::new());
    db
}

fn env() -> EvmEnv<MegaSpecId> {
    let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg.chain_id = CHAIN_ID;
    let block = BlockEnv { number: U256::from(1), gas_limit: BLOCK_GAS, ..Default::default() };
    EvmEnv::new(cfg, block)
}

/// A legacy call. Gas price 0, chain id [`CHAIN_ID`].
pub(super) fn legacy(
    from: Address,
    nonce: u64,
    to: Address,
    gas_limit: u64,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 0,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO)),
        from,
    )
}

/// A user deposit to `to`.
pub(super) fn deposit(
    from: Address,
    to: Address,
    gas_limit: u64,
    system: bool,
) -> Recovered<MegaTxEnvelope> {
    let deposit = TxDeposit {
        source_hash: B256::repeat_byte(0x11),
        from,
        to: TxKind::Call(to),
        mint: 0,
        value: U256::ZERO,
        gas_limit,
        is_system_transaction: system,
        input: Bytes::new(),
    };
    Recovered::new_unchecked(
        MegaTxEnvelope::Deposit(Sealed::new_unchecked(deposit, B256::repeat_byte(0xde))),
        from,
    )
}

/// Runs `txs` in order on a fresh block over `db`. Each entry is `Ok` when the transaction was
/// included, including one whose receipt failed.
pub(super) fn drive(
    db: MemoryDatabase,
    txs: Vec<Recovered<MegaTxEnvelope>>,
) -> (Vec<Result<(), String>>, State<MemoryDatabase>) {
    drive_with(db, txs, BlockLimits::no_limits(), None)
}

/// [`drive`] under a builder's `policy`, and under `protocol` in place of the spec's limits when
/// it is given.
pub(super) fn drive_with(
    db: MemoryDatabase,
    txs: Vec<Recovered<MegaTxEnvelope>>,
    policy: BlockLimits,
    protocol: Option<ProtocolLimits>,
) -> (Vec<Result<(), String>>, State<MemoryDatabase>) {
    let mut state = State::builder().with_database(db).build();
    let evm = MegaEvmFactory::new().create_evm(&mut state, env());
    let ctx = MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), policy);
    let mut executor = MegaBlockExecutor::new(
        evm,
        ctx,
        MegaHardforkConfig::default().with_all_activated(),
        OpAlloyReceiptBuilder::default(),
    );
    if let Some(protocol) = protocol {
        executor = executor.with_protocol_limits(protocol);
    }
    executor.apply_pre_execution_changes().expect("the block starts");
    let results = txs
        .iter()
        .map(|tx| executor.execute_transaction(tx).map(|_| ()).map_err(|err| err.to_string()))
        .collect();
    drop(executor);
    (results, state)
}

pub(super) fn slot(state: &mut State<MemoryDatabase>, address: Address, index: u64) -> U256 {
    state.storage(address, U256::from(index)).expect("the slot is readable")
}

/// The block's state growth is already 1,000. The next ordinary transaction is skipped before it
/// runs. A deposit is not skipped.
#[test]
fn test_an_ordinary_transaction_is_skipped_at_the_block_state_growth_limit() {
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT) else { return };
    let db = database(store_n_slots(TX_STATE_GROWTH_LIMIT as u16));
    let (results, mut state) = drive(
        db,
        vec![
            legacy(CALLER, 0, CONTRACT, gas),
            deposit(DEPOSITOR, CONTRACT, gas, false),
            legacy(OTHER, 0, CONTRACT, gas),
        ],
    );
    assert!(results[0].is_ok(), "the filling transaction is included: {:?}", results[0]);
    assert_eq!(slot(&mut state, CONTRACT, 1), U256::from(1));
    assert!(results[1].is_ok(), "a deposit is not skipped: {:?}", results[1]);
    assert!(
        results[2].is_err(),
        "the next ordinary transaction is skipped before it runs: {:?}",
        results[2]
    );
}

/// A block KV limit reduced to 10, so the case is reachable before data size. The transaction
/// whose 10 records reach it is included. A deposit after it is not skipped and counts; the next
/// ordinary transaction is skipped before it runs.
#[test]
fn test_a_reduced_block_kv_limit_skips_the_next_ordinary_transaction() {
    let Some(gas) = slots_gas(10) else { return };
    let protocol = ProtocolLimits::for_spec(MegaSpecId::SATIN).with_block_kv_update_limit(10);
    let db = database(store_n_slots(10));
    let (results, mut state) = drive_with(
        db,
        vec![
            legacy(CALLER, 0, CONTRACT, gas),
            deposit(DEPOSITOR, CONTRACT, gas, false),
            legacy(OTHER, 0, CONTRACT, gas),
        ],
        BlockLimits::no_limits(),
        Some(protocol),
    );
    assert!(results[0].is_ok(), "the transaction that reaches the limit is included");
    assert_eq!(slot(&mut state, CONTRACT, 10), U256::from(1));
    assert!(results[1].is_ok(), "a deposit is not skipped: {:?}", results[1]);
    assert!(results[2].is_err(), "the next ordinary transaction is skipped: {:?}", results[2]);
}

/// The block's data size, KV and state growth are all reached by one transaction under reduced
/// block limits. The next ordinary transaction is skipped. A node may name any of the three, so
/// the test asserts the skip and no dimension.
/// Filling only data size would not hold the other two budgets to the rule.
#[test]
fn test_a_skip_with_every_block_dimension_reached_records_no_dimension() {
    let Some(gas) = slots_gas(2) else { return };
    let protocol = ProtocolLimits::for_spec(MegaSpecId::SATIN)
        .with_block_txs_data_limit(1)
        .with_block_kv_update_limit(1)
        .with_block_state_growth_limit(1);
    let db = database(store_n_slots(2));
    let (results, _) = drive_with(
        db,
        vec![legacy(CALLER, 0, CONTRACT, gas), legacy(OTHER, 0, CONTRACT, gas)],
        BlockLimits::no_limits(),
        Some(protocol),
    );
    assert!(results[0].is_ok(), "the transaction that reaches the limits is included");
    assert!(results[1].is_err(), "the next ordinary transaction is skipped: {:?}", results[1]);
}

/// A system-originated transaction creates 1,001 new slots. It is not stopped: the slots are
/// kept. Its usage is counted and moves the block's state-growth counter past 1,000, so the
/// ordinary transaction after it is skipped.
#[test]
fn test_a_system_transaction_of_1001_slots_is_not_stopped() {
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT + 1) else { return };
    let mut db = database(store_n_slots(1));
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, store_n_slots((TX_STATE_GROWTH_LIMIT + 1) as u16));
    let (results, mut state) = drive(
        db,
        vec![
            legacy(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, gas),
            legacy(CALLER, 0, CONTRACT, gas),
        ],
    );
    assert!(results[0].is_ok(), "the system transaction is included: {:?}", results[0]);
    assert_eq!(
        slot(&mut state, ORACLE_CONTRACT_ADDRESS, TX_STATE_GROWTH_LIMIT + 1),
        U256::from(1),
        "1,001 slots are kept"
    );
    assert!(results[1].is_err(), "the system transaction counted: {:?}", results[1]);
}

/// A pre-block call creates 1,001 new slots. It is not stopped, and the block counters do not
/// move: the ordinary transaction after it that fills 1,000 slots is included, and only the one
/// after that is skipped.
///
/// The pre-block calls' gas is fixed at 31,566,720, which at the spec's byte prices cannot pay
/// 1,001 fresh slots, so the test is built with the `satin-price-override` feature and returns
/// early unless the prices in effect fit them. The pre-block call runs no inspector, so the whole
/// reservoir its outermost frame receives is not observed here.
#[cfg(feature = "satin-price-override")]
#[test]
fn test_a_pre_block_call_of_1001_slots_is_not_stopped_and_does_not_move_the_block() {
    use alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;
    use revm::context_interface::cfg::GasId;
    // A fresh slot's regular gas is the store's own, from the schedule, and the loop's opcodes
    // around it, which take less than 100. Its state gas draws the reservoir first and then
    // spills onto the regular budget, so the two together must fit the call's whole gas.
    let params = mega_evm::satin_gas_params();
    let store_regular = params.get(GasId::sstore_static()) +
        params.get(GasId::cold_storage_cost()) +
        params.get(GasId::sstore_set_without_load_cost());
    let slots = TX_STATE_GROWTH_LIMIT + 1;
    let needed = slots * (crate::common::slot_state_gas() + store_regular + 100);
    if needed > mega_evm::constants::PRE_BLOCK_CALL_GAS_LIMIT {
        mega_evm::test_utils::note_price_guard("1,001 slots do not fit a pre-block call");
        return;
    }
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT) else { return };
    let mut db = database(store_n_slots(TX_STATE_GROWTH_LIMIT as u16));
    db.set_account_code(HISTORY_STORAGE_ADDRESS, store_n_slots(slots as u16));
    let (results, mut state) =
        drive(db, vec![legacy(CALLER, 0, CONTRACT, gas), legacy(OTHER, 0, CONTRACT, gas)]);
    assert_eq!(slot(&mut state, HISTORY_STORAGE_ADDRESS, slots), U256::from(1), "not stopped");
    assert!(results[0].is_ok(), "the pre-block call moved no counter: {:?}", results[0]);
    assert!(results[1].is_err(), "the block is now at 1,000: {:?}", results[1]);
}

/// The block is already at 1,000 state growth: the ordinary transaction that comes later is
/// skipped. A Mega System Transaction at that point is not skipped, and not stopped by the
/// per-transaction limit: its 1,001 slots are kept. It counts.
#[test]
fn test_a_system_transaction_after_1000_slots_is_not_skipped() {
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT + 1) else { return };
    let mut db = database(store_n_slots(TX_STATE_GROWTH_LIMIT as u16));
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, store_n_slots((TX_STATE_GROWTH_LIMIT + 1) as u16));
    let (results, mut state) = drive(
        db,
        vec![
            legacy(CALLER, 0, CONTRACT, gas),
            legacy(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, gas),
            legacy(OTHER, 0, CONTRACT, gas),
        ],
    );
    assert!(results[0].is_ok(), "{:?}", results[0]);
    assert!(results[1].is_ok(), "a system transaction is not skipped: {:?}", results[1]);
    assert_eq!(
        slot(&mut state, ORACLE_CONTRACT_ADDRESS, TX_STATE_GROWTH_LIMIT + 1),
        U256::from(1),
        "it is not stopped"
    );
    assert!(results[2].is_err(), "the block is at its limit: {:?}", results[2]);
}

/// A builder's policy holds the block's KV packing budget at 10 and its state-growth packing
/// budget at 2. The ordinary transaction that fills two slots reaches the state-growth budget,
/// so the next ordinary one is not packed; a deposit and a Mega System Transaction are not
/// refused and still count. A validator applies no policy and includes the next one too, and the
/// transactions both packed compute the same. The protocol values stay 500,000 and 1,000.
#[test]
fn test_a_builders_kv_and_state_growth_policy_does_not_bind_a_validator() {
    let protocol = ProtocolLimits::for_spec(MegaSpecId::SATIN);
    assert_eq!(protocol.block_kv_update_limit, super::TX_KV_UPDATE_LIMIT);
    assert_eq!(protocol.block_state_growth_limit, TX_STATE_GROWTH_LIMIT);
    let Some(gas) = slots_gas(4) else { return };
    let txs = || {
        vec![
            legacy(CALLER, 0, CONTRACT, gas),
            deposit(DEPOSITOR, CONTRACT, gas, false),
            legacy(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, gas),
            legacy(OTHER, 0, CONTRACT, gas),
        ]
    };
    let db = || {
        let mut db = database(store_n_slots(2));
        db.set_account_code(ORACLE_CONTRACT_ADDRESS, store_n_slots(2));
        db
    };
    let policy =
        BlockLimits::no_limits().with_block_kv_update_limit(10).with_block_state_growth_limit(2);
    let (built, mut built_state) = drive_with(db(), txs(), policy, None);
    assert!(built[0].is_ok() && built[1].is_ok() && built[2].is_ok(), "{built:?}");
    assert!(built[3].is_err(), "the builder does not pack it: {:?}", built[3]);
    let (validated, mut validated_state) = drive(db(), txs());
    assert!(validated.iter().all(Result::is_ok), "a validator applies no policy: {validated:?}");
    for (address, index) in [(CONTRACT, 2), (ORACLE_CONTRACT_ADDRESS, 2)] {
        assert_eq!(
            slot(&mut built_state, address, index),
            slot(&mut validated_state, address, index)
        );
    }
}
