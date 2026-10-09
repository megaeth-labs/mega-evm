//! What `MegaEvm` on Satin adds to op-revm, and what it does not.
//!
//! An ordinary transaction on the same `CfgEnv`, block and L1 info must produce the same result
//! through `MegaEvm` and through op-revm's `OpEvm` — the same state, the same logs, the same
//! refund and floor — and the same gas but for the history gas Satin charges, which op-revm has
//! no ledger for. That is the one divergence every transaction has:
//! [`assert_same_but_history`] holds each baseline case to `op-revm's total + the history ledger`
//! and to op-revm's state gas exactly, and
//! [`test_the_history_ledger_is_what_satin_adds_to_every_transaction`] pins what that ledger is
//! made of. One baseline case runs above the execution cap, because a transaction that has an
//! EIP-8037 reservoir pays its history out of it, and a charge that is given back behind the
//! ledger's back shows up nowhere else. The six cases at the end are the ones that diverge for a
//! reason of their own — the
//! system contract interceptors, keyless deployment, the system-address transaction,
//! the account a deposit creates for its sender, a crowded SALT bucket and the history ledger —
//! and they pin both sides, so a divergence is never silently absorbed. Later changes (the
//! limits) add their own.
//!
//! SALT pricing multiplies a state gas charge by the capacity of the bucket it lands in, and
//! op-revm has no SALT to read. Every baseline case runs without a SALT environment, where every
//! bucket is minimal and the multiplier is one, so the baseline holds; the last case crowds one
//! bucket and pins what the difference is and how large.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::{
        COST_PER_HISTORY_BYTE, MAX_CONTRACT_SIZE, MAX_INITCODE_SIZE, SLOT_STATE_GAS,
        TX_GAS_LIMIT_CAP,
    },
    satin_gas_params,
    test_utils::{
        note_price_guard, op_transaction, transfer_log, zero_fee_l1_block_info, BytecodeBuilder,
        MemoryDatabase,
    },
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransactionOutcome, TX_BODY_SIZE,
    WRITE_RECORD_SIZE,
};
use op_revm::{constants::BASE_FEE_RECIPIENT, L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, PUSH0, PUSH1},
    context::{
        result::{ExecResultAndState, ExecutionResult},
        BlockEnv, CfgEnv, Context, ContextTr, TxEnv,
    },
    context_interface::cfg::GasId,
    inspector::NoOpInspector,
    state::EvmState,
    ExecuteEvm, Journal,
};

use crate::common::{
    account_state_gas, body_history, history, runs_at_measurement_prices, slot_state_gas,
};

const CALLER: Address = address!("0x4000000000000000000000000000000000000001");
const CALLEE: Address = address!("0x5000000000000000000000000000000000000001");

type Outcome = ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>;
type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;

fn block() -> BlockEnv {
    BlockEnv {
        number: U256::from(1),
        timestamp: U256::from(1_800_000_000u64),
        gas_limit: 10_000_000_000,
        ..Default::default()
    }
}

/// Runs `tx` on `db` through `MegaEvm`, then through op-revm's `OpEvm` configured with the
/// `CfgEnv` the `MegaEvm` context holds. Returns both outcomes — the `MegaEvm` one with its gas
/// by ledger — and that `CfgEnv`.
fn run_both(db: MemoryDatabase, tx: TxEnv) -> (MegaTransactionOutcome, Outcome, CfgEnv<OpSpecId>) {
    run_both_in(db, tx, block())
}

/// Runs `tx` on `db` through op-revm's `OpEvm` alone, on the `CfgEnv` a `MegaEvm` context holds:
/// for a gas limit that is op-revm's own, which Satin, charging the body's history on top, may
/// refuse at validation.
fn run_op_alone(db: MemoryDatabase, tx: TxEnv) -> Outcome {
    let (_, mut op, _) = both_evms(db, block());
    op.transact(op_transaction(tx)).unwrap()
}

/// [`run_both`] in `block`.
fn run_both_in(
    db: MemoryDatabase,
    tx: TxEnv,
    block: BlockEnv,
) -> (MegaTransactionOutcome, Outcome, CfgEnv<OpSpecId>) {
    let (mut mega, mut op, cfg) = both_evms(db, block);
    let mega_outcome = mega.execute_transaction(OpTx(op_transaction(tx.clone()))).unwrap();
    let op_outcome = op.transact(op_transaction(tx)).unwrap();
    (mega_outcome, op_outcome, cfg)
}

