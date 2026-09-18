//! `MegaEvm` on Satin adds nothing to op-revm yet.
//!
//! The same transaction on the same `CfgEnv`, block and L1 info must produce the same result
//! through `MegaEvm` and through op-revm's `OpEvm`, down to every `ResultGas` field, the logs and
//! the resulting state. Once later tickets add `MegaETH` behavior (SALT pricing, history gas,
//! limits), these cases stay the baseline that shows where the two diverge on purpose.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use op_revm::{L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    context::{
        result::{ExecResultAndState, ExecutionResult},
        BlockEnv, CfgEnv, Context, ContextTr, TxEnv,
    },
    inspector::NoOpInspector,
    state::EvmState,
    ExecuteEvm, Journal,
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

fn l1_block_info() -> L1BlockInfo {
    L1BlockInfo {
        operator_fee_scalar: Some(U256::ZERO),
        operator_fee_constant: Some(U256::ZERO),
        ..Default::default()
    }
}

fn op_tx(tx: TxEnv) -> OpTransaction<TxEnv> {
    OpTransaction { base: tx, enveloped_tx: Some(Bytes::new()), ..Default::default() }
}

/// Runs `tx` on `db` through `MegaEvm`, then through op-revm's `OpEvm` configured with the
/// `CfgEnv` the `MegaEvm` context holds. Returns both outcomes and that `CfgEnv`.
fn run_both(db: MemoryDatabase, tx: TxEnv) -> (Outcome, Outcome, CfgEnv<OpSpecId>) {
    let ctx = MegaContext::new(db.clone(), MegaSpecId::SATIN)
        .with_block(block())
        .with_chain(l1_block_info());
    let cfg = ctx.cfg().clone();
    let mut mega = MegaEvm::new(ctx);
    let mega_outcome = mega.transact(OpTx(op_tx(tx.clone()))).unwrap();

    let op_ctx = OpContext::new(db, OpSpecId::KARST)
        .with_cfg(cfg.clone())
        .with_block(block())
        .with_chain(l1_block_info());
    let mut op = OpEvm::new(op_ctx, NoOpInspector);
    let op_outcome = op.transact(op_tx(tx)).unwrap();

    (mega_outcome, op_outcome, cfg)
}

/// Asserts the two outcomes agree field by field, then as a whole so a field added later is
/// covered too.
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

/// Both engines run on the Satin configuration: Karst with EIP-8037 and EIP-2780 switched on
/// and the 200M execution cap.
fn assert_satin_cfg(cfg: &CfgEnv<OpSpecId>) {
    assert_eq!(cfg.spec, OpSpecId::KARST);
    assert!(cfg.enable_amsterdam_eip8037);
    assert!(cfg.enable_amsterdam_eip2780);
    assert_eq!(cfg.tx_gas_limit_cap, Some(200_000_000));
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
    assert_eq!(mega.result.gas().reservoir_remaining(), reservoir);
    assert_same(&mega, &op);
}

#[test]
fn test_value_transfer_matches_op_revm() {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)));
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value: U256::from(1_000),
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (mega, op, cfg) = run_both(db, tx);

    assert_satin_cfg(&cfg);
    assert!(mega.result.is_success());
    assert_eq!(mega.state[&CALLEE].info.balance, U256::from(1_000));
    assert_same(&mega, &op);
}

#[test]
fn test_sstore_matches_op_revm() {
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
    assert_same(&mega, &op);
}
