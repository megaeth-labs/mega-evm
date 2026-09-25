//! How a system call's gas is settled: at most 30M of it regular gas, the rest its state-gas
//! reservoir.
//!
//! Every case runs the same system call through `MegaEvm` and through op-revm's `OpEvm` on the
//! `CfgEnv` the `MegaEvm` context holds — so on the fork with the same switch on — and compares
//! the two field by field: the gas, the reservoir left, the result and the state. A system call is
//! the protocol's own work: it pays no history gas, prices its state at the minimum SALT bucket and
//! is held to no per-transaction limit, so nothing Satin adds to a transaction separates the two.
//!
//! A transaction is not a system call. The last cases hold a system-address transaction, which is
//! promoted to a deposit, to the deposit op-revm runs for it, below and above the execution cap:
//! its gas is split by the cap, as every transaction's is, and not by the system-call budget.

use std::convert::Infallible;

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::{SLOT_STATE_GAS, TX_GAS_LIMIT_CAP},
    satin_gas_params,
    system::{
        storage_slots::{
            CURRENT_SYSTEM_ADDRESS, PENDING_SYSTEM_ADDRESS, SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        },
        IOracle, ISequencerRegistry, MEGA_SYSTEM_ADDRESS, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, ExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    TestExternalEnvs, MIN_BUCKET_SIZE,
};
use op_revm::{
    handler::OpHandler, transaction::deposit::DEPOSIT_TRANSACTION_TYPE, L1BlockInfo, OpEvm,
    OpSpecId, OpTransaction, OpTransactionError,
};
use revm::{
    bytecode::opcode::{GAS, INVALID, NUMBER, POP, PUSH0, SSTORE, STOP},
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult},
        BlockEnv, CfgEnv, Context, ContextSetters, ContextTr, Transaction, TxEnv,
    },
    context_interface::cfg::GasId,
    handler::{
        EthFrame, ExecuteEvm, Handler, SystemCallTx, SYSTEM_CALL_GAS_LIMIT,
        SYSTEM_CALL_REGULAR_GAS_LIMIT,
    },
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    state::EvmState,
    Journal,
};

use crate::common::runs_at_measurement_prices;

const CALLER: Address = address!("0x4000000000000000000000000000000000000001");
const CONTRACT: Address = address!("0x5000000000000000000000000000000000000001");

/// The caller the protocol's own pre-block calls run as.
const SYSTEM_ADDRESS: Address = alloy_eips::eip4788::SYSTEM_ADDRESS;

/// The reservoir a system call on revm's default gas limit carries: the margin above 30M.
const DEFAULT_RESERVOIR: u64 = SYSTEM_CALL_GAS_LIMIT - SYSTEM_CALL_REGULAR_GAS_LIMIT;

/// The state gas of one fresh slot at the minimum bucket, under the schedule in force: Satin's
/// own, [`SLOT_STATE_GAS`], or the one a measurement build's byte prices built.
fn slot() -> u64 {
    satin_gas_params().get(GasId::sstore_set_state_gas())
}

type Outcome = ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>;
type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;
type Op = OpEvm<OpContext, NoOpInspector>;

fn block() -> BlockEnv {
    BlockEnv {
        number: U256::from(1_000),
        timestamp: U256::from(1_800_000_000u64),
        gas_limit: 10_000_000_000,
        ..Default::default()
    }
}

/// An `OpEvm` over `db` on `cfg`, in [`block`].
fn op_evm(db: MemoryDatabase, cfg: CfgEnv<OpSpecId>) -> Op {
    let ctx = OpContext::new(db, OpSpecId::KARST)
        .with_cfg(cfg)
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());
    OpEvm::new(ctx, NoOpInspector)
}

/// A `MegaEvm` over `db` reading `envs`, in [`block`], and an `OpEvm` over the same `db` on the
/// `CfgEnv` the `MegaEvm` context holds.
fn both_evms_with(
    db: MemoryDatabase,
    envs: TestExternalEnvs,
) -> (MegaEvm<MemoryDatabase, NoOpInspector, TestExternalEnvs>, Op) {
    let ctx = MegaContext::new_with_external_envs(
        db.clone(),
        MegaSpecId::SATIN,
        ExternalEnvs::from(envs),
    )
    .with_block(block())
    .with_chain(zero_fee_l1_block_info());
    let cfg = ctx.cfg().clone();
    assert!(cfg.system_call_state_gas_margin_in_reservoir, "both run with the split");
    assert!(cfg.enable_amsterdam_eip8037, "which needs EIP-8037");
    (MegaEvm::new(ctx), op_evm(db, cfg))
}