/// A `MegaEvm` and an `OpEvm` over `db` in `block`, on the same `CfgEnv`.
fn both_evms(
    db: MemoryDatabase,
    block: BlockEnv,
) -> (MegaEvm<MemoryDatabase, NoOpInspector>, OpEvm<OpContext, NoOpInspector>, CfgEnv<OpSpecId>) {
    let ctx = MegaContext::new(db.clone(), MegaSpecId::SATIN)
        .with_block(block.clone())
        .with_chain(zero_fee_l1_block_info());
    let cfg = ctx.cfg().clone();
    let op_ctx = OpContext::new(db, OpSpecId::KARST)
        .with_cfg(cfg.clone())
        .with_block(block)
        .with_chain(zero_fee_l1_block_info());
    (MegaEvm::new(ctx), OpEvm::new(op_ctx, NoOpInspector), cfg)
}

/// Asserts the two outcomes agree on everything but the history gas Satin charges: `mega` spends
/// exactly its history ledger more than `op` and nothing else moves. The whole `ResultGas` is
/// compared against op-revm's with the two fields history touches rebuilt, so a field added later
/// is covered too.
///
/// The reservoir pays history first and the regular budget pays what it cannot, which is why the
/// reservoir left over saturates at zero rather than going negative.
///
/// Two derived figures follow the larger total rather than staying put, and neither is a second
/// divergence. The EIP-3529 refund cap is a fraction of what the transaction spent, so a Satin
/// transaction can keep a refund op-revm has to cap — EIP-8037 state gas already raises the cap
/// the same way, which is why the refund is compared as `at least op-revm's` and pinned exactly
/// where it matters ([`test_refund_matches_op_revm`]). The receipt's gas used is the spend
/// against the EIP-7623 floor, so a floor that binds op-revm's receipt need not bind Satin's;
/// the whole `ResultGas` is still compared, with the two fields history moves rebuilt.
///
/// The state is compared as it is, which holds because these transactions carry no gas price: a
/// priced transaction pays the extra gas out of the sender's balance
/// ([`test_fees_match_op_revm`]).
fn assert_same_but_history(mega: &MegaTransactionOutcome, op: &Outcome) {
    let history = mega.gas.history;
    assert!(history >= body_history(0), "every Satin transaction pays for its own body");
    let (m, o) = (mega.result.gas(), op.result.gas());
    assert_eq!(m.total_gas_spent(), o.total_gas_spent() + history, "total gas spent");
    assert_eq!(m.state_gas_spent_final(), o.state_gas_spent_final(), "state gas spent");
    assert_eq!(mega.gas.state, o.state_gas_spent_final(), "the state ledger");
    assert!(m.inner_refunded() >= o.inner_refunded(), "refund");
    assert_eq!(m.floor_gas(), o.floor_gas(), "EIP-7623 floor");
    assert_eq!(
        m.reservoir_remaining(),
        o.reservoir_remaining().saturating_sub(history),
        "reservoir remaining"
    );
    assert_eq!(
        m,
        &(*o)
            .with_total_gas_spent(o.total_gas_spent() + history)
            .with_reservoir_remaining(o.reservoir_remaining().saturating_sub(history))
            .with_refunded(m.inner_refunded()),
        "ResultGas"
    );
    assert_eq!(mega.result.logs(), op.result.logs(), "logs");
    assert_eq!(mega.result.output(), op.result.output(), "output");
    assert_eq!(mega.result.is_success(), op.result.is_success(), "success");
    assert_eq!(mega.state, op.state, "state");
}

/// Asserts the two outcomes agree field by field, then as a whole so a field added later is
/// covered too. For the paths that charge no history: a system call, which the protocol pays
/// nothing for.
fn assert_same(mega: &Outcome, op: &Outcome) {
    let (m, o) = (mega.result.gas(), op.result.gas());
    assert_eq!(m.total_gas_spent(), o.total_gas_spent(), "total gas spent");
    assert_eq!(m.state_gas_spent_final(), o.state_gas_spent_final(), "state gas spent");
    assert_eq!(m.inner_refunded(), o.inner_refunded(), "refund");
    assert_eq!(m.floor_gas(), o.floor_gas(), "EIP-7623 floor");
    assert_eq!(m.reservoir_remaining(), o.reservoir_remaining(), "reservoir remaining");
    assert_eq!(m.tx_gas_used(), o.tx_gas_used(), "gas used");
    assert_eq!(m, o, "ResultGas");
    assert_eq!(mega.result.logs(), op.result.logs(), "logs");
    assert_eq!(mega.result, op.result, "execution result");
    assert_eq!(mega.state, op.state, "state");
}

