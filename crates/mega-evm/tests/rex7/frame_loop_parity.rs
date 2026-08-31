//! The two frame loops must agree, on every shape a frame can end in.
//!
//! `frame_run` and `inspect_frame_run` are separate functions, and so are `frame_init` and
//! `inspect_frame_init`. What they share is a body: the frame's settlement point, the reading the
//! frozen post-action charge is measured against, the guards in front of interceptor dispatch, and
//! the journal decision. The inspected copies add exactly one thing to it — the callback that can
//! rewrite a frame's classification — and an observation-only inspector rewrites nothing.
//!
//! So with such an inspector attached, every quantity a transaction produces has to be identical
//! to the uninspected run: the receipt, the four resource dimensions, the enforced / destroyed
//! split, and the state. A difference means one loop reached a settlement the other did not.
//!
//! The cases below are not representative samples. They are one per branch of the frame lifecycle
//! that can end a frame: the classification arms of a contract creation (accepted, oversized,
//! `0xEF`-prefixed, unaffordable deposit, reverted constructor, occupied address), a call frame's
//! three outcomes, the frame inits that refuse to build a frame at all, a precompile, and the two
//! suspension shapes that must settle nothing.

use crate::common::{CALLEE, CALLER, CONTRACT, EMPTY_TARGET, ONE_ETH};
use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    constants::mini_rex::MAX_CONTRACT_SIZE,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, INVALID, MSTORE8, PUSH0, RETURN, REVERT, STATICCALL, STOP},
    context::{tx::TxEnvBuilder, CfgEnv},
    inspector::NoOpInspector,
    primitives::TxKind,
    state::EvmState,
};
use std::{collections::BTreeMap, string::String};

/// High enough that EVM gas is never what binds, except where a case says otherwise.
const TX_GAS_LIMIT: u64 = 30_000_000;
/// `ecrecover`, the cheapest precompile to reach with junk input.
const ECRECOVER: Address = address!("0000000000000000000000000000000000000001");

/// Everything one transaction produced, in a form two runs can be compared field by field.
#[derive(Debug, PartialEq, Eq)]
struct Reading {
    result: String,
    compute_gas: u64,
    enforced: u64,
    destroyed: u64,
    data_size: u64,
    kv_updates: u64,
    state_growth: u64,
    gas_used: u64,
    total_gas_spent: u64,
    state: String,
}

/// Renders the produced state in a canonical, order-independent form.
///
/// Comparing the state is what makes these cases cover the journal decision rather than only the
/// accounting: a frame committed on one loop and reverted on the other shows up here and nowhere
/// else.
fn render_state(state: &EvmState) -> String {
    let canonical: BTreeMap<Address, (U256, u64, B256, BTreeMap<U256, U256>)> = state
        .iter()
        .map(|(address, account)| {
            let storage = account
                .storage
                .iter()
                .map(|(slot, value)| (*slot, value.present_value()))
                .collect();
            (*address, (account.info.balance, account.info.nonce, account.info.code_hash, storage))
        })
        .collect();
    std::format!("{canonical:?}")
}

/// Runs `case` once under `spec`, with the inspector either driving the inspected loops or
/// switched off.
///
/// Both arms build the same `MegaEvm` type and toggle the flag, so the only thing that changes is
/// which pair of loops runs — not the inspector, not the context, not the transaction.
fn run_under(case: &Case, spec: MegaSpecId, inspected: bool) -> Reading {
    let mut db = (case.db)();
    let mut cfg = CfgEnv::default();
    cfg.spec = spec;
    cfg.limit_contract_code_size = Some(case.code_size_limit.unwrap_or(MAX_CONTRACT_SIZE));
    let mut context = MegaContext::new(&mut db, spec)
        .with_cfg(cfg)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(CALLER)
            .kind(case.kind)
            .data(case.data.clone())
            .value(case.value)
            .gas_limit(case.gas_limit)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm: MegaEvm<_, NoOpInspector, EmptyExternalEnv> =
        MegaEvm::new(context).with_inspector(NoOpInspector);
    if !inspected {
        alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    }
    let outcome: MegaTransactionOutcome =
        evm.execute_transaction(tx).expect("tx should not surface EVMError");

    Reading {
        result: std::format!("{:?}", outcome.result_and_state.result),
        compute_gas: outcome.compute_gas_used,
        enforced: outcome.compute_gas_enforced,
        destroyed: outcome.compute_gas_destroyed,
        data_size: outcome.data_size,
        kv_updates: outcome.kv_updates,
        state_growth: outcome.state_growth_used,
        gas_used: outcome.result_and_state.result.tx_gas_used(),
        total_gas_spent: outcome.result_and_state.result.gas().total_gas_spent(),
        state: render_state(&outcome.result_and_state.state),
    }
}