/// [`both_evms_with`], without a SALT environment.
fn both_evms(db: MemoryDatabase) -> (MegaEvm<MemoryDatabase, NoOpInspector, TestExternalEnvs>, Op) {
    both_evms_with(db, TestExternalEnvs::new())
}

/// op-revm's own system call from `caller` to `contract` on `gas_limit`: its system-call
/// transaction with the gas limit replaced, run through its handler's system-call path.
fn op_system_call(
    op: &mut Op,
    caller: Address,
    contract: Address,
    data: Bytes,
    gas_limit: u64,
) -> Outcome {
    let mut tx = OpTransaction::<TxEnv>::new_system_tx_with_caller(caller, contract, data);
    tx.base.gas_limit = gas_limit;
    op.0.ctx.set_tx(tx);
    let result: Result<_, EVMError<Infallible, OpTransactionError>> =
        OpHandler::<_, _, EthFrame<EthInterpreter>>::new().run_system_call(op);
    ExecResultAndState::new(result.expect("the system call runs"), op.finalize())
}

/// Runs the system call from `caller` to `contract` on `gas_limit` through both engines.
fn run_both(
    (mut mega, mut op): (MegaEvm<MemoryDatabase, NoOpInspector, TestExternalEnvs>, Op),
    caller: Address,
    contract: Address,
    data: Bytes,
    gas_limit: u64,
) -> (Outcome, Outcome) {
    let mega_outcome = mega
        .transact_system_call_with_gas_limit(caller, contract, data.clone(), gas_limit)
        .expect("the system call runs");
    let op_outcome = op_system_call(&mut op, caller, contract, data, gas_limit);
    (mega_outcome, op_outcome)
}

/// Asserts the two outcomes agree field by field, then as a whole, so a field added later is
/// covered too.
fn assert_same(mega: &Outcome, op: &Outcome) {
    let (m, o) = (mega.result.gas(), op.result.gas());
    assert_eq!(m.total_gas_spent(), o.total_gas_spent(), "total gas spent");
    assert_eq!(m.state_gas_spent_final(), o.state_gas_spent_final(), "state gas spent");
    assert_eq!(m.reservoir_remaining(), o.reservoir_remaining(), "reservoir remaining");
    assert_eq!(m.inner_refunded(), o.inner_refunded(), "refund");
    assert_eq!(m.floor_gas(), o.floor_gas(), "EIP-7623 floor");
    assert_eq!(m.tx_gas_used(), o.tx_gas_used(), "gas used");
    assert_eq!(m, o, "ResultGas");
    assert_eq!(mega.result.logs(), op.result.logs(), "logs");
    assert_eq!(mega.result.output(), op.result.output(), "output");
    assert_eq!(mega.result, op.result, "execution result");
    assert_eq!(mega.state, op.state, "state");
}

/// Code that stores the regular gas left after `GAS` into slot zero, a fresh slot.
fn gas_to_slot() -> Bytes {
    Bytes::from_static(&[GAS, PUSH0, SSTORE, STOP])
}

/// What [`gas_to_slot`] stored.
fn stored_gas(outcome: &Outcome) -> u64 {
    let slot = outcome.state[&CONTRACT].storage[&U256::ZERO].present_value();
    slot.to::<u64>()
}

/// Code that writes `slots` fresh slots, `SSTORE(i, 1)` for each.
fn fresh_writes(slots: u64) -> BytecodeBuilder {
    (0..slots)
        .fold(BytecodeBuilder::default(), |code, slot| code.sstore(U256::from(slot), U256::from(1)))
}

/// A database whose [`CONTRACT`] runs `code`.
fn db_with(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default().account_code(CONTRACT, code)
}

/// A system call whose gas limit is at most 30M is regular gas alone: there is no reservoir, and
/// the fresh slot's state gas is paid out of the regular budget.
#[test]
fn test_a_system_call_within_30m_has_no_reservoir() {
    for gas_limit in [1_000_000, SYSTEM_CALL_REGULAR_GAS_LIMIT] {
        let (mega, op) =
            run_both(both_evms(db_with(gas_to_slot())), CALLER, CONTRACT, Bytes::new(), gas_limit);
        assert!(mega.result.is_success(), "{:?}", mega.result);
        assert_eq!(mega.result.gas().reservoir_remaining(), 0);
        assert_eq!(mega.result.gas().state_gas_spent_final(), slot());
        assert!(stored_gas(&mega) > gas_limit - 10, "all {gas_limit} of it is regular gas");
        assert_same(&mega, &op);
    }
}

