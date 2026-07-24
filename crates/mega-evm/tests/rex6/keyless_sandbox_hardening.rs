//! REX6 `KeylessDeploy` sandbox hardening tests.
//!
//! Covers two behavioral changes in `sandbox/execution.rs`:
//!
//! - **Keyless overhead OOG rescue.** When the keyless dispatch overhead
//!   (`KEYLESS_DEPLOY_OVERHEAD_GAS`) pushes the `AdditionalLimit` compute-gas tracker over its
//!   tx-level cap, `execute_keyless_deploy_call` halts with a full-spend OOG. REX6+ routes through
//!   `AdditionalLimit::try_rescue_gas` so the unused outer-gas remainder is refunded; pre-REX6
//!   leaves it fully spent.
//!
//! - **Create-then-SELFDESTRUCT detection.** Pre-REX6 `process_sandbox_transact_result` treats
//!   non-empty `Output::Create(bytecode, addr)` as a successful deploy without checking whether the
//!   sandbox-state account at `addr` was marked `SelfDestructed`.
//!   `apply_sandbox_created_selfdestruct` then zeroes that account on merge, so the wire-reported
//!   `deployedAddress` points at an account with no on-chain code. REX6+ folds the
//!   create-then-selfdestruct case onto the same empty-code branch as `bytecode.is_empty()`.
//!
//! REX5 baselines are included so each fix is shown to actually move
//! behavior. Pre-REX6 dispatch paths are unchanged (REX5 204-test suite
//! freezes them).

use std::vec::Vec;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ExecutionResult,
    sandbox::{calculate_keyless_deploy_address, decode_error_result, KeylessDeployError},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{DELEGATECALL, MSTORE8, POP, RETURN, SELFDESTRUCT},
    context::TxEnv,
    inspector::NoOpInspector,
};

const RELAYER: Address = address!("0000000000000000000000000000000000990000");
const SIGNED_GAS_PRICE: u128 = 100_000_000_000;
const SIGNED_GAS_LIMIT: u64 = 100_000;
const OUTER_GAS_LIMIT: u64 = 30_000_000;
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;
/// Helper contract used by the create-then-SELFDESTRUCT test —
/// invokes `SELFDESTRUCT(0)` so a DELEGATECALL from a constructor
/// destroys the constructor's own (being-created) account.
const SELFDESTRUCT_HELPER: Address = address!("00000000000000000000000000000000000abcde");

// ============================================================================
// HELPERS
// ============================================================================

/// Builds a deterministic pre-EIP-155 keyless tx with the given init code.
/// Mirrors the helper in `tests/rex5/keyless_empty_code_logs.rs`.
fn build_keyless_tx_with_init_code(init_code: Bytes) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: SIGNED_GAS_PRICE,
        gas_limit: SIGNED_GAS_LIMIT,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };
    let r = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let s = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let sig = Signature::new(r, s, false);
    let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    let tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("should recover signer");
    (tx_bytes, signer)
}

/// Runs the outer `keylessDeployCall` and returns the parent `ExecutionResult`.
///
/// `tx_compute_gas_limit` lets the overhead-OOG test squeeze the
/// `AdditionalLimit` compute-gas tracker below `KEYLESS_DEPLOY_OVERHEAD_GAS`
/// so the tx-level exceed path fires inside `execute_keyless_deploy_call`'s
/// dispatch-overhead charge.
///
/// `outer_gas_limit_override` lets tests that need to exercise behavior past
/// the default `OUTER_GAS_LIMIT` (e.g., the EIP-170 size-limit invariant test
/// which needs enough gas for a 512KB+ `CODE_DEPOSIT_COST` charge) widen the
/// envelope. `None` defaults to `OUTER_GAS_LIMIT`.
fn run_keyless_outer(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
    tx_compute_gas_limit: Option<u64>,
    outer_gas_limit_override: Option<u64>,
) -> ExecutionResult<MegaHaltReason> {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    if let Some(limit) = tx_compute_gas_limit {
        context = context.with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(limit),
        );
    }

    let tx = TxEnv {
        caller: RELAYER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: outer_gas_limit_override.unwrap_or(OUTER_GAS_LIMIT),
        gas_price: 0,
        chain_id: Some(1),
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    alloy_evm::Evm::transact_commit(&mut evm, tx)
        .expect("outer keyless call should not fail at the EVM-error level")
}

