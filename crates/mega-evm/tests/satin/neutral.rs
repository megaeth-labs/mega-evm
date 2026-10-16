//! The neutral configuration the execution-spec gate runs Ethereum's fixtures under.
//!
//! Under it, `MegaEvm` must agree exactly — gas, refund, logs, output and state — with two
//! references, where on Satin's own configuration it spends the history ledger more
//! (`equivalence.rs`):
//!
//! - op-revm's `OpEvm` on the same configuration, which routes fees the way Satin does, for priced
//!   transactions: a transfer that creates its recipient, a fresh slot, a log, a creation and a
//!   nested value call, whose two write records Satin charges its caller for;
//! - revm's own mainnet EVM on the fork's spec, which is Ethereum, for unpriced ones: the same
//!   programs, plus the two parts of a fork's pricing the EVM rather than the context carries — an
//!   `EXTCODESIZE`, whose static price Amsterdam raises, and a KZG point evaluation, which
//!   `MegaETH` prices at twice upstream's.
//!
//! Each case runs on Osaka's configuration and on Amsterdam's.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, hex, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{
        neutral_cfg, neutralize_evm, op_transaction, zero_fee_l1_block_info, BytecodeBuilder,
        MemoryDatabase,
    },
    EthSpecId, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    VolatileDataAccess,
};
use op_revm::{
    constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    L1BlockInfo, OpEvm, OpHaltReason, OpSpecId, OpTransaction,
};
use revm::{
    bytecode::opcode::{
        ADDRESS, BALANCE, CALL, EXTCODESIZE, GAS, LOG1, LOG3, NUMBER, POP, PUSH0, PUSH1, SSTORE,
        TIMESTAMP,
    },
    context::{
        result::{ExecResultAndState, ExecutionResult},
        BlockEnv, CfgEnv, Context, ContextTr, TxEnv,
    },
    inspector::NoOpInspector,
    state::EvmState,
    ExecuteEvm, Journal, MainBuilder, MainContext,
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
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits())
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());
    let op_ctx = OpContext::new(db, OpSpecId::KARST)
        .with_cfg(ctx.cfg().clone())
        .with_block(block())
        .with_chain(zero_fee_l1_block_info());

    let mut mega = MegaEvm::new(ctx);
    neutralize_evm(&mut mega, fork).expect("a neutral fork");
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

/// Runs `tx` on `db`, unpriced, through a neutral `MegaEvm` for `fork` and through revm's mainnet
/// EVM on `fork`'s spec, and returns both outcomes and the history ledger `MegaEvm` reported.
///
/// op-revm credits the three fee vaults with nothing here and so touches them; the touched, empty
/// accounts it leaves are not Ethereum's, and are taken out of `MegaEvm`'s state before the two
/// are compared.
fn run_against_ethereum(fork: EthSpecId, db: MemoryDatabase, tx: TxEnv) -> (Outcome, Outcome, u64) {
    let block = BlockEnv { basefee: 0, ..block() };
    let tx = TxEnv { gas_price: 0, ..tx };
    let ctx = MegaContext::new(db.clone(), MegaSpecId::SATIN)
        .with_neutral_cfg(neutral_cfg(fork).expect("a neutral fork"))
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits())
        .with_block(block.clone())
        .with_chain(zero_fee_l1_block_info());
    let mut mega = MegaEvm::new(ctx);
    neutralize_evm(&mut mega, fork).expect("a neutral fork");
    let outcome = mega.execute_transaction(OpTx(op_transaction(tx.clone()))).unwrap();
    let mut state = outcome.result_and_state.state;
    for vault in [BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT] {
        if state.get(&vault).is_some_and(|account| account.is_empty()) {
            state.remove(&vault);
        }
    }
    let mega = ExecResultAndState::new(outcome.result_and_state.result, state);

    let mut cfg = CfgEnv::new();
    cfg.set_spec_and_mainnet_gas_params(fork);
    let eth = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(block)
        .build_mainnet()
        .transact(tx)
        .unwrap();
    let eth = ExecResultAndState::new(eth.result.map_haltreason(OpHaltReason::Base), eth.state);
    (mega, eth, outcome.gas.history)
}

/// A caller whose code is `code`, funded, with an empty callee.
fn with_code(code: Bytes) -> MemoryDatabase {
    funded().account_code(CALLEE, code)
}