/// A system call whose gas limit is above 30M runs on 30M of regular gas, which is what `GAS`
/// reads, and carries the rest as its reservoir: the fresh slot is paid out of it, and what is
/// left is reported. So on revm's default limit and on a far larger one.
#[test]
fn test_a_system_call_above_30m_carries_the_excess_as_reservoir() {
    for gas_limit in [SYSTEM_CALL_GAS_LIMIT, 250_000_000] {
        let (mega, op) =
            run_both(both_evms(db_with(gas_to_slot())), CALLER, CONTRACT, Bytes::new(), gas_limit);
        assert!(mega.result.is_success(), "{:?}", mega.result);
        let gas = stored_gas(&mega);
        assert!(gas < SYSTEM_CALL_REGULAR_GAS_LIMIT, "GAS reads the regular budget: {gas}");
        assert!(gas > SYSTEM_CALL_REGULAR_GAS_LIMIT - 10, "all of it: {gas}");
        assert_eq!(
            mega.result.gas().reservoir_remaining(),
            gas_limit - SYSTEM_CALL_REGULAR_GAS_LIMIT - slot(),
            "the excess, less the one slot it paid for",
        );
        assert_same(&mega, &op);
    }
}

/// The default reservoir holds sixteen fresh slots at Satin's price. That many writes empty it and
/// spend no regular gas on state; the next one spills what the reservoir cannot pay onto the
/// regular budget. `GAS`, read after the writes, shows the spill: the regular gas the next write
/// took is its own regular cost, which the write before it shows, plus the part of its slot the
/// reservoir did not hold.
#[test]
fn test_a_system_call_s_state_draws_the_reservoir_first_then_spills() {
    if !runs_at_measurement_prices() {
        assert_eq!(SLOT_STATE_GAS, slot());
        assert_eq!(DEFAULT_RESERVOIR, 16 * SLOT_STATE_GAS);
    }
    // The reading is stored into a slot that already holds a value, which writes no new state.
    const READING: u64 = 0xff;
    let run = |writes| {
        let code =
            fresh_writes(writes).append(GAS).push_number(READING as u8).append(SSTORE).stop();
        let db =
            db_with(code.build()).account_storage(CONTRACT, U256::from(READING), U256::from(1));
        let (mega, op) =
            run_both(both_evms(db), CALLER, CONTRACT, Bytes::new(), SYSTEM_CALL_GAS_LIMIT);
        assert!(mega.result.is_success(), "{:?}", mega.result);
        assert_eq!(mega.result.gas().state_gas_spent_final(), writes * slot());
        assert_same(&mega, &op);
        let reading = mega.state[&CONTRACT].storage[&U256::from(READING)].present_value();
        (reading.to::<u64>(), mega.result.gas().reservoir_remaining())
    };

    // The most slots the reservoir holds, and what the one after them spills.
    let held = DEFAULT_RESERVOIR / slot();
    let spill = (held + 1) * slot() - DEFAULT_RESERVOIR;
    let (before, reservoir_before) = run(held - 1);
    let (at, reservoir_at) = run(held);
    let (after, reservoir_after) = run(held + 1);
    assert_eq!(reservoir_before, DEFAULT_RESERVOIR - (held - 1) * slot());
    assert_eq!(reservoir_at, DEFAULT_RESERVOIR - held * slot(), "empty at Satin's price");
    assert_eq!(reservoir_after, 0);
    let one_write = before - at;
    assert!(
        one_write < slot(),
        "the last write the reservoir held took regular gas for itself alone"
    );
    assert_eq!(at - after, one_write + spill, "the next one spilled");
}

/// What is left of the reservoir is reported, slot by slot, and a call that writes nothing
/// leaves all of it.
#[test]
fn test_reservoir_remaining_is_what_the_writes_left() {
    for writes in [0, 1, 5] {
        let code = fresh_writes(writes).stop().build();
        let (mega, op) = run_both(
            both_evms(db_with(code)),
            CALLER,
            CONTRACT,
            Bytes::new(),
            SYSTEM_CALL_GAS_LIMIT,
        );
        assert_eq!(
            mega.result.gas().reservoir_remaining(),
            DEFAULT_RESERVOIR - writes * slot(),
            "{writes} writes",
        );
        assert_same(&mega, &op);
    }
}