/// Extract `IKeylessDeploy::keylessDeployReturn` payload from a Success-style outer result.
fn decode_keyless_return(
    result: &ExecutionResult<MegaHaltReason>,
) -> IKeylessDeploy::keylessDeployReturn {
    let output = match result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        other => panic!("expected Success-style outer result; got {other:?}"),
    };
    IKeylessDeploy::keylessDeployCall::abi_decode_returns(&output)
        .expect("Success output must decode as keylessDeployReturn")
}

// ============================================================================
// Sandbox success that selfdestructed during the constructor
// ============================================================================

/// REX6: a constructor that DELEGATECALLs a helper which SELFDESTRUCTs (so the
/// being-created account is marked `SelfDestructed`) but RETURNs non-empty
/// bytecode must surface as `EmptyCodeDeployed` with
/// `deployedAddress = 0x0` — matching what the parent-merged state actually
/// shows. REX5 reports `Deployed { deployedAddress = <addr> }` even though
/// the merged account has no code on-chain.
///
/// The DELEGATECALL-into-`SELFDESTRUCT(0)` shape is the minimal construction;
/// the same surface is reachable by any constructor that selfdestructs the
/// being-created account via an indirect call before `RETURN`-ing non-empty
/// bytecode. The attacker payoff: integrators that check
/// `deployedAddress != 0x0` and stop there (without re-fetching code at the
/// reported address) see a "successful deploy" for an account that has no
/// on-chain code, with the signer's Nick's-Method replay barrier consumed.
#[test]
fn test_rex6_create_then_selfdestruct_surfaces_as_empty_code() {
    // Init code:
    //   PUSH1 0x00 (retSize=0) PUSH1 0x00 (retOff=0) PUSH1 0x00 (argsSize=0)
    //   PUSH1 0x00 (argsOff=0) PUSH20 helper PUSH1 0x64 (gas=100) DELEGATECALL POP
    //   // After DELEGATECALL: the executing account has been SELFDESTRUCTed.
    //   PUSH1 0xff (some non-zero byte)
    //   PUSH1 0x00 MSTORE8
    //   PUSH1 0x01 PUSH1 0x00 RETURN
    let init_code = BytecodeBuilder::default()
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(0_u8) // argsSize
        .push_number(0_u8) // argsOffset
        .push_address(SELFDESTRUCT_HELPER)
        .push_number(100_000_u64) // gas
        .append(DELEGATECALL)
        .append(POP)
        // Now selfdestructed via DELEGATECALL — but still return non-empty bytecode.
        .push_number(0xff_u8)
        .push_number(0_u8)
        .append(MSTORE8)
        .push_number(1_u8) // size
        .push_number(0_u8) // offset
        .append(RETURN)
        .build();

    // Helper at SELFDESTRUCT_HELPER does:
    //   PUSH1 0x00 SELFDESTRUCT
    let helper_code = BytecodeBuilder::default().push_number(0_u8).append(SELFDESTRUCT).build();

    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let build_db = || {
        let mut db = MemoryDatabase::default();
        db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
        db.set_account_code(SELFDESTRUCT_HELPER, helper_code.clone());
        db
    };

    // REX5 baseline: reports Deployed at `deploy_address`, but the on-chain
    // code at `deploy_address` is empty (the bug).
    let mut db_rex5 = build_db();
    let result_rex5 = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db_rex5,
        keyless_tx_bytes.clone(),
        LARGE_GAS_LIMIT_OVERRIDE,
        None,
        None,
    );
    let decoded_rex5 = decode_keyless_return(&result_rex5);
    assert_eq!(
        decoded_rex5.deployedAddress, deploy_address,
        "REX5 surface: sandbox reports a non-zero deployedAddress even though the \
         constructor selfdestructed the account",
    );
    let rex5_code_hash = {
        use revm::Database as _;
        db_rex5.basic(deploy_address).unwrap().map(|info| info.code_hash).unwrap_or_default()
    };
    // REX5: the wire reports Deployed but the merged on-chain account has no code.
    let rex5_code = {
        use revm::Database as _;
        if rex5_code_hash == revm::primitives::KECCAK_EMPTY {
            Vec::new()
        } else {
            db_rex5.code_by_hash(rex5_code_hash).unwrap().bytes_slice().to_vec()
        }
    };
    assert!(
        rex5_code.is_empty(),
        "REX5 baseline: merged account at deployedAddress has no code — the divergence \
         REX6 fixes; got {rex5_code:?}",
    );

    // REX6: same setup, but the wire now matches the chain — EmptyCodeDeployed
    // with `deployedAddress = 0x0`.
    let mut db_rex6 = build_db();
    let result_rex6 = run_keyless_outer(
        MegaSpecId::REX6,
        &mut db_rex6,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        None,
        None,
    );
    let decoded_rex6 = decode_keyless_return(&result_rex6);
    assert_eq!(
        decoded_rex6.deployedAddress,
        Address::ZERO,
        "REX6 must report deployedAddress=0x0 for create+SELFDESTRUCT",
    );
    let decoded_err = decode_error_result(&decoded_rex6.errorData);
    assert!(
        matches!(decoded_err, Some(KeylessDeployError::EmptyCodeDeployed { .. })),
        "REX6 errorData must decode as EmptyCodeDeployed; got {decoded_err:?}",
    );
}

