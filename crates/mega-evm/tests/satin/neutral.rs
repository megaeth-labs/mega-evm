//! The neutral configuration the execution-spec gate runs Ethereum's fixtures under.
//!
//! Under it, `MegaEvm` must agree with op-revm's `OpEvm` on the same configuration exactly —
//! gas, refund, logs, output and state — where on Satin's own configuration it spends the
//! history ledger more (`equivalence.rs`). Each case runs on Osaka's configuration and on
//! Amsterdam's, and each is one that pays history on Satin's: a priced transfer that creates its
//! recipient, a fresh slot, a log, a creation and a nested value call, whose two write records
//! Satin charges its caller for.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{
        neutral_cfg, neutral_precompiles, op_transaction, zero_fee_l1_block_info, BytecodeBuilder,
        MemoryDatabase,
    },
    EthSpecId, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use op_revm::{L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    bytecode::opcode::{CALL, GAS, LOG3, POP, PUSH0, PUSH1},
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
const COINBASE: Address = address!("0x00000000000000000000000000000000000c0ffe");

type Outcome = ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>;
type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;

const FORKS: [EthSpecId; 2] = [EthSpecId::OSAKA, EthSpecId::AMSTERDAM];

fn block() -> BlockEnv {
    BlockEnv {
        number: U256::from(1),
        timestamp: U256::from(1_800_000_000u64),
        gas_limit: 100_000_000,
        basefee: 7,
        beneficiary: COINBASE,
        ..Default::default()
    }
}

/// Runs `tx` on `db` through a neutral `MegaEvm` for `fork` and through op-revm's `OpEvm` on the
/// configuration that context holds, and returns both outcomes and the history ledger `MegaEvm`
/// reported.
fn run_both(fork: EthSpecId, db: MemoryDatabase, tx: TxEnv) -> (Outcome, Outcome, u64) {
    let ctx = MegaContext::new(db.clone(), MegaSpecId::SATIN)
        .with_neutral_cfg(neutral_cfg(fork).expect("a neutral fork"))
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());
    let op_ctx = OpContext::new(db, OpSpecId::KARST)
        .with_cfg(ctx.cfg().clone())
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());

    let mut mega = MegaEvm::new(ctx);
    *mega.precompiles_mut() = neutral_precompiles(fork).expect("a neutral fork");
    let outcome = mega.execute_transaction(OpTx(op_transaction(tx.clone()))).unwrap();
    let mega =
        ExecResultAndState::new(outcome.result_and_state.result, outcome.result_and_state.state);

    let op = OpEvm::new(op_ctx, NoOpInspector).transact(op_transaction(tx)).unwrap();
    (mega, op, outcome.gas.history)
}

/// Asserts the two outcomes agree field by field, then as a whole, and that nothing was priced
/// as history.
fn assert_same(fork: EthSpecId, (mega, op, history): (Outcome, Outcome, u64)) {
    assert_eq!(history, 0, "{fork:?}: no history gas");
    assert_eq!(mega.result.gas(), op.result.gas(), "{fork:?}: ResultGas");
    assert_eq!(mega.result.logs(), op.result.logs(), "{fork:?}: logs");
    assert_eq!(mega.result, op.result, "{fork:?}: execution result");
    assert_eq!(mega.state, op.state, "{fork:?}: state");
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)))
}

fn call(data: Bytes, value: U256) -> TxEnv {
    TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        data,
        value,
        gas_price: 10,
        gas_limit: 1_000_000,
        ..Default::default()
    }
}

/// A priced transfer that creates its recipient: the fees go where op-revm sends them, and the
/// new account costs what the fork's schedule says.
#[test]
fn test_neutral_transfer_matches_op_revm() {
    for fork in FORKS {
        let outcome = run_both(fork, funded(), call(Bytes::new(), U256::from(1_000)));
        assert!(outcome.0.result.is_success());
        assert_same(fork, outcome);
    }
}

#[test]
fn test_neutral_sstore_matches_op_revm() {
    let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(42)).stop().build();
    for fork in FORKS {
        let outcome = run_both(
            fork,
            funded().account_code(CALLEE, code.clone()),
            call(Bytes::new(), U256::ZERO),
        );
        assert!(outcome.0.result.is_success());
        assert_same(fork, outcome);
    }
}

#[test]
fn test_neutral_log_matches_op_revm() {
    // LOG3 of the 32-byte word 7 under topics 1, 2 and 3.
    let code = BytecodeBuilder::default()
        .mstore(0, U256::from(7).to_be_bytes::<32>())
        .push_number(3u8)
        .push_number(2u8)
        .push_number(1u8)
        .push_number(32u8)
        .append(PUSH0)
        .append(LOG3)
        .stop()
        .build();
    for fork in FORKS {
        let outcome = run_both(
            fork,
            funded().account_code(CALLEE, code.clone()),
            call(Bytes::new(), U256::ZERO),
        );
        assert_eq!(outcome.0.result.logs().len(), 1);
        assert_same(fork, outcome);
    }
}

#[test]
fn test_neutral_creation_matches_op_revm() {
    let init_code =
        BytecodeBuilder::default().return_with_data([0x60, 0x00, 0x60, 0x00, 0xf3]).build();
    for fork in FORKS {
        let tx = TxEnv {
            kind: TxKind::Create,
            data: init_code.clone(),
            ..call(Bytes::new(), U256::ZERO)
        };
        let outcome = run_both(fork, funded(), tx);
        assert!(outcome.0.result.is_success(), "{:?}", outcome.0.result);
        assert_same(fork, outcome);
    }
}

/// A nested value call: on Satin's configuration its caller pays for the two write records the
/// callee's frame makes, and the callee gets a history allowance. Neither shows here.
#[test]
fn test_neutral_nested_value_call_matches_op_revm() {
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
    for fork in FORKS {
        let db =
            funded().account_balance(CALLEE, U256::from(10_000)).account_code(CALLEE, code.clone());
        let outcome = run_both(fork, db, call(Bytes::new(), U256::ZERO));
        assert!(outcome.0.result.is_success());
        assert_same(fork, outcome);
    }
}