/// Both engines run on the Satin configuration: Karst on the Satin gas schedule, EIP-8037,
/// EIP-2780 and EIP-7708 switched on, the 200M execution cap, `MegaETH`'s code-size limits, and a
/// system call's gas above 30M in its reservoir. So op-revm journals the same transfer logs, and
/// the logs of the two are compared as they are, and op-revm splits a system call's gas as Satin
/// does.
fn assert_satin_cfg(cfg: &CfgEnv<OpSpecId>) {
    assert_eq!(cfg.spec, OpSpecId::KARST);
    assert_eq!(cfg.gas_params.table(), satin_gas_params().table());
    assert!(cfg.enable_amsterdam_eip8037);
    assert!(cfg.enable_amsterdam_eip2780);
    assert_eq!(cfg.tx_gas_limit_cap, Some(TX_GAS_LIMIT_CAP));
    assert_eq!(cfg.limit_contract_code_size, Some(MAX_CONTRACT_SIZE));
    assert_eq!(cfg.limit_contract_initcode_size, Some(MAX_INITCODE_SIZE));
    assert!(cfg.enable_amsterdam_eip7708);
    assert!(!cfg.amsterdam_eip7708_disabled);
    assert!(cfg.system_call_state_gas_margin_in_reservoir);
}

#[test]
fn test_empty_transaction_matches_op_revm() {
    // The gas limit exceeds the execution cap, so the rest goes to the EIP-8037 reservoir; an
    // empty call draws no state gas, so all of it is left over.
    let reservoir = 1_000_000;
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: TX_GAS_LIMIT_CAP + reservoir,
        ..Default::default()
    };
    let (mega, op, cfg) = run_both(MemoryDatabase::default(), tx);

    assert_satin_cfg(&cfg);
    assert!(mega.result.is_success());
    assert_eq!(
        mega.result.gas().reservoir_remaining(),
        reservoir - mega.gas.history,
        "the reservoir paid for the body",
    );
    assert_eq!(op.result.gas().reservoir_remaining(), reservoir, "op-revm has nothing to pay");
    assert_same_but_history(&mega, &op);
}

#[test]
fn test_value_transfer_matches_op_revm() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value: U256::from(1_000),
        // Room for the account the transfer creates, at the byte prices in effect.
        gas_limit: 1_000_000 + account_state_gas(),
        ..Default::default()
    };
    let (mega, op, cfg) = run_both(db, tx);

    assert_satin_cfg(&cfg);
    assert!(mega.result.is_success());
    assert_eq!(mega.state[&CALLEE].info.balance, U256::from(1_000));
    assert_eq!(mega.result.logs(), [transfer_log(CALLER, CALLEE, U256::from(1_000))]);
    assert_same_but_history(&mega, &op);
    // The transfer log is in the receipt and costs no history: the ledger is the body and the
    // recipient's record, as it is for the same transfer without the log.
    assert_eq!(mega.gas.history, history(TX_BODY_SIZE) + history(WRITE_RECORD_SIZE));
}

/// A nested value call above the execution cap, where the reservoir is what pays the history.
///
/// The frame-start write records are the one history charge made from outside the frame it
/// belongs to, and the frame carries its caller's reservoir, so a baseline without a reservoir
/// cannot see whether the charge survived the frame it started. This one runs the same program
/// above the cap: op-revm charges nothing for the records, and Satin's total must exceed
/// op-revm's by exactly the history ledger here too.
///
/// The reservoir left at the end holds the child to inheriting its caller's: a caller takes back
/// the reservoir its child returns as its own, so a child handed none would hand none back, and
/// the transaction would end with no reservoir rather than what the state and history left. The
/// child here is answered without running and spends nothing, so this does not show that a child
/// can spend the whole reservoir it inherits; a child that does is held in the data-size tests.
///
/// Rule [S4.10]. Expected values: the history ledger is `constants`, the byte table at the cost
/// per history byte; the reservoir is an identity against the run's own state and history
/// ledgers, not a figure of its own; the rest is op-revm's run (`external`).
#[test]
fn test_a_nested_value_call_with_a_reservoir_matches_op_revm() {
    if runs_at_measurement_prices() {
        return;
    }
    let inner = address!("0x5000000000000000000000000000000000000002");
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(1u8)
        .push_address(inner)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, code);
    let reservoir = 100_000_000;
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: TX_GAS_LIMIT_CAP + reservoir,
        ..Default::default()
    };
    let (mega, op, cfg) = run_both(db, tx);

    assert_satin_cfg(&cfg);
    assert!(mega.result.is_success(), "{:?}", mega.result);
    assert_eq!(
        mega.gas.history,
        (TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE) * COST_PER_HISTORY_BYTE,
        "the body, the caller's account and the recipient's",
    );
    assert_eq!(
        mega.gas.reservoir_remaining,
        reservoir - mega.gas.state - mega.gas.history,
        "the reservoir paid the state and the history, and nothing came back",
    );
    assert_same_but_history(&mega, &op);
}

#[test]
fn test_sstore_matches_op_revm() {
    if runs_at_measurement_prices() {
        return;
    }
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(42)).stop().build();
    let db = MemoryDatabase::default().account_code(CALLEE, code);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (mega, op, cfg) = run_both(db, tx);

    assert_satin_cfg(&cfg);
    assert!(mega.result.is_success());
    assert_eq!(mega.state[&CALLEE].storage[&U256::ZERO].present_value, U256::from(42));
    // The new slot draws the Satin schedule's state gas, which op-revm on the same schedule
    // draws too: the schedule is configuration, not engine behavior.
    assert_eq!(mega.result.gas().state_gas_spent_final(), SLOT_STATE_GAS);
    assert_same_but_history(&mega, &op);
}