/// A system call that fails settles as op-revm settles it: a revert gives the state gas of what
/// it wrote back to the reservoir and keeps its regular gas; a halt spends the regular budget and
/// still gives the reservoir back.
#[test]
fn test_a_failing_system_call_settles_like_op_revm() {
    let reverting = fresh_writes(3).revert().build();
    let (mega, op) = run_both(
        both_evms(db_with(reverting)),
        CALLER,
        CONTRACT,
        Bytes::new(),
        SYSTEM_CALL_GAS_LIMIT,
    );
    assert!(matches!(mega.result, ExecutionResult::Revert { .. }), "{:?}", mega.result);
    assert_eq!(mega.result.gas().reservoir_remaining(), DEFAULT_RESERVOIR, "nothing kept");
    assert_eq!(mega.result.gas().state_gas_spent_final(), 0);
    assert_same(&mega, &op);

    let halting = fresh_writes(3).append(INVALID).build();
    let (mega, op) = run_both(
        both_evms(db_with(halting)),
        CALLER,
        CONTRACT,
        Bytes::new(),
        SYSTEM_CALL_GAS_LIMIT,
    );
    assert!(mega.result.is_halt(), "{:?}", mega.result);
    assert_eq!(mega.result.gas().reservoir_remaining(), DEFAULT_RESERVOIR);
    assert_eq!(
        mega.result.gas().total_gas_spent(),
        SYSTEM_CALL_REGULAR_GAS_LIMIT,
        "a halt burns the regular budget, not the reservoir",
    );
    assert_same(&mega, &op);
}

/// A system call with a reservoir is still the protocol's own work. In a crowded SALT region,
/// under limits every one of which it would cross, and after reading the block number, it prices
/// its state at the minimum bucket — reservoir draws and spills alike — pays no history and is
/// stopped and detained by nothing: it is op-revm's system call, field for field.
#[test]
fn test_a_system_call_with_a_reservoir_stays_exempt() {
    let code = BytecodeBuilder::default().append(NUMBER).append(POP);
    let code = (0..20_u64).fold(code, |code, slot| code.sstore(U256::from(slot), U256::from(1)));
    let crowded =
        TestExternalEnvs::new().with_default_bucket_capacity(MIN_BUCKET_SIZE as u64 * 1_000);
    let (mut mega, op) = both_evms_with(db_with(code.stop().build()), crowded.clone());
    mega.set_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(0)
            .with_frame_data_size_limit(0)
            .with_tx_kv_update_limit(0)
            .with_frame_kv_update_limit(0)
            .with_tx_state_gas_limit(0)
            .with_block_env_access_compute_gas_limit(0),
    );

    let (mega_outcome, op_outcome) =
        run_both((mega, op), SYSTEM_ADDRESS, CONTRACT, Bytes::new(), 250_000_000);
    assert!(mega_outcome.result.is_success(), "{:?}", mega_outcome.result);
    assert_eq!(mega_outcome.result.gas().state_gas_spent_final(), 20 * slot(), "m = 1");
    assert_eq!(crowded.total_bucket_queries(), 0, "no capacity was read");
    assert_same(&mega_outcome, &op_outcome);
}

/// The registry, holding a system-address change due at the block, for the `applyPendingChanges()`
/// call to apply.
fn registry_with_a_due_change() -> MemoryDatabase {
    let word = |address: Address| U256::from_be_bytes(address.into_word().0);
    MemoryDatabase::default()
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
        .account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            word(MEGA_SYSTEM_ADDRESS),
        )
        .account_storage(SEQUENCER_REGISTRY_ADDRESS, PENDING_SYSTEM_ADDRESS, word(CALLER))
        .account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1_000),
        )
}

/// `applyPendingChanges()` on 30M in a crowded SALT region applies the change at the minimum
/// bucket's price: with no reservoir its state gas is paid out of the 30M, which holds it many
/// times over, and it is what op-revm charges, which has no SALT.
#[test]
fn test_apply_pending_changes_at_30m_is_priced_at_the_minimum_bucket() {
    let crowded =
        TestExternalEnvs::new().with_default_bucket_capacity(MIN_BUCKET_SIZE as u64 * 2_000);
    let data = Bytes::from(ISequencerRegistry::applyPendingChangesCall {}.abi_encode());
    let (mega, op) = run_both(
        both_evms_with(registry_with_a_due_change(), crowded.clone()),
        SYSTEM_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
        data,
        SYSTEM_CALL_REGULAR_GAS_LIMIT,
    );
    assert!(mega.result.is_success(), "{:?}", mega.result);
    assert_eq!(mega.result.gas().reservoir_remaining(), 0);
    assert!(mega.result.gas().state_gas_spent_final() > 0, "the change wrote fresh slots");
    assert_eq!(mega.result.gas().state_gas_spent_final() % slot(), 0, "at m = 1");
    assert_eq!(crowded.total_bucket_queries(), 0);
    assert_same(&mega, &op);
}