// ============================================================================
// Keyless dispatch-overhead OOG rescues parent gas
// ============================================================================

/// REX6: when the `KEYLESS_DEPLOY_OVERHEAD_GAS` charge exceeds the
/// `AdditionalLimit`'s tx-level compute-gas cap, the outer Gas counter's
/// unused remainder must be rescued. Pre-REX6 leaves the unused remainder
/// fully spent so the sender pays the entire outer envelope for a halt that
/// barely touched anything.
///
/// We compare the outer tx's reported `gas_used` between REX5 and REX6 and
/// pin both an inequality (REX6 < REX5) and an absolute lower bound on the
/// rescue delta. The bound is set well below the realistic rescue amount
/// (≈ outer envelope − intrinsic − overhead) but high enough that a
/// partial-rescue regression (e.g., rescuing only the overhead, or missing
/// the post-overhead remainder) would fail it.
#[test]
fn test_rex6_keyless_overhead_oog_rescues_parent_gas() {
    // Minimal init code — never executes because the 100K overhead halts first.
    let init_code =
        BytecodeBuilder::default().push_number(0_u8).push_number(0_u8).append(RETURN).build();
    let (keyless_tx_bytes, _signer) = build_keyless_tx_with_init_code(init_code);

    // Squeeze tx compute gas below 100K so the keyless overhead record overflows.
    let tx_compute_gas_limit: u64 = 50_000;

    let build_db = || {
        let mut db = MemoryDatabase::default();
        db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
        db
    };

    let mut db_rex5 = build_db();
    let result_rex5 = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db_rex5,
        keyless_tx_bytes.clone(),
        LARGE_GAS_LIMIT_OVERRIDE,
        Some(tx_compute_gas_limit),
        None,
    );

    let mut db_rex6 = build_db();
    let result_rex6 = run_keyless_outer(
        MegaSpecId::REX6,
        &mut db_rex6,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        Some(tx_compute_gas_limit),
        None,
    );

    let gas_used_rex5 = result_rex5.gas_used();
    let gas_used_rex6 = result_rex6.gas_used();
    assert!(
        gas_used_rex6 < gas_used_rex5,
        "REX6 must rescue parent gas on tx-level compute exceed: REX6={gas_used_rex6} \
         < REX5={gas_used_rex5}",
    );

    // Pin the rescue delta against a conservative lower bound. With
    // `OUTER_GAS_LIMIT = 30_000_000`, the rescued amount should be roughly the
    // outer envelope minus the intrinsic + 100K overhead. A partial-rescue
    // regression (only the overhead, or only the storage-stipend slice) would
    // produce a delta well under 25M and fail this assertion.
    let rescue_delta = gas_used_rex5 - gas_used_rex6;
    assert!(
        rescue_delta >= 25_000_000,
        "REX6 rescue delta too small: {rescue_delta} (REX5={gas_used_rex5}, REX6={gas_used_rex6})",
    );
}