const COINBASE: Address = address!("0x00000000000000000000000000000000000c0ffe");

/// A priced transaction pays its fee, gets its unused gas back and rewards the beneficiary and
/// the fee vaults exactly as op-revm has it.
#[test]
fn test_fees_match_op_revm() {
    let db =
        MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18))).account_code(
            CALLEE,
            BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
        );
    let block = BlockEnv { basefee: 7, beneficiary: COINBASE, ..block() };
    // Room for the slot the callee fills, and for the body's and the two records' history, at the
    // byte prices in effect.
    let gas_limit = 1_000_000 +
        slot_state_gas() +
        body_history(0) +
        history(WRITE_RECORD_SIZE) +
        history(WRITE_RECORD_SIZE);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value: U256::from(1_000),
        gas_price: 10,
        gas_limit,
        ..Default::default()
    };
    let (mega, op, _) = run_both_in(db, tx, block);
    assert!(mega.result.is_success());
    assert!(mega.state[&COINBASE].info.balance > U256::ZERO, "the beneficiary was paid");
    assert!(
        mega.state[&CALLER].info.balance > U256::from(10u64.pow(18) - 10 * gas_limit - 1_000),
        "the unused gas came back"
    );
    assert_eq!(
        mega.result.gas().total_gas_spent(),
        op.result.gas().total_gas_spent() + mega.gas.history,
        "the history ledger is the whole of the extra spend",
    );
    assert_eq!(mega.result.logs(), op.result.logs(), "logs");

    // The extra gas is paid for at the block's prices: the sender is out all ten wei of it, the
    // base fee's seven go to the fee vault and the priority fee's three to the beneficiary.
    let history = U256::from(mega.gas.history);
    let balance = |state: &EvmState, address: Address| state[&address].info.balance;
    assert_eq!(balance(&op.state, CALLER) - balance(&mega.state, CALLER), history * U256::from(10),);
    assert_eq!(
        balance(&mega.state, BASE_FEE_RECIPIENT) - balance(&op.state, BASE_FEE_RECIPIENT),
        history * U256::from(7),
    );
    assert_eq!(
        balance(&mega.state, COINBASE) - balance(&op.state, COINBASE),
        history * U256::from(3),
    );
}

/// A storage refund is applied exactly as op-revm has it.
#[test]
fn test_refund_matches_op_revm() {
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).stop().build();
    let db = MemoryDatabase::default().account_code(CALLEE, code).account_storage(
        CALLEE,
        U256::ZERO,
        U256::from(5),
    );
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (mega, op, _) = run_both(db, tx);
    assert!(mega.result.is_success());
    assert!(mega.result.gas().inner_refunded() > 0, "the clear is refunded");

    // The EIP-3529 cap is a fifth of what the transaction spent, so the history ledger raises it:
    // op-revm has to cap the clearing refund here and Satin, spending more, keeps all of it once
    // the history it pays lifts the cap past the refund, as it does at the spec's prices. EIP-8037
    // state gas raises the same cap; this is that rule applied to the third ledger.
    let quotient = satin_gas_params().get(GasId::max_refund_quotient());
    let clearing = satin_gas_params().get(GasId::sstore_clearing_slot_refund());
    assert_eq!(
        op.result.gas().inner_refunded(),
        op.result.gas().total_gas_spent() / quotient,
        "op-revm is at the cap",
    );
    assert_eq!(
        mega.result.gas().inner_refunded(),
        clearing.min(mega.result.gas().total_gas_spent() / quotient),
        "Satin keeps the whole clearing refund, where its history lifts the cap past it",
    );
    assert_same_but_history(&mega, &op);
}

/// A system call runs exactly as op-revm runs it, its gas above 30M in the reservoir on both: the
/// fresh slot it writes is paid out of the reservoir, and what is left of it is reported.
#[test]
fn test_system_call_matches_op_revm() {
    use revm::handler::{
        system_call::SystemCallEvm, SYSTEM_CALL_GAS_LIMIT, SYSTEM_CALL_REGULAR_GAS_LIMIT,
    };
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(3)).stop().build();
    let (mut mega, mut op, cfg) =
        both_evms(MemoryDatabase::default().account_code(CALLEE, code), block());
    assert_satin_cfg(&cfg);
    let mega_outcome = mega.system_call_with_caller(CALLER, CALLEE, Default::default()).unwrap();
    let op_outcome = op.system_call_with_caller(CALLER, CALLEE, Default::default()).unwrap();
    assert!(mega_outcome.result.is_success());
    assert_eq!(
        mega_outcome.result.gas().reservoir_remaining(),
        SYSTEM_CALL_GAS_LIMIT -
            SYSTEM_CALL_REGULAR_GAS_LIMIT -
            satin_gas_params().get(GasId::sstore_set_state_gas()),
    );
    assert_same(&mega_outcome, &op_outcome);
}

