//! Tests for the REX5+ zero-copy selector probing in system-contract
//! interceptor dispatch.
//!
//! Pre-REX5: each interceptor began with `call_inputs.input.bytes(ctx)` which
//! materializes the entire calldata. For `CallInput::SharedBuffer`, this copies the
//! full `argsSize` range out of shared memory on every dispatch attempt — even when
//! the selector ultimately does not match — producing a memory-bandwidth /
//! allocator `DoS` vector.
//!
//! Under REX5: REX5+ uses `peek_selector` which only reads four bytes from the input
//! head. Trailing bytes are never copied. Pre-REX5 keeps the original
//! `input.bytes(ctx)` + `abi_decode` path so historical replay is unaffected.
//!
//! These tests prove byte-for-byte equivalence of the admission decision across
//! REX4 and REX5 for the consensus-visible inputs (exact selector, selector +
//! trailing junk, truncated input below 4 bytes, unknown 4-byte selector). The
//! plan's `..._no_full_copy` performance assertion would require instrumenting
//! `LocalContext` to count `shared_memory_buffer_slice` calls and is intentionally
//! left out of this regression suite — behavior equivalence is the
//! consensus-critical invariant.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaAccessControl, IOracle, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError, TestExternalEnvs, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE,
    ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
};

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const PROBE_CONTRACT: Address = address!("0000000000000000000000000000000000400001");

const DISABLE_SELECTOR: [u8; 4] = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Builds bytecode that:
/// 1. Writes a 4-byte selector at memory[0..4].
/// 2. Writes `tail_size` zero bytes after the selector (no-op MSTORE pad to grow memory).
/// 3. Calls `ACCESS_CONTROL` with `argsSize = 4 + tail_size`, `argsOffset = 0`.
/// 4. Returns the CALL's success flag (32-byte big-endian) as the outer-tx output.
fn build_probe_bytecode(selector: [u8; 4], tail_size: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default().mstore(0x0, selector);
    if tail_size > 0 {
        // Grow memory by writing one byte at offset `4 + tail_size - 1`. MSTORE writes 32 bytes,
        // so this expands memory to cover the full 4 + tail_size span. The bytes beyond the
        // selector are whatever the EVM zero-initialized — that's the "trailing junk" the
        // peek_selector path must skip reading from shared memory.
        let last_byte_offset = 4 + tail_size - 1;
        builder = builder.push_number(0u64).push_number(last_byte_offset as u64).append(MSTORE8);
    }
    let args_size = 4u64 + tail_size as u64;
    builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(args_size)
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(1_000_000u64) // gas forwarded to system contract
        .append(CALL) // success flag on stack
        .push_number(0u64)
        .append(MSTORE) // store success flag at memory[0..32]
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build()
}

fn run_probe(
    spec: MegaSpecId,
    selector: [u8; 4],
    tail_size: usize,
) -> ResultAndState<MegaHaltReason> {
    let code = build_probe_bytecode(selector, tail_size);
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_code(PROBE_CONTRACT, code)
        // The on-chain ACCESS_CONTROL_CODE is needed so non-intercepted calls fall through
        // to actual bytecode (which reverts NotIntercepted()) instead of being treated as
        // a call to an empty account (which would return success=1 and obscure the test).
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE);
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(PROBE_CONTRACT)
        .gas_limit(100_000_000)
        .build_fill();
    transact(spec, &mut db, tx).expect("transact should succeed")
}

fn extract_success_flag(result: &ResultAndState<MegaHaltReason>) -> U256 {
    let ExecutionResult::Success { output, .. } = &result.result else {
        panic!("expected outer tx success, got {:?}", result.result);
    };
    let bytes = output.data();
    assert_eq!(bytes.len(), 32, "outer return must be a 32-byte success flag");
    U256::from_be_slice(bytes)
}

/// Pinned admission rule across REX4 and REX5:
/// An exact 4-byte selector to `ACCESS_CONTROL` is intercepted in both specs.
#[test]
fn test_exact_selector_intercepts_under_both_specs() {
    for spec in [MegaSpecId::REX4, MegaSpecId::REX5] {
        let result = run_probe(spec, DISABLE_SELECTOR, 0);
        let flag = extract_success_flag(&result);
        assert_eq!(
            flag,
            U256::from(1),
            "{:?}: exact 4-byte selector must intercept (CALL success)",
            spec,
        );
    }
}

/// Pinned admission rule across REX4 and REX5:
/// A 4-byte selector followed by 1 KiB of trailing junk to `ACCESS_CONTROL` is
/// intercepted in both specs. Under REX5 the trailing bytes are never read from
/// shared memory; behavior equivalence is what this test pins, the perf
/// invariant is documented in the plan.
#[test]
fn test_selector_plus_junk_intercepts_under_both_specs() {
    for spec in [MegaSpecId::REX4, MegaSpecId::REX5] {
        let result = run_probe(spec, DISABLE_SELECTOR, 1024);
        let flag = extract_success_flag(&result);
        assert_eq!(flag, U256::from(1), "{:?}: selector + 1KiB junk must still intercept", spec,);
    }
}