#[test]
fn test_neutral_programs_match_ethereum() {
    let log = BytecodeBuilder::default()
        .mstore(0, U256::from(7).to_be_bytes::<32>())
        .push_number(1u8)
        .push_number(32u8)
        .append(PUSH0)
        .append(LOG1)
        .stop()
        .build();
    let cases = [
        ("transfer", funded(), call(Bytes::new(), U256::from(1_000))),
        (
            "sstore",
            with_code(BytecodeBuilder::default().sstore(U256::ZERO, U256::from(42)).stop().build()),
            call(Bytes::new(), U256::ZERO),
        ),
        ("log", with_code(log), call(Bytes::new(), U256::ZERO)),
        (
            "creation",
            funded(),
            TxEnv {
                kind: TxKind::Create,
                data: BytecodeBuilder::default()
                    .return_with_data([0x60, 0x00, 0x60, 0x00, 0xf3])
                    .build(),
                ..call(Bytes::new(), U256::ZERO)
            },
        ),
    ];
    for fork in FORKS {
        for (name, db, tx) in cases.clone() {
            let outcome = run_against_ethereum(fork, db, tx);
            assert!(outcome.1.result.is_success(), "{name}: {:?}", outcome.1.result);
            assert_same(fork, outcome);
        }
    }
}

/// The neutral configuration, under the limits the gate's runner installs with it, detains
/// nothing: `no_limits` leaves detention's caps unlimited, so a program that reads the block
/// environment and the block beneficiary's account records no read and sets no compute limit, and
/// matches Ethereum.
#[test]
fn test_neutral_reads_of_volatile_data_detain_nothing() {
    let code = BytecodeBuilder::default()
        .append_many([TIMESTAMP, NUMBER, revm::bytecode::opcode::COINBASE, BALANCE, POP, POP, POP])
        .sstore(U256::ZERO, U256::from(1))
        .stop()
        .build();
    let block = BlockEnv { basefee: 0, ..block() };
    for fork in FORKS {
        let ctx = MegaContext::new(with_code(code.clone()), MegaSpecId::SATIN)
            .with_neutral_cfg(neutral_cfg(fork).expect("a neutral fork"))
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits())
            .with_block(block.clone())
            .with_chain(zero_fee_l1_block_info());
        let mut mega = MegaEvm::new(ctx);
        neutralize_evm(&mut mega, fork).expect("a neutral fork");
        let tx = TxEnv { gas_price: 0, ..call(Bytes::new(), U256::ZERO) };
        let outcome = mega.execute_transaction(OpTx(op_transaction(tx))).unwrap();
        assert!(outcome.result.is_success(), "{fork:?}");
        assert!(!mega.ctx().detention().detains(), "{fork:?}");
        assert_eq!(mega.ctx().detention().accessed(), VolatileDataAccess::empty(), "{fork:?}");
        assert_eq!(mega.ctx().detention().compute_limit(), None, "{fork:?}");

        let outcome =
            run_against_ethereum(fork, with_code(code.clone()), call(Bytes::new(), U256::ZERO));
        assert!(outcome.0.result.is_success());
        assert_same(fork, outcome);
    }
}

/// `EXTCODESIZE` pays Amsterdam's static price under Amsterdam's configuration, which Satin's
/// instruction table, on its Osaka base, does not carry.
#[test]
fn test_neutral_extcodesize_matches_ethereum() {
    let code =
        BytecodeBuilder::default().append(ADDRESS).append(EXTCODESIZE).append(POP).stop().build();
    for fork in FORKS {
        let outcome =
            run_against_ethereum(fork, with_code(code.clone()), call(Bytes::new(), U256::ZERO));
        assert!(outcome.0.result.is_success());
        assert_same(fork, outcome);
    }
}

/// The c-kzg test vector `verify_kzg_proof_case_correct_proof_4_4`, as the precompile takes it:
/// `versioned_hash ++ z ++ y ++ commitment ++ proof`.
fn kzg_input() -> Vec<u8> {
    let commitment = hex!(
        "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca2\
         5f26936857bc3a7c2539ea8ec3a952b7"
    );
    let mut input =
        revm::precompile::kzg_point_evaluation::kzg_to_versioned_hash(&commitment).to_vec();
    input.extend(hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000"));
    input.extend(hex!("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9"));
    input.extend(commitment);
    input.extend(hex!(
        "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc216074\
         4faf0070725e00b60ad9a026a15b1a8c"
    ));
    input
}

/// A KZG point evaluation forwarded 60,000 gas succeeds, as upstream prices it at 50,000: the
/// fork's precompile set, not Satin's, which prices it at 100,000.
#[test]
fn test_neutral_kzg_call_matches_ethereum() {
    let code = BytecodeBuilder::default()
        .mstore(0, kzg_input())
        .push_number(64u8)
        .append(PUSH0)
        .push_number(192u8)
        .append(PUSH0)
        .append(PUSH0)
        .push_address(revm::precompile::kzg_point_evaluation::ADDRESS)
        .push_number(60_000u32)
        .append(CALL)
        .append(PUSH0)
        .append(SSTORE)
        .stop()
        .build();
    for fork in FORKS {
        let outcome =
            run_against_ethereum(fork, with_code(code.clone()), call(Bytes::new(), U256::ZERO));
        assert_eq!(
            outcome.0.state[&CALLEE].storage[&U256::ZERO].present_value,
            U256::from(1),
            "{fork:?}: the evaluation succeeded"
        );
        assert_same(fork, outcome);
    }
}