/// A transaction op-revm rejects, Satin rejects with the same error.
#[test]
fn test_invalid_transaction_matches_op_revm() {
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (mut mega, mut op, _) = both_evms(MemoryDatabase::default(), block());
    let unenveloped = OpTransaction { base: tx, enveloped_tx: None, ..Default::default() };
    let mega_error = mega.transact(OpTx(unenveloped.clone())).unwrap_err();
    let op_error = op.transact(unenveloped).unwrap_err();
    assert_eq!(format!("{mega_error:?}"), format!("{op_error:?}"));
    assert!(format!("{mega_error:?}").contains("MissingEnvelopedTx"), "{mega_error:?}");

    // A gas limit above the block's is rejected before anything is charged.
    let small_block = BlockEnv { gas_limit: 500_000, ..block() };
    let (mut mega, mut op, _) = both_evms(MemoryDatabase::default(), small_block);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let mega_error = mega.transact(OpTx(op_transaction(tx.clone()))).unwrap_err();
    let op_error = op.transact(op_transaction(tx)).unwrap_err();
    assert_eq!(format!("{mega_error:?}"), format!("{op_error:?}"));
    assert!(format!("{mega_error:?}").contains("CallerGasLimitMoreThanBlock"), "{mega_error:?}");
}

/* Where the two diverge on purpose */

/// A call to a system contract is answered by the engine, where op-revm runs the contract's
/// bytecode and gets its `NotIntercepted()` revert.
#[test]
fn test_an_intercepted_call_diverges_from_op_revm() {
    use alloy_sol_types::{SolCall, SolError};
    use mega_evm::system::{IMegaAccessControl, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE};

    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(ACCESS_CONTROL_ADDRESS),
        data: IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR.into(),
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (mega, op, cfg) = run_both(db, tx);

    assert_satin_cfg(&cfg);
    assert_eq!(
        mega.result.output().cloned().unwrap_or_default(),
        IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&false),
        "the interceptor answers",
    );
    assert!(!op.result.is_success(), "op-revm runs the bytecode");
    assert_eq!(
        op.result.output().cloned().unwrap_or_default()[..],
        IMegaAccessControl::NotIntercepted::SELECTOR,
    );
}

/// A legacy transaction from the system address is a fee-free deposit on Satin, where op-revm
/// sees an ordinary transaction and refuses it for want of a balance to pay with. Satin knows
/// the address from the `SequencerRegistry`, which the transaction's validation reads: its state
/// carries the read, which op-revm has no reason to make.
#[test]
fn test_a_system_address_transaction_diverges_from_op_revm() {
    use mega_evm::system::{
        IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS,
    };

    let db = MemoryDatabase::default()
        .account_code(ORACLE_CONTRACT_ADDRESS, mega_evm::system::ORACLE_CONTRACT_CODE)
        .sequencer_registry(MEGA_SYSTEM_ADDRESS);
    let tx = TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        data: alloy_sol_types::SolCall::abi_encode(&IOracle::getSlotCall { slot: U256::ZERO })
            .into(),
        // Room for the account the engine creates for the sender, at the byte prices in effect.
        gas_limit: 1_000_000 + account_state_gas(),
        gas_price: 1_000,
        chain_id: Some(1),
        ..Default::default()
    };
    let (mut mega, mut op, cfg) = both_evms(db, block());
    assert_satin_cfg(&cfg);

    let mega_outcome = mega.transact(OpTx(op_transaction(tx.clone()))).expect("Satin accepts it");
    assert!(mega_outcome.result.is_success(), "{:?}", mega_outcome.result);
    assert!(
        mega_outcome
            .state
            .get(&MEGA_SYSTEM_ADDRESS)
            .is_none_or(|account| account.info.balance.is_zero()),
        "the sender pays no fee and needed no balance",
    );
    assert!(
        mega_outcome.state.get(&SEQUENCER_REGISTRY_ADDRESS).is_some_and(|a| !a.is_touched()),
        "the read of the live system address is a read-only entry of the transaction's state",
    );

    let error = op.transact(op_transaction(tx)).expect_err("op-revm wants a fee");
    assert!(format!("{error:?}").contains("LackOfFundForMaxFee"), "{error:?}");
}

