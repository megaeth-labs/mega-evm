//! `MegaEvm` on Satin adds nothing to op-revm yet.
//!
//! The same transaction on the same `CfgEnv`, block and L1 info must produce the same result
//! through `MegaEvm` and through op-revm's `OpEvm`, down to every `ResultGas` field, the logs and
//! the resulting state. Once later changes add `MegaETH` behavior (SALT pricing, history gas,
//! limits), these cases stay the baseline that shows where the two diverge on purpose.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, TxKind, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use op_revm::{L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    context::{
        result::{ExecResultAndState, ExecutionResult},
        BlockEnv, CfgEnv, Context, ContextTr, TxEnv,
    },
    context_interface::cfg::GasParams,
    inspector::NoOpInspector,
    primitives::hardfork::SpecId,
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

/// Runs `tx` on `db` through `MegaEvm`, then through op-revm's `OpEvm` configured with the
/// `CfgEnv` the `MegaEvm` context holds. Returns both outcomes and that `CfgEnv`.
fn run_both(db: MemoryDatabase, tx: TxEnv) -> (Outcome, Outcome, CfgEnv<OpSpecId>) {
    run_both_in(db, tx, block())
}

/// [`run_both`] in `block`.
fn run_both_in(
    db: MemoryDatabase,
    tx: TxEnv,
    block: BlockEnv,
) -> (Outcome, Outcome, CfgEnv<OpSpecId>) {
    let (mut mega, mut op, cfg) = both_evms(db, block);
    let mega_outcome = mega.transact(OpTx(op_transaction(tx.clone()))).unwrap();
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

/// Both engines run on the Satin configuration: Karst on the Osaka gas table, EIP-8037 and
/// EIP-2780 switched on, the 200M execution cap, EIP-7708 and the system-call reservoir margin
/// off.
fn assert_satin_cfg(cfg: &CfgEnv<OpSpecId>) {
    assert_eq!(cfg.spec, OpSpecId::KARST);
    assert_eq!(cfg.gas_params.table(), GasParams::new_spec(SpecId::OSAKA).table());
    assert!(cfg.enable_amsterdam_eip8037);
    assert!(cfg.enable_amsterdam_eip2780);
    assert_eq!(cfg.tx_gas_limit_cap, Some(200_000_000));
    assert!(!cfg.enable_amsterdam_eip7708);
    assert!(!cfg.system_call_state_gas_margin_in_reservoir);
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
    // The Osaka gas table has no state-gas prices, so the new slot draws no state gas even with
    // EIP-8037 on. The Satin gas table prices it; this assertion changes with it.
    assert_eq!(mega.result.gas().state_gas_spent_final(), 0);
    assert_same(&mega, &op);
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
    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value: U256::from(1_000),
        gas_price: 10,
        gas_limit: 1_000_000,
        ..Default::default()
    };
    let (mega, op, _) = run_both_in(db, tx, block);
    assert!(mega.result.is_success());
    assert!(mega.state[&COINBASE].info.balance > U256::ZERO, "the beneficiary was paid");
    assert!(
        mega.state[&CALLER].info.balance > U256::from(10u64.pow(18) - 10 * 1_000_000 - 1_000),
        "the unused gas came back"
    );
    assert_same(&mega, &op);
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
    assert_same(&mega, &op);
}

/// A system call runs exactly as op-revm runs it.
#[test]
fn test_system_call_matches_op_revm() {
    use revm::handler::system_call::SystemCallEvm;
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(3)).stop().build();
    let (mut mega, mut op, _) =
        both_evms(MemoryDatabase::default().account_code(CALLEE, code), block());
    let mega_outcome = mega.system_call_with_caller(CALLER, CALLEE, Default::default()).unwrap();
    let op_outcome = op.system_call_with_caller(CALLER, CALLEE, Default::default()).unwrap();
    assert!(mega_outcome.result.is_success());
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