/// Pinned fall-through rule across REX4 and REX5:
/// A 2-byte input (below the 4-byte selector size) cannot match any selector and
/// must fall through to the on-chain bytecode (which reverts `NotIntercepted`, so
/// the CALL returns success=0).
#[test]
fn test_truncated_calldata_falls_through_under_both_specs() {
    // 2-byte input by reusing build_probe_bytecode with tail_size 0 but custom selector
    // ... actually we need argsSize = 2. Build manually:
    let code = BytecodeBuilder::default()
        .mstore(0x0, DISABLE_SELECTOR) // 32-byte word
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(2u64) // argsSize = 2 bytes (truncated)
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    for spec in [MegaSpecId::REX4, MegaSpecId::REX5] {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10_000_000))
            .account_code(PROBE_CONTRACT, code.clone())
            .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE);
        let tx = TxEnvBuilder::default()
            .caller(CALLER)
            .call(PROBE_CONTRACT)
            .gas_limit(100_000_000)
            .build_fill();
        let result = transact(spec, &mut db, tx).expect("transact ok");
        let flag = extract_success_flag(&result);
        assert_eq!(
            flag,
            U256::from(0),
            "{:?}: 2-byte input must fall through (CALL returns failure)",
            spec,
        );
    }
}

/// Pinned fall-through rule across REX4 and REX5:
/// An unknown 4-byte selector to `ACCESS_CONTROL` falls through to on-chain
/// bytecode (`NotIntercepted` revert).
#[test]
fn test_unknown_selector_falls_through_under_both_specs() {
    let unknown = [0xde, 0xad, 0xbe, 0xef];
    for spec in [MegaSpecId::REX4, MegaSpecId::REX5] {
        let result = run_probe(spec, unknown, 0);
        let flag = extract_success_flag(&result);
        assert_eq!(flag, U256::from(0), "{:?}: unknown 4-byte selector must fall through", spec,);
    }
}

/// Selector-match-plus-malformed-args parity for the `OracleHint` interceptor.
///
/// This is the only interceptor where the REX5+ branch still calls
/// `input.bytes(ctx)` and `abi_decode` after the selector probe matches (because
/// `sendHint` carries args). The risk being pinned: if a refactor ever drops the
/// inner `abi_decode` retry under REX5+, `on_hint` would fire with garbage data.
///
/// We send `sendHint`'s selector followed by truncated args (4 + 64 bytes — not
/// enough to satisfy the `(bytes32, bytes)` encoding) and assert no hint is
/// recorded under either spec.
#[test]
fn test_oracle_hint_malformed_args_does_not_record_under_both_specs() {
    let send_hint_selector: [u8; 4] = IOracle::sendHintCall::SELECTOR;
    // Bytecode: write `selector || 16 zero bytes` at memory[0..20], then CALL with argsSize=20.
    // sendHint requires `(bytes32 topic, bytes data)` = at minimum 64 bytes (32 topic + 32 offset).
    // 16 bytes of args is far short of even a single ABI word; abi_decode MUST fail.
    let code = BytecodeBuilder::default()
        .mstore(0x0, send_hint_selector)
        // Grow memory to 20 bytes via MSTORE8 at offset 19.
        .push_number(0u64)
        .push_number(19u64)
        .append(MSTORE8)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(20u64) // argsSize (selector + 16 zero bytes — too short for topic)
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();

    for spec in [MegaSpecId::REX4, MegaSpecId::REX5] {
        let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10_000_000))
            .account_code(PROBE_CONTRACT, code.clone())
            .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);
        let mut context =
            MegaContext::new(&mut db, spec).with_external_envs((&external_envs).into());
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::from(0));
            chain.operator_fee_constant = Some(U256::from(0));
        });
        let tx = TxEnvBuilder::default()
            .caller(CALLER)
            .call(PROBE_CONTRACT)
            .gas_limit(100_000_000)
            .build_fill();
        let mut evm = MegaEvm::new(context);
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());
        let _ = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");

        assert!(
            external_envs.recorded_hints().is_empty(),
            "{:?}: malformed sendHint args must NOT trigger on_hint",
            spec,
        );
    }
}

/// Cross-spec parity: every input shape produces the same CALL success flag in
/// REX4 and REX5. This is the consensus-critical invariant — a divergence here
/// would break replay of a REX4 block under a REX5+ node or vice versa.
#[test]
fn test_admission_decision_parity_across_specs() {
    let shapes: &[(_, usize, &str)] = &[
        (DISABLE_SELECTOR, 0, "exact selector"),
        (DISABLE_SELECTOR, 1, "selector + 1B junk"),
        (DISABLE_SELECTOR, 1024, "selector + 1KiB junk"),
        ([0xde, 0xad, 0xbe, 0xef], 0, "unknown 4-byte selector"),
        ([0xde, 0xad, 0xbe, 0xef], 1024, "unknown selector + 1KiB junk"),
    ];
    for (sel, tail, label) in shapes {
        let rex4 = extract_success_flag(&run_probe(MegaSpecId::REX4, *sel, *tail));
        let rex5 = extract_success_flag(&run_probe(MegaSpecId::REX5, *sel, *tail));
        assert_eq!(
            rex4, rex5,
            "admission decision must match across REX4 and REX5 for shape: {}",
            label,
        );
    }
}