/// A deposit-like transaction whose sender does not exist yet pays the state gas of the account
/// it creates for it; op-revm creates the same account and charges nothing for it.
#[test]
fn test_the_created_deposit_caller_diverges_from_op_revm() {
    use alloy_primitives::B256;

    let sender = address!("0x00000000000000000000000000000000000f0001");
    let tx = TxEnv {
        caller: sender,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000 + account_state_gas(),
        gas_price: 0,
        ..Default::default()
    };
    let deposit = |tx: TxEnv| {
        let mut tx = op_transaction(tx);
        tx.deposit.source_hash = B256::repeat_byte(0x11);
        tx
    };
    let (mut mega, mut op, cfg) = both_evms(MemoryDatabase::default(), block());
    assert_satin_cfg(&cfg);

    let mega_outcome = mega.transact(OpTx(deposit(tx.clone()))).unwrap();
    let op_outcome = op.transact(deposit(tx)).unwrap();

    assert!(mega_outcome.result.is_success() && op_outcome.result.is_success());
    assert_eq!(
        mega_outcome.result.gas().state_gas_spent_final(),
        account_state_gas(),
        "Satin charges the account the deposit creates for its sender",
    );
    assert_eq!(
        op_outcome.result.gas().state_gas_spent_final(),
        0,
        "op-revm charges nothing for it",
    );
}

/// A `keylessDeploy` transaction is a deployment in Satin and a call to the contract's bytecode in
/// op-revm, which has no keyless dispatch: Satin deploys the canonical `CREATE2` factory at its
/// canonical address, while op-revm runs the method body, which reverts with `NotIntercepted()`
/// and deploys nothing.
///
/// What a deployment costs beyond op-revm is pinned charge by charge in the system tests. Here the
/// divergence of a call the rules refuse is pinned exactly: Satin charges the fixed overhead,
/// runs no bytecode and keeps nothing else, so it spends what op-revm spends on the same calldata
/// sent to an account with no code, plus the overhead and the history ledger.
#[test]
fn test_a_keyless_deployment_diverges_from_op_revm() {
    use alloy_sol_types::{SolCall, SolError};
    use mega_evm::system::keyless::{
        tests::{CREATE2_FACTORY_CODE_HASH, CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_TX},
        IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_OVERHEAD_GAS,
    };

    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);
    let keyless_deploy = |tx: &[u8]| -> Bytes {
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: Bytes::copy_from_slice(tx),
            gasLimitOverride: U256::from(1_000_000),
        }
        .abi_encode()
        .into()
    };
    let call = |to: Address, data: Bytes| TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data,
        gas_limit: 10_000_000,
        ..Default::default()
    };

    let (mega, op, cfg) =
        run_both(db.clone(), call(KEYLESS_DEPLOY_ADDRESS, keyless_deploy(CREATE2_FACTORY_TX)));
    assert_satin_cfg(&cfg);
    let output = mega.result.output().cloned().unwrap_or_default();
    let deployed = IKeylessDeploy::keylessDeployCall::abi_decode_returns(&output).unwrap();
    assert_eq!(deployed.deployedAddress, CREATE2_FACTORY_CONTRACT, "Satin deploys");
    assert_eq!(
        mega.state.get(&CREATE2_FACTORY_CONTRACT).map(|account| account.info.code_hash),
        Some(CREATE2_FACTORY_CODE_HASH),
    );
    assert_eq!(
        op.result.output().cloned().unwrap_or_default(),
        Bytes::from_static(&IKeylessDeploy::NotIntercepted::SELECTOR),
        "op-revm runs the method body",
    );
    assert!(
        op.state.get(&CREATE2_FACTORY_CONTRACT).is_none_or(|account| account.info.is_empty()),
        "op-revm deploys nothing",
    );

    // A call the rules refuse: bytes that are not a signed transaction.
    let refused = keyless_deploy(b"a transaction");
    let (mut mega_evm, mut op_evm, _) = both_evms(db, block());
    let mega = mega_evm
        .execute_transaction(OpTx(op_transaction(call(KEYLESS_DEPLOY_ADDRESS, refused.clone()))))
        .unwrap();
    let op = op_evm.transact(op_transaction(call(CALLEE, refused))).unwrap();
    assert_eq!(
        mega.result.output().cloned().unwrap_or_default(),
        Bytes::from_static(&IKeylessDeploy::MalformedEncoding::SELECTOR),
    );
    assert!(op.result.is_success(), "the reference call runs no code");
    // What each transaction spent, not what its receipt reports: the EIP-7623 calldata floor
    // lifts op-revm's receipt above what it spent.
    let charged =
        mega.result.gas().total_gas_spent() - op.result.gas().total_gas_spent() - mega.gas.history;
    assert_eq!(charged, KEYLESS_DEPLOY_OVERHEAD_GAS, "the divergence is the overhead, exactly");
}