// ============================================================================
// Bytecode invariant pins (EIP-3541 / EIP-170 enforcement reaches the
// sandbox dispatch path)
// ============================================================================

/// Constructors that return runtime bytecode beginning with `0xEF` must be
/// rejected by the sandbox (EIP-3541). The wire-level outcome is a
/// Halt-shaped `ExecutionHalted` failure, NOT a `Deployed` or
/// `EmptyCodeDeployed` result.
///
/// This is an invariant pin, not a fix for a finding. revm has enforced
/// EIP-3541 since Berlin, and the sandbox tx goes through revm's normal
/// CREATE handler, so the rejection happens upstream of any MegaETH-specific
/// logic. The test guards against a future REX6+ refactor that accidentally
/// bypasses revm's CREATE validation or short-circuits it in the sandbox
/// dispatch.
#[test]
fn test_rex6_keyless_constructor_returning_ef_prefix_is_rejected() {
    // Init code:
    //   PUSH1 0xef        // first byte of returned runtime bytecode
    //   PUSH1 0x00 MSTORE8
    //   PUSH1 0x01        // size = 1
    //   PUSH1 0x00        // offset = 0
    //   RETURN
    let init_code = BytecodeBuilder::default()
        .push_number(0xef_u8)
        .push_number(0_u8)
        .append(MSTORE8)
        .push_number(1_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build();
    let (keyless_tx_bytes, _signer) = build_keyless_tx_with_init_code(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let result = run_keyless_outer(
        MegaSpecId::REX6,
        &mut db,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        None,
        None,
    );
    let decoded = decode_keyless_return(&result);
    assert_eq!(
        decoded.deployedAddress,
        Address::ZERO,
        "EIP-3541 violation must not surface as a successful Deploy",
    );
    let decoded_err = decode_error_result(&decoded.errorData);
    assert!(
        matches!(decoded_err, Some(KeylessDeployError::ExecutionHalted { .. })),
        "EIP-3541 violation must surface as ExecutionHalted, not EmptyCodeDeployed or \
         a missing errorData; got {decoded_err:?}",
    );
}

/// Constructors that return runtime bytecode exceeding the configured
/// `limit_contract_code_size` (`MegaETH`'s `MAX_CONTRACT_SIZE = 512 KB` under
/// `MINI_REX`+) must be rejected by the sandbox (EIP-170-equivalent). The
/// wire-level outcome is `ExecutionHalted`, NOT `Deployed`.
///
/// This is an invariant pin. The size check is enforced inside revm's CREATE
/// handler; the test guards against a future REX6+ refactor that bypasses or
/// short-circuits revm's check from the sandbox dispatch.
///
/// The outer gas envelope is widened to 200M to comfortably accommodate the
/// `CODE_DEPOSIT_COST` charge that revm would attempt before failing the
/// size check (~200 × 512 KB ≈ 105M gas in the worst-case ordering); the
/// envelope size is irrelevant to what we're testing, only that it's large
/// enough not to OOG before the size check fires.
#[test]
fn test_rex6_keyless_constructor_returning_oversized_bytecode_is_rejected() {
    // MegaETH `MAX_CONTRACT_SIZE = 512 * 1024`; return one byte past it.
    let oversize: u64 = 512 * 1024 + 1;

    // Init code:
    //   PUSH3 oversize         // size > MAX_CONTRACT_SIZE
    //   PUSH1 0x00             // offset = 0 (memory is auto-zero)
    //   RETURN
    let init_code = BytecodeBuilder::default()
        .push_number(oversize as u32)
        .push_number(0_u8)
        .append(RETURN)
        .build();
    let (keyless_tx_bytes, _signer) = build_keyless_tx_with_init_code(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let result = run_keyless_outer(
        MegaSpecId::REX6,
        &mut db,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        None,
        Some(200_000_000),
    );
    let decoded = decode_keyless_return(&result);
    assert_eq!(
        decoded.deployedAddress,
        Address::ZERO,
        "oversized-bytecode constructor must not surface as a successful Deploy",
    );
    let decoded_err = decode_error_result(&decoded.errorData);
    assert!(
        matches!(decoded_err, Some(KeylessDeployError::ExecutionHalted { .. })),
        "oversized-bytecode constructor must surface as ExecutionHalted; got {decoded_err:?}",
    );
}