/// The default system-call entry point runs on revm's default gas limit however large the block,
/// in a crowded region too: 30M of regular gas and the margin as reservoir, the EIP-2935 and
/// EIP-4788 contracts' writes paid out of it at the minimum bucket. The block's own pre-block
/// calls widen the reservoir with the block (see the block tests); this entry point does not.
#[test]
fn test_the_default_pre_block_calls_keep_the_30m_regular_budget_in_a_crowded_region() {
    use alloy_eips::{
        eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
        eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    };
    for (contract, code, slots) in [
        (HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone(), 1),
        (BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone(), 2),
    ] {
        let db = MemoryDatabase::default().account_code(contract, code);
        let crowded =
            TestExternalEnvs::new().with_default_bucket_capacity(MIN_BUCKET_SIZE as u64 * 2_000);
        let (mut mega, mut op) = both_evms_with(db, crowded.clone());
        let data = Bytes::from(B256::repeat_byte(0x29).0);
        let mega_outcome =
            Evm::transact_system_call(&mut mega, SYSTEM_ADDRESS, contract, data.clone())
                .expect("the system call runs");
        let op_outcome = revm::handler::SystemCallEvm::system_call_with_caller(
            &mut op,
            SYSTEM_ADDRESS,
            contract,
            data,
        )
        .expect("the system call runs");

        assert!(mega_outcome.result.is_success(), "{:?}", mega_outcome.result);
        assert_eq!(mega.ctx().tx().gas_limit(), SYSTEM_CALL_GAS_LIMIT, "not the block's 10B");
        assert_eq!(
            mega_outcome.result.gas().reservoir_remaining(),
            DEFAULT_RESERVOIR - slots * slot(),
        );
        assert_eq!(crowded.total_bucket_queries(), 0);
        assert_same(&mega_outcome, &op_outcome);
    }
}

/// A system-address transaction from [`MEGA_SYSTEM_ADDRESS`] to the Oracle on `gas_limit`, as the
/// sequencer sends it, and the deposit op-revm runs for it: the same fields, with the source hash
/// the promotion stamps and no gas price.
fn system_address_transaction(gas_limit: u64) -> (TxEnv, OpTransaction<TxEnv>) {
    let tx = TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        data: IOracle::getSlotCall { slot: U256::ZERO }.abi_encode().into(),
        gas_limit,
        chain_id: Some(1),
        ..Default::default()
    };
    let mut deposit = OpTransaction::new(TxEnv { tx_type: DEPOSIT_TRANSACTION_TYPE, ..tx.clone() });
    deposit.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    (tx, deposit)
}

/// A system-address transaction is a transaction, not a system call: below the execution cap its
/// whole gas limit is regular gas, and above it the reservoir is what exceeds the cap, as for the
/// deposit op-revm runs for it — never the system call's 30M split.
#[test]
fn test_a_system_address_transaction_is_split_by_the_execution_cap() {
    for (gas_limit, reservoir) in [(1_000_000, 0), (TX_GAS_LIMIT_CAP + 5_000_000, 5_000_000)] {
        let db = MemoryDatabase::default()
            .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1))
            .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE);
        let (mut mega, mut op) = both_evms(db);
        let (tx, deposit) = system_address_transaction(gas_limit);

        let mega_outcome = mega.execute_transaction(OpTx(op_transaction(tx))).expect("accepted");
        assert!(mega.ctx().is_system_originated(), "it is the protocol's own transaction");
        let op_outcome = op.transact(deposit).expect("accepted");

        assert!(mega_outcome.result.is_success(), "{:?}", mega_outcome.result);
        assert_eq!(mega_outcome.gas.history, 0);
        assert_eq!(mega_outcome.result.gas().reservoir_remaining(), reservoir, "{gas_limit}");
        assert_same(&mega_outcome.result_and_state, &op_outcome);
    }
}