/// Where Satin leaves op-revm on every transaction: the history gas of the bytes it appends to
/// the chain.
///
/// op-revm charges nothing for them, so Satin's total is op-revm's plus the history ledger, to
/// the gas — the divergence [`assert_same_but_history`] holds every baseline case to. This pins
/// what the ledger is made of: an empty call carries its body and nothing else, and a byte of
/// calldata adds a byte of history on top of the token rate both engines charge for it.
#[test]
fn test_the_history_ledger_is_what_satin_adds_to_every_transaction() {
    if runs_at_measurement_prices() {
        return;
    }
    let tx = |data: Bytes| TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        data,
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (empty, op_empty, cfg) = run_both(MemoryDatabase::default(), tx(Bytes::new()));
    let (with_data, op_with_data) = {
        let (m, o, _) = run_both(MemoryDatabase::default(), tx(vec![0xab_u8; 100].into()));
        (m, o)
    };

    assert_satin_cfg(&cfg);
    assert_eq!(empty.gas.history, TX_BODY_SIZE * COST_PER_HISTORY_BYTE, "the body alone");
    assert_eq!(
        with_data.gas.history,
        (TX_BODY_SIZE + 100) * COST_PER_HISTORY_BYTE,
        "the body and one history byte per calldata byte",
    );
    assert_eq!(empty.gas.state, 0, "the body is history, not state");
    assert_eq!(empty.result.gas().state_gas_spent_final(), 0);
    assert_eq!(
        with_data.result.gas().total_gas_spent() - op_with_data.result.gas().total_gas_spent(),
        with_data.gas.history,
    );
    assert_eq!(
        empty.result.gas().total_gas_spent() - op_empty.result.gas().total_gas_spent(),
        empty.gas.history,
    );
    assert_same_but_history(&empty, &op_empty);
    assert_same_but_history(&with_data, &op_with_data);
}

/// Where Satin leaves op-revm on purpose: a state gas charge in a crowded SALT bucket.
///
/// The transaction, the configuration, the block and the database are the ones
/// [`test_sstore_matches_op_revm`] uses, and every field of the outcome still agrees — except the
/// state ledger, which Satin multiplies by the bucket's capacity in minimum buckets. op-revm has
/// no SALT to read, so it charges the schedule's entry, which is Satin's own answer at the
/// minimum bucket.
#[test]
fn test_a_crowded_salt_bucket_is_where_satin_leaves_op_revm() {
    use crate::salt::{crowded_slot, minimal_envs, salt_context};

    const MULTIPLIER: u64 = 8;
    if runs_at_measurement_prices() {
        return;
    }
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(42)).stop().build();
    let db = MemoryDatabase::default().account_code(CALLEE, code);
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 1_000_000,
        ..Default::default()
    };

    let envs = crowded_slot(minimal_envs(), CALLEE, U256::ZERO, MULTIPLIER);
    let ctx = salt_context(db.clone(), envs).with_block(block());
    let cfg = ctx.cfg().clone();
    let mega = MegaEvm::new(ctx).execute_transaction(OpTx(op_transaction(tx.clone()))).unwrap();
    let op_ctx = OpContext::new(db, OpSpecId::KARST)
        .with_cfg(cfg.clone())
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());
    let op = OpEvm::new(op_ctx, NoOpInspector).transact(op_transaction(tx)).unwrap();

    assert_satin_cfg(&cfg);
    assert!(mega.result.is_success() && op.result.is_success());

    // The one difference, and its exact size.
    assert_eq!(op.result.gas().state_gas_spent_final(), SLOT_STATE_GAS);
    assert_eq!(mega.result.gas().state_gas_spent_final(), SLOT_STATE_GAS * MULTIPLIER);
    assert_eq!(
        mega.result.gas().total_gas_spent() - op.result.gas().total_gas_spent() - mega.gas.history,
        SLOT_STATE_GAS * (MULTIPLIER - 1),
        "beside the history ledger, the extra state gas is the whole of the extra spend",
    );

    // Everything the transaction did is the same: the fees are zero here, so the multiplier
    // moves the gas ledgers and nothing else.
    assert_eq!(mega.result.logs(), op.result.logs(), "logs");
    assert_eq!(mega.result.output(), op.result.output(), "output");
    assert_eq!(mega.state, op.state, "state");
}