/// One frame-lifecycle shape, and how to reach it.
struct Case {
    /// What the case pins, used as the assertion label.
    name: &'static str,
    db: fn() -> MemoryDatabase,
    kind: TxKind,
    data: Bytes,
    value: U256,
    gas_limit: u64,
    /// A lowered contract-size limit, for the case that needs revm's size reject. `MegaETH`'s own
    /// 512 KiB limit is far past what a constructor can afford to return under the per-byte
    /// storage gas.
    code_size_limit: Option<usize>,
    /// Asserted against the plain run, so a case that stops reaching its shape fails loudly
    /// instead of comparing two runs of something else.
    expect: fn(&Reading),
}

fn base_db() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(ONE_ETH))
}

fn caller_db(code: Bytes) -> MemoryDatabase {
    base_db().account_code(CONTRACT, code)
}

/// Init code returning `len` bytes of runtime code, the first of them `first_byte`.
fn init_code_returning(len: u64, first_byte: u8) -> Bytes {
    BytecodeBuilder::default()
        .push_number(u128::from(first_byte))
        .push_number(0u64)
        .append(MSTORE8)
        .push_number(u128::from(len))
        .push_number(0u64)
        .append(RETURN)
        .build()
}

/// A contract whose body issues one `CALL` to `target`, forwarding `gas`, then stops.
fn calls(target: Address, gas: u64, value: u128) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(u128::from(gas))
        .append(CALL)
        .append(STOP)
        .build()
}

fn assert_success(r: &Reading) {
    assert!(r.result.starts_with("Success"), "expected a success, got {}", r.result);
}

fn assert_revert(r: &Reading) {
    assert!(r.result.starts_with("Revert"), "expected a revert, got {}", r.result);
}

fn assert_halt(r: &Reading) {
    assert!(r.result.starts_with("Halt"), "expected a halt, got {}", r.result);
}

/// A halt with a named reason, so a case that stops reaching its classification arm fails rather
/// than comparing two runs of a different failure.
fn assert_halt_reason(r: &Reading, reason: &str) {
    assert_halt(r);
    assert!(r.result.contains(reason), "expected a {reason} halt, got {}", r.result);
}

/// The frame's remainder was destroyed rather than handed back — the shape whose booking site sits
/// on one side of the callback and whose derivation sits on the other.
fn assert_destroyed(r: &Reading) {
    assert!(r.destroyed > 0, "expected a destroyed remainder, got {r:?}");
}