/// A transaction that runs out of gas in the runtime gas phase, before its first frame — here on
/// EIP-2780's charge for the account its value creates — burns its whole gas limit on both
/// engines and keeps nothing but its sender's nonce: no reservoir comes back from a transaction
/// below the execution cap, and Satin's history ledger reads the body, the record its frame
/// would have made given back.
///
/// Each engine runs one gas short of what the same transfer spends on it when it succeeds, so
/// each falls short on the last charge of the phase at any byte price: op-revm has no history to
/// charge and needs less by exactly the body's history and the record's, each a charge priced on
/// its own — nothing where a history byte costs nothing, or so little that both round to nothing.
/// Where a state byte costs nothing there is no such charge on op-revm, and the case returns
/// early.
#[test]
fn test_an_out_of_gas_before_the_first_frame_matches_op_revm() {
    if crate::common::state_is_free() {
        return;
    }
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let transfer = |gas_limit| TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value: U256::from(1_000),
        gas_limit,
        ..Default::default()
    };
    let (mega_success, op_success, _) = run_both(db(), transfer(1_000_000 + account_state_gas()));
    assert!(mega_success.result.is_success() && op_success.result.is_success());
    let mega_limit = mega_success.result.gas().total_gas_spent() - 1;
    let op_limit = op_success.result.gas().total_gas_spent() - 1;
    assert_eq!(
        mega_limit - op_limit,
        body_history(0) + history(WRITE_RECORD_SIZE),
        "Satin charges the body's and the record's history on top",
    );

    let (mega, _, cfg) = run_both(db(), transfer(mega_limit));
    let op = run_op_alone(db(), transfer(op_limit));
    assert_satin_cfg(&cfg);
    for (engine, result, gas_limit) in
        [("Satin", &mega.result, mega_limit), ("op-revm", &op.result, op_limit)]
    {
        assert!(result.is_halt(), "{engine}: {result:?}");
        let gas = result.gas();
        assert_eq!(gas.total_gas_spent(), gas_limit, "{engine}: the whole gas limit burns");
        assert_eq!(gas.reservoir_remaining(), 0, "{engine}: no reservoir comes back");
        assert_eq!(gas.tx_gas_used(), gas_limit, "{engine}: the receipt bills the gas limit");
    }
    assert_eq!(mega.gas.history, body_history(0), "the history ledger reads the body");
    assert_eq!(mega.gas.history_bytes, TX_BODY_SIZE);
    assert_eq!(mega.gas.state, 0, "the account was not created");
    // With no gas price the fee is nothing, so the two states are the same: the sender's nonce
    // moved and nothing else.
    assert_eq!(mega.state, op.state, "state");
    assert_eq!(mega.state[&CALLER].info.nonce, 1);
}

/// A creation transaction that runs out of gas in the runtime gas phase bumps its sender's nonce
/// on both engines, so an included out-of-gas creation cannot be replayed, and burns its whole gas
/// limit. Satin runs out on the history of the record its own frame would make, the one charge of
/// the phase it adds; op-revm on EIP-2780's charge for the created account, its last.
///
/// Where a record's history is nothing — a history byte that costs nothing, or so little that a
/// record rounds to nothing — Satin's phase has nothing of its own left to charge, and the case
/// returns early with a note.
#[test]
fn test_an_out_of_gas_creation_before_the_first_frame_matches_op_revm() {
    if crate::common::state_is_free() {
        return;
    }
    if history(WRITE_RECORD_SIZE) == 0 {
        note_price_guard("a write record's history rounds to nothing at these prices");
        return;
    }
    let db = || MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let init_code = BytecodeBuilder::default().stop().build();
    let creation = |gas_limit| TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        data: init_code.clone(),
        gas_limit,
        ..Default::default()
    };
    // A creation whose init code stops spends its intrinsic gas and nothing else on the regular
    // ledger; the body and the created account's record are its history, the account its state.
    let (success, op_success, _) = run_both(db(), creation(1_000_000 + account_state_gas()));
    assert!(success.result.is_success() && op_success.result.is_success());
    let intrinsic = success.gas.regular;
    let mega_limit =
        intrinsic + body_history(init_code.len() as u64) + history(WRITE_RECORD_SIZE) - 1;
    let op_limit = op_success.result.gas().total_gas_spent() - 1;

    let (mega, _, cfg) = run_both(db(), creation(mega_limit));
    let op = run_op_alone(db(), creation(op_limit));
    assert_satin_cfg(&cfg);
    for (engine, result, gas_limit) in
        [("Satin", &mega.result, mega_limit), ("op-revm", &op.result, op_limit)]
    {
        assert!(result.is_halt(), "{engine}: {result:?}");
        assert_eq!(
            result.gas().total_gas_spent(),
            gas_limit,
            "{engine}: the whole gas limit burns"
        );
        assert_eq!(result.gas().reservoir_remaining(), 0, "{engine}: no reservoir comes back");
    }
    assert_eq!(mega.gas.history, body_history(init_code.len() as u64), "the ledger reads the body");
    // op-revm reaches the charge for the created account and loads its address; Satin falls short
    // before it, so only the sender is compared: its nonce moved and nothing else.
    assert_eq!(mega.state[&CALLER], op.state[&CALLER], "the sender's account");
    assert_eq!(mega.state[&CALLER].info.nonce, 1, "the sender's nonce moves by one");
    assert!(
        mega.state.values().all(|account| !account.is_created()),
        "nothing was created: {:?}",
        mega.state.keys().collect::<Vec<_>>()
    );
}