fn cases() -> Vec<Case> {
    std::vec![
        Case {
            name: "CALL succeeds and commits its storage write",
            db: || {
                caller_db(calls(CALLEE, 200_000, 0)).account_code(
                    CALLEE,
                    BytecodeBuilder::default().sstore(U256::from(1), U256::from(7)).stop().build(),
                )
            },
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "CALL reverts and its storage write is rolled back",
            db: || {
                caller_db(calls(CALLEE, 200_000, 0)).account_code(
                    CALLEE,
                    BytecodeBuilder::default()
                        .sstore(U256::from(1), U256::from(7))
                        .revert()
                        .build(),
                )
            },
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "CALL halts on INVALID and destroys its forwarded budget",
            db: || {
                caller_db(calls(CALLEE, 200_000, 0))
                    .account_code(CALLEE, Bytes::from_static(&[INVALID]))
            },
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: |r| {
                assert_success(r);
                assert_destroyed(r);
            },
        },
        Case {
            name: "CALL into empty code stops without a frame",
            db: || caller_db(calls(EMPTY_TARGET, 200_000, 0)),
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "CALL is refused for want of balance",
            db: || caller_db(calls(CALLEE, 200_000, 1))
                .account_code(CALLEE, init_code_returning(1, 0x00)),
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "STATICCALL reaches a precompile, which returns without a frame",
            db: || {
                caller_db(
                    BytecodeBuilder::default()
                        .push_number(0u64)
                        .push_number(0u64)
                        .push_number(32u64)
                        .push_number(0u64)
                        .push_address(ECRECOVER)
                        .push_number(100_000u64)
                        .append(STATICCALL)
                        .append(STOP)
                        .build(),
                )
            },
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "CREATE deposits its code",
            db: base_db,
            kind: TxKind::Create,
            data: init_code_returning(64, 0x00),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "CREATE is rejected for an oversized runtime code",
            db: base_db,
            kind: TxKind::Create,
            data: init_code_returning(64, 0x00),
            value: U256::ZERO,
            code_size_limit: Some(32),
            gas_limit: TX_GAS_LIMIT,
            expect: |r| {
                assert_halt_reason(r, "CreateContractSizeLimit");
                assert_destroyed(r);
            },
        },
        Case {
            name: "CREATE is rejected for an 0xEF-prefixed runtime code",
            db: base_db,
            kind: TxKind::Create,
            data: init_code_returning(4, 0xEF),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: |r| {
                assert_halt_reason(r, "CreateContractStartingWithEF");
                assert_destroyed(r);
            },
        },
        Case {
            name: "CREATE runs out of gas paying for its own code",
            db: base_db,
            // 1000 bytes of runtime code cost ten million gas to store; the limit below lets the
            // constructor run and leaves it unable to pay for what it returned.
            data: init_code_returning(1_000, 0x00),
            kind: TxKind::Create,
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: 150_000,
            expect: |r| assert_halt_reason(r, "OutOfGas"),
        },
        Case {
            name: "CREATE's constructor reverts",
            db: base_db,
            kind: TxKind::Create,
            data: BytecodeBuilder::default().revert().build(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_revert,
        },
        Case {
            name: "CREATE onto an occupied address collides",
            db: || {
                caller_db(
                    BytecodeBuilder::default()
                        .push_number(0u64)
                        .push_number(0u64)
                        .push_number(0u64)
                        .append(CREATE)
                        .append(STOP)
                        .build(),
                )
                // The address CONTRACT's first CREATE derives, pre-occupied with code.
                .account_code(CONTRACT.create(1), Bytes::from_static(&[STOP]))
                .account_nonce(CONTRACT, 1)
            },
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: |r| {
                assert_success(r);
                assert_destroyed(r);
            },
        },
        Case {
            name: "a nested CALL suspends its caller without settling it",
            db: || {
                caller_db(calls(CALLEE, 500_000, 0))
                    .account_code(CALLEE, calls(EMPTY_TARGET, 100_000, 0))
            },
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_success,
        },
        Case {
            name: "the top-level frame itself halts",
            db: || caller_db(Bytes::from_static(&[PUSH0, PUSH0, REVERT])),
            kind: TxKind::Call(CONTRACT),
            data: Bytes::new(),
            value: U256::ZERO,
            code_size_limit: None,
            gas_limit: TX_GAS_LIMIT,
            expect: assert_revert,
        },
    ]
}

/// Every frame-lifecycle shape, run through both loops, compared on everything a transaction
/// produces.
///
/// The state comparison is what covers the journal decision: the loops now decide it themselves,
/// after the callback, and a frame committed on one and reverted on the other is invisible in the
/// receipt of a transaction whose caller absorbed the difference.
#[test]
fn test_both_frame_loops_agree_on_every_frame_outcome() {
    for case in cases() {
        let plain = run_under(&case, MegaSpecId::REX7, false);
        (case.expect)(&plain);
        let inspected = run_under(&case, MegaSpecId::REX7, true);
        assert_eq!(
            plain, inspected,
            "{}: an observation-only inspector must change nothing",
            case.name,
        );
    }
}

/// The same matrix under the frozen spec the REX7 loops share their body with.
///
/// The loops are not spec-gated — only where they take the journal decision is — so a settlement
/// that reaches one loop and not the other would show up here too, on a spec whose behaviour is
/// closed.
#[test]
fn test_both_frame_loops_agree_on_every_frame_outcome_under_rex6() {
    for case in cases() {
        let plain = run_under(&case, MegaSpecId::REX6, false);
        let inspected = run_under(&case, MegaSpecId::REX6, true);
        assert_eq!(
            plain, inspected,
            "{}: an observation-only inspector must change nothing under REX6",
            case.name,
        );
    }
}
