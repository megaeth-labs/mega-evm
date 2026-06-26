//! REX5 stipend-accounting regression suite.
//!
//! Covers two corrections sharing the `STORAGE_CALL_STIPEND` lifecycle surface.
//! REX5 subtracts only the parent-contributed portion of `call_inputs.gas_limit` from
//! parent compute gas (pre-REX5 under-counts by `CALL_STIPEND = 2,300` per value-transfer
//! CALL/CALLCODE; preserved byte-for-byte for replay). REX5 also tracks
//! `STORAGE_CALL_STIPEND` as an internal allowance drained at five `storage_gas_ext`
//! sites (CALL, CREATE, SSTORE, LOG, SELFDESTRUCT) instead of inflating `gas.limit()`;
//! pre-REX5 retains the legacy inflation byte-for-byte.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaAccessControl, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionError, ACCESS_CONTROL_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    handler::EvmTr,
    Database,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000200000");
/// A contract that performs CALL with value to RECEIVER.
const SENDER_CONTRACT: Address = address!("0000000000000000000000000000000000200001");
/// A contract that emits events / SSTOREs / SELFDESTRUCTs when receiving ETH.
const RECEIVER: Address = address!("0000000000000000000000000000000000200002");
/// An EOA-style fresh empty address used as a CALL target inside `receive()`.
const FRESH_TARGET: Address = address!("0000000000000000000000000000000000300001");
/// The 4-byte selector for `disableVolatileDataAccess()` on `MegaAccessControl`.
/// Calling this synthetic-system-contract method short-circuits `frame_init` and
/// exercises the `push_empty_frame` path in `StorageCallStipendTracker`.
const DISABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

// ============================================================================
// HELPERS
// ============================================================================

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context =
        MegaContext::new(db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Returns `(execution_result, tx_compute_gas_used)` from a successful tx.
fn transact_with_compute(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> (ExecutionResult<MegaHaltReason>, u64) {
    let mut context =
        MegaContext::new(db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(spec));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let compute = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    (result.result, compute)
}

/// Run a tx with a custom `EvmTxRuntimeLimits` override. Used to force TX-level
/// compute-gas detention inside the child for rescue-path tests.
fn transact_with_limits(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
    limits: EvmTxRuntimeLimits,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

fn setup_db(contracts: &[(Address, Bytes)]) -> MemoryDatabase {
    let mut db =
        MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_000u128));
    for (addr, bytecode) in contracts {
        db = db.account_code(*addr, bytecode.clone());
    }
    db.set_account_balance(SENDER_CONTRACT, U256::from(1_000_000_000u128));
    db
}

fn default_tx() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(SENDER_CONTRACT).gas_limit(100_000_000).build_fill()
}

fn inner_call_success_flag(output: &[u8]) -> U256 {
    U256::from_be_slice(&output[..32])
}

// ============================================================================
// CONTRACT BUILDERS
// ============================================================================

/// `CALL(gas=0, RECEIVER, value=1, ...)` then RETURN the 32-byte success flag.
/// Forwarded gas is 0; the child relies entirely on the stipend.
fn build_zero_gas_transfer_contract(to: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value
        .push_address(to)
        .push_number(0_u64) // gas = 0
        .append(CALL)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// `CALL(gas=forwarded_gas, RECEIVER, value=1, ...)` then RETURN the success flag.
fn build_value_transfer_with_gas(to: Address, forwarded_gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value
        .push_address(to)
        .push_number(forwarded_gas)
        .append(CALL)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// `CALLCODE(gas=0, RECEIVER, value=1, ...)` — mirrors the zero-gas CALL variant.
fn build_zero_gas_callcode_contract(to: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(1_u64)
        .push_address(to)
        .push_number(0_u64)
        .append(CALLCODE)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// `DELEGATECALL(gas=forwarded_gas, to, ...)` — no value transfer, no stipend.
fn build_delegatecall_contract(to: Address, forwarded_gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(to)
        .push_number(forwarded_gas)
        .append(DELEGATECALL)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// `STATICCALL(gas=forwarded_gas, to, ...)` — no value transfer, no stipend.
fn build_staticcall_contract(to: Address, forwarded_gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(to)
        .push_number(forwarded_gas)
        .append(STATICCALL)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// Receiver that emits LOG1 with 0 bytes and STOPs. Standard EVM cost ~750.
fn build_log1_receiver() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0xdeadbeef_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(LOG1)
        .append(STOP)
        .build()
}

/// Receiver that SSTOREs slot 0 = 1 (first-time-write zero-to-nonzero) and STOPs.
/// Standard EIP-2200 cost ≈ 22,100.
fn build_sstore_receiver() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u64) // value
        .push_number(0_u64) // slot
        .append(SSTORE)
        .append(STOP)
        .build()
}

/// Receiver that CALLs a fresh empty address with value=1.
/// Triggers the CALL/CALLCODE new-account surcharge inside the stipend-receiving frame.
fn build_nested_value_call_receiver(target: Address, forwarded_gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value
        .push_address(target)
        .push_number(forwarded_gas)
        .append(CALL)
        .append(STOP)
        .build()
}

/// Receiver that SELFDESTRUCTs to a fresh empty beneficiary. Standard EVM cost
/// for SELFDESTRUCT to a cold, empty, value-receiving account ≈ 32,500.
fn build_selfdestruct_receiver(beneficiary: Address) -> Bytes {
    BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build()
}

/// Receiver that executes `CREATE2(value=0, offset=0, size=2, salt=0)` over
/// in-memory initcode `PUSH1 0x00 STOP` (just two bytes: 0x60 0x00 -- actually
/// the bytecode here writes `STOP STOP` into memory, immediately stops on
/// invocation). Returns the 32-byte `created_address` from CREATE2.
fn build_create2_receiver() -> Bytes {
    BytecodeBuilder::default()
        // Write a single STOP byte at memory[0]. STOP = 0x00, push as a 32-byte word.
        .push_number(0_u64)
        .push_number(0_u64)
        .append(MSTORE)
        // CREATE2(value, offset, size, salt)
        .push_number(0_u64) // salt
        .push_number(1_u64) // size = 1 byte initcode (STOP)
        .push_number(31_u64) // offset (so the STOP byte from MSTORE is at memory[31])
        .push_number(0_u64) // value
        .append(CREATE2)
        .append(STOP)
        .build()
}

/// Appends ~`target_gas` worth of compute burn via repeated PUSH0/POP pairs.
fn append_burn_gas(mut builder: BytecodeBuilder, target_gas: u64) -> BytecodeBuilder {
    let iterations = target_gas / 5;
    for _ in 0..iterations {
        builder = builder.push_number(0_u8).append(POP);
    }
    builder
}

/// Receiver that accesses TIMESTAMP (triggering volatile-data detention), then burns
/// ~`burn_target` compute gas. Used by the TX-level rescue test to force a TX-level
/// `VolatileDataAccessOutOfGas` halt inside a stipend-receiving child.
fn build_timestamp_detention_receiver(burn_target: u64) -> Bytes {
    let builder = BytecodeBuilder::default().append(TIMESTAMP).append(POP);
    append_burn_gas(builder, burn_target).append(STOP).build()
}

/// Receiver that does a first-time-write SSTORE (drains stipend allowance + charges
/// the EVM residual + standard SSTORE EVM gas) and then explicitly REVERTs. Used by
/// the consume-then-revert test to pin that the drained allowance does not leak
/// gas back to the caller across a revert.
fn build_sstore_then_revert_receiver() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u64) // value
        .push_number(0_u64) // slot
        .append(SSTORE)
        // REVERT(offset=0, size=0)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(REVERT)
        .build()
}

/// `CALL(gas=0, ACCESS_CONTROL_ADDRESS, value=1, selector=disable...)`. Sends 1 wei
/// to the system contract. The interceptor short-circuits `frame_init`, exercising
/// the `push_empty_frame` path in `StorageCallStipendTracker`.
fn build_intercepted_value_call() -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(0_u64) // gas = 0
        .append(CALL)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// Receiver that does `KECCAK256(offset=0, len=32_768)` — 1,024 words of hash
/// cost (6 × 1024 = 6,144) plus memory expansion for 32 KiB
/// (`3 × 1024 + 1024² / 512 = 3,072 + 2,048 = 5,120`) plus base 30 — total ≈
/// 11,294 gas. Comfortably inside (2,300, 25,300) — proves REX5 prevents the
/// leak by OOG'ing inside KECCAK256 while REX4 absorbs the full cost into
/// parent compute via the legacy inflation path.
fn build_keccak256_leaky_receiver() -> Bytes {
    BytecodeBuilder::default()
        .push_number(32_768_u64)
        .push_number(0_u64)
        .append(KECCAK256)
        .append(POP)
        .append(STOP)
        .build()
}

// ============================================================================
// Parent compute-gas attribution: +CALL_STIPEND delta on value-transfer CALL
// ============================================================================

/// REX5 records `CALL_STIPEND` (2,300) of additional parent compute-gas per
/// value-transferring `CALL`. REX4 under-counts by exactly that amount.
#[test]
fn test_call_value_transfer_parent_compute_gas_delta_rex5_vs_rex4() {
    let sender_code = build_zero_gas_transfer_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex4_db = setup_db(&contracts);
    let (rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    let mut rex5_db = setup_db(&contracts);
    let (rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut rex5_db, default_tx());

    assert!(matches!(rex4_result, ExecutionResult::Success { .. }));
    assert!(matches!(rex5_result, ExecutionResult::Success { .. }));
    assert_eq!(
        rex5_compute,
        rex4_compute + 2_300,
        "REX5 must record exactly +CALL_STIPEND (2,300) more parent compute-gas \
         than REX4 for a single value-transferring CALL"
    );
}

/// Same invariant for `CALLCODE`.
#[test]
fn test_callcode_value_transfer_parent_compute_gas_delta_rex5_vs_rex4() {
    let sender_code = build_zero_gas_callcode_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex4_db = setup_db(&contracts);
    let (rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    let mut rex5_db = setup_db(&contracts);
    let (rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut rex5_db, default_tx());

    assert!(matches!(rex4_result, ExecutionResult::Success { .. }));
    assert!(matches!(rex5_result, ExecutionResult::Success { .. }));
    assert_eq!(
        rex5_compute,
        rex4_compute + 2_300,
        "REX5 must record exactly +CALL_STIPEND (2,300) more parent compute-gas \
         than REX4 for a single value-transferring CALLCODE"
    );
}

/// DELEGATECALL receives no `CALL_STIPEND` from revm, so the fix must not
/// adjust its parent compute-gas attribution. REX4 and REX5 must record
/// identical parent compute-gas for a DELEGATECALL.
#[test]
fn test_delegatecall_parent_compute_gas_parity_across_specs() {
    let sender_code = build_delegatecall_contract(RECEIVER, 100_000);
    let receiver_code = build_log1_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex4_db = setup_db(&contracts);
    let (rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    let mut rex5_db = setup_db(&contracts);
    let (rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut rex5_db, default_tx());

    assert!(matches!(rex4_result, ExecutionResult::Success { .. }));
    assert!(matches!(rex5_result, ExecutionResult::Success { .. }));
    assert_eq!(
        rex5_compute, rex4_compute,
        "DELEGATECALL has no CALL_STIPEND; REX4 and REX5 must record identical \
         parent compute-gas"
    );
}

/// STATICCALL receives no `CALL_STIPEND` from revm (no value transfer
/// semantics). Same parity guard as DELEGATECALL.
#[test]
fn test_staticcall_parent_compute_gas_parity_across_specs() {
    let sender_code = build_staticcall_contract(RECEIVER, 100_000);
    let receiver_code = build_log1_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex4_db = setup_db(&contracts);
    let (rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    let mut rex5_db = setup_db(&contracts);
    let (rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut rex5_db, default_tx());

    assert!(matches!(rex4_result, ExecutionResult::Success { .. }));
    assert!(matches!(rex5_result, ExecutionResult::Success { .. }));
    assert_eq!(
        rex5_compute, rex4_compute,
        "STATICCALL has no CALL_STIPEND; REX4 and REX5 must record identical \
         parent compute-gas"
    );
}

// ============================================================================
// Storage allowance coverage per charging site
// ============================================================================

/// LOG1 in `receive()` at `gas = 0` succeeds on REX5: the standard LOG1 EVM cost
/// (~750) fits inside `CALL_STIPEND = 2,300`, and the Mega LOG storage surcharge
/// drains from the allowance.
#[test]
fn test_log1_value_transfer_succeeds_at_zero_gas_rex5() {
    let sender_code = build_zero_gas_transfer_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::from(1),
                "REX5: inner LOG1 CALL with gas=0 must succeed via separated allowance"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// First-time-write SSTORE inside `receive()` succeeds on REX5 when forwarded gas
/// covers the standard EIP-2200 cost. The 20,000 Mega `sstore_set_storage_gas`
/// surcharge drains from the 23,000 allowance.
#[test]
fn test_sstore_in_receive_succeeds_at_minimum_envelope_rex5() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 22_500);
    let receiver_code = build_sstore_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::from(1),
                "REX5: first-time SSTORE in receive() must succeed when forwarded gas \
                 covers standard EIP-2200 cost; Mega surcharge drains from allowance"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// REX4 parity for the SSTORE-in-receive fixture: same forwarded amount must
/// succeed via legacy `gas.limit()` inflation.
#[test]
fn test_sstore_in_receive_succeeds_at_minimum_envelope_rex4_parity() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 22_500);
    let receiver_code = build_sstore_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::from(1),
                "REX4: same fixture must succeed via legacy inflation"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// SELFDESTRUCT to an empty beneficiary inside `receive()` succeeds on REX5 when
/// forwarded gas covers the standard EVM cost plus the residual portion of the
/// Mega new-account surcharge (25,000 − 23,000 = 2,000 at multiplier=2). The
/// allowance covers 23,000 of the surcharge; the residual comes from forwarded gas.
#[test]
fn test_selfdestruct_in_receive_succeeds_rex5() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 50_000);
    let receiver_code = build_selfdestruct_receiver(FRESH_TARGET);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::from(1),
                "REX5: SELFDESTRUCT to empty beneficiary must succeed; allowance \
                 drains the Mega surcharge"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// REX4 parity for the SELFDESTRUCT-in-receive fixture.
#[test]
fn test_selfdestruct_in_receive_succeeds_rex4_parity() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 50_000);
    let receiver_code = build_selfdestruct_receiver(FRESH_TARGET);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(inner_call_success_flag(output.data()), U256::from(1), "REX4 parity guard");
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// Nested value-transfer CALL B → C where C is empty exercises the
/// `wrap_call_with_storage_gas!` drain site. B's allowance covers 23,000 of the
/// 25,000 new-account surcharge; C receives its own fresh allowance because B → C
/// is itself a value-transfer CALL. Forwarded gas covers the residual + standard
/// CALL EVM costs.
#[test]
fn test_nested_call_to_empty_account_succeeds_rex5() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 200_000);
    let receiver_code = build_nested_value_call_receiver(FRESH_TARGET, 50_000);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::from(1),
                "REX5: nested value-transfer CALL to fresh empty target must succeed; \
                 each frame gets its own allowance"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// REX4 parity for the nested CALL-to-empty fixture.
#[test]
fn test_nested_call_to_empty_account_succeeds_rex4_parity() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 200_000);
    let receiver_code = build_nested_value_call_receiver(FRESH_TARGET, 50_000);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(inner_call_success_flag(output.data()), U256::from(1), "REX4 parity guard");
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

// ============================================================================
// Compute-leak isolation: stipend cannot fund compute-only opcodes
// ============================================================================

/// REX5 must not let `STORAGE_CALL_STIPEND` be spent on a compute-only opcode
/// inside the (2,300, 25,300) envelope. Fixture: child receives 1 wei with gas=0
/// then runs `KECCAK256(offset=0, len=32_768)` (~11,294 EVM gas — above
/// `CALL_STIPEND` and below `CALL_STIPEND + STORAGE_CALL_STIPEND`). REX4's legacy
/// inflation lets KECCAK256 run and records the full cost into parent compute
/// before the per-frame cap revert fires; REX5's un-inflated child OOGs inside
/// revm's standard `gas!` before any compute attribution lands on the parent.
/// REX4 parent compute-gas therefore exceeds REX5's by roughly the KECCAK256
/// opcode cost (minus the +2,300 parent-attribution correction).
#[test]
fn test_storage_stipend_cannot_cover_compute_only_opcode_rex5() {
    let sender_code = build_zero_gas_transfer_contract(RECEIVER);
    let receiver_code = build_keccak256_leaky_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex4_db = setup_db(&contracts);
    let (_rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    let mut rex5_db = setup_db(&contracts);
    let (_rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut rex5_db, default_tx());

    // REX4 path absorbs the KECCAK256 cost into parent compute via the legacy leak.
    // REX5 path stops the KECCAK256 before any compute is recorded to parent.
    // Even after REX5's +2,300 CALL_STIPEND parent-attribution correction (§1),
    // REX4 must still be substantially larger for this fixture.
    assert!(
        rex4_compute > rex5_compute + 5_000,
        "REX4 must record substantially more parent compute-gas than REX5 because \
         the legacy inflation lets KECCAK256 leak through. REX4={rex4_compute}, REX5={rex5_compute}"
    );
}

/// REX5 does not refund unused allowance to the caller: the allowance never
/// enters `gas.limit()`, so the LOG1-in-receive fixture's REX5 vs REX4 parent
/// compute-gas delta equals exactly `+CALL_STIPEND` (the §1 correction). A
/// refund leak would shift this delta downward.
#[test]
fn test_storage_stipend_unused_portion_does_not_inflate_caller_gas_rex5() {
    let sender_code = build_zero_gas_transfer_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex4_db = setup_db(&contracts);
    let (_rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    let mut rex5_db = setup_db(&contracts);
    let (_rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut rex5_db, default_tx());

    // REX5 = REX4 + 2,300 (§1) — and the +2,300 also implicitly proves the unused
    // stipend was not refunded back (refunded gas would shrink parent's recorded
    // CALL cost via revm's gas accounting).
    assert_eq!(
        rex5_compute,
        rex4_compute + 2_300,
        "Parent compute-gas delta must be exactly +CALL_STIPEND; any unused-stipend \
         refund leaking back would shift this delta"
    );
}

/// CREATE2 inside a value-transferring child's `receive()` exercises the
/// `storage_gas_ext::create` drain site. With generous forwarded gas, both specs
/// succeed: REX5 drains 23,000 of the 32,000 `create_contract_storage_gas`
/// (multiplier=2) from the allowance; REX4 covers it via legacy inflation.
#[test]
fn test_create2_in_receive_succeeds_rex5() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 200_000);
    let receiver_code = build_create2_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::from(1),
                "REX5: CREATE2 inside stipend-receiving frame must succeed"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// REX4 parity for CREATE2-in-receive.
#[test]
fn test_create2_in_receive_succeeds_rex4_parity() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 200_000);
    let receiver_code = build_create2_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(inner_call_success_flag(output.data()), U256::from(1), "REX4 parity guard");
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

// ============================================================================
// Negative controls
// ============================================================================

/// Stipend (REX5 allowance or REX4 inflated gas) bridges at most its 23,000
/// portion. A child forwarded gas below the standard EVM opcode cost must fail
/// the inner CALL on both specs. Fixture: SSTORE receiver with 5,000 forwarded
/// gas (well below the 22,100 EIP-2200 cost).
#[test]
fn test_sstore_underforward_fails_on_both_specs() {
    // Forward only 5,000 EVM gas — far below the 22,100 standard SSTORE cost.
    // Effective gas = 5,000 + CALL_STIPEND = 7,300, still well below the
    // standard SSTORE cost — child must OOG on both specs regardless of stipend.
    let sender_code = build_value_transfer_with_gas(RECEIVER, 5_000);
    let receiver_code = build_sstore_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut rex5_db = setup_db(&contracts);
    let rex5_result = transact(MegaSpecId::REX5, &mut rex5_db, default_tx()).unwrap();
    let mut rex4_db = setup_db(&contracts);
    let rex4_result = transact(MegaSpecId::REX4, &mut rex4_db, default_tx()).unwrap();

    // Both specs return successfully at the OUTER tx level (the parent's CALL
    // returns failure but the parent contract itself returns the flag = 0).
    // What we assert: the INNER CALL flag is 0 on both specs.
    match &rex5_result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::ZERO,
                "REX5: under-forwarded SSTORE child must fail"
            );
        }
        other => panic!("REX5: expected outer Success, got {other:?}"),
    }
    match &rex4_result.result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::ZERO,
                "REX4: under-forwarded SSTORE child must fail — stipend can't bridge \
                 beyond its 23,000 size"
            );
        }
        other => panic!("REX4: expected outer Success, got {other:?}"),
    }
}

// ============================================================================
// Synthetic interceptor frame stack alignment
// ============================================================================

/// Value-transferring CALL to a system contract is short-circuited by
/// `frame_init` interception, which uses `push_empty_frame` to keep tracker
/// stacks aligned. The stipend tracker must push a zero-allowance entry on the
/// interception path so subsequent pops match — passes if no panic fires in the
/// per-frame stack-non-empty assertion on either spec.
#[test]
fn test_intercepted_value_call_stack_alignment_rex5() {
    let sender_code = build_intercepted_value_call();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code)]);

    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).unwrap();
    // The interceptor may succeed or revert depending on the system contract
    // state; what matters is that no panic occurred and the tx completed.
    assert!(
        matches!(result.result, ExecutionResult::Success { .. } | ExecutionResult::Revert { .. }),
        "REX5: intercepted value CALL must not panic stack alignment"
    );
}

/// REX4 parity for the synthetic interceptor stack alignment.
#[test]
fn test_intercepted_value_call_stack_alignment_rex4_parity() {
    let sender_code = build_intercepted_value_call();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    assert!(
        matches!(result.result, ExecutionResult::Success { .. } | ExecutionResult::Revert { .. }),
        "REX4 parity guard"
    );
}

/// The stipend is granted only to internal (depth > 0) value-transferring
/// CALL/CALLCODE. A top-level value-transfer transaction must NOT receive
/// the stipend on either spec; LOG1 in the receiver should fail under both
/// REX4 and REX5 when the tx `gas_limit` is too tight (no stipend to cover the
/// 4,500-gas Mega LOG cost).
#[test]
fn test_top_level_value_transfer_does_not_get_stipend_rex5() {
    let receiver_code = build_log1_receiver();
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(RECEIVER)
        .gas_limit(60_000)
        .value(U256::from(1_u64))
        .build_fill();

    let mut rex5_db = setup_db(&[(RECEIVER, receiver_code)]);
    let rex5_result = transact(MegaSpecId::REX5, &mut rex5_db, tx).unwrap();
    assert!(
        matches!(rex5_result.result, ExecutionResult::Halt { .. }),
        "REX5 top-level value transfer must NOT receive STORAGE_CALL_STIPEND; \
         the tight tx gas_limit must cause a Halt"
    );
}

// ============================================================================
// TX-level rescue + revert path inside a stipend-receiving child
// ============================================================================

/// Value-transferring child enters with a stipend allowance, accesses TIMESTAMP
/// to trigger volatile-data detention, then burns compute past the detained
/// `tx_compute_gas_limit`. REX5's `rescue_gas` returns `gas.remaining()` directly
/// (the allowance never bled into `gas.limit()`), so the rescue path produces a
/// bounded refund without panic; the halt is `VolatileDataAccessOutOfGas` and
/// `gas_used` reflects the rescued gas (far below the forwarded child-frame gas).
#[test]
fn test_stipend_receiving_child_tx_level_rescue_rex5() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 100_000);
    let receiver_code = build_timestamp_detention_receiver(20_000);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let limits = EvmTxRuntimeLimits::no_limits()
        .with_tx_compute_gas_limit(1_000_000)
        .with_block_env_access_compute_gas_limit(10_000);
    let result = transact_with_limits(MegaSpecId::REX5, &mut db, default_tx(), limits).unwrap();

    match &result.result {
        ExecutionResult::Halt {
            reason: MegaHaltReason::VolatileDataAccessOutOfGas { limit, .. },
            gas_used,
        } => {
            assert!(
                *limit <= 1_000_000,
                "detained limit should stay within the tx compute gas limit, got {limit}"
            );
            assert!(
                *gas_used < 200_000,
                "REX5 rescue_gas must refund excess gas — gas_used {gas_used} indicates no \
                 refund occurred. Allowance must NOT inflate the rescued amount; it never \
                 entered gas.limit() on REX5"
            );
        }
        other => panic!("expected VolatileDataAccessOutOfGas, got {other:?}"),
    }
}

/// Stipend-receiving child drains its allowance via SSTORE then REVERTs. The
/// drained allowance is consumed — it does not refund across the revert. Outer
/// tx succeeds, inner CALL flag is 0, SSTORE state is rolled back, and the
/// parent compute-gas delta `REX5 = REX4 + 2,300` is unchanged by the revert.
#[test]
fn test_stipend_drained_then_revert_no_refund_rex5() {
    let sender_code = build_value_transfer_with_gas(RECEIVER, 50_000);
    let receiver_code = build_sstore_then_revert_receiver();
    let contracts = vec![(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)];

    let mut db = setup_db(&contracts);
    let (rex5_result, rex5_compute) =
        transact_with_compute(MegaSpecId::REX5, &mut db, default_tx());

    match &rex5_result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::ZERO,
                "REX5: inner CALL must return success=0 because child REVERTed"
            );
        }
        other => panic!("expected outer Success, got {other:?}"),
    }

    // The receiver's slot 0 must remain zero — SSTORE was rolled back by the revert.
    let slot_zero = db.storage(RECEIVER, U256::from(0)).unwrap();
    assert_eq!(slot_zero, U256::ZERO, "REX5: revert must roll back the SSTORE side-effect");

    // Parity guard: REX4 fixture must also produce inner-call success=0 and zero slot.
    let mut rex4_db = setup_db(&contracts);
    let (rex4_result, rex4_compute) =
        transact_with_compute(MegaSpecId::REX4, &mut rex4_db, default_tx());
    match &rex4_result {
        ExecutionResult::Success { output, .. } => {
            assert_eq!(
                inner_call_success_flag(output.data()),
                U256::ZERO,
                "REX4 parity guard: inner CALL must return success=0"
            );
        }
        other => panic!("expected REX4 outer Success, got {other:?}"),
    }
    let rex4_slot_zero = rex4_db.storage(RECEIVER, U256::from(0)).unwrap();
    assert_eq!(rex4_slot_zero, U256::ZERO, "REX4 parity guard");

    // REX5 records exactly +CALL_STIPEND (2,300) more parent compute-gas than REX4
    // (the §1 fix). The child's revert path doesn't perturb this delta, proving the
    // drained allowance and the revert interact correctly with the §1 attribution.
    assert_eq!(
        rex5_compute,
        rex4_compute + 2_300,
        "REX5 parent compute-gas must be exactly REX4 + CALL_STIPEND even when the \
         stipend-receiving child REVERTs after draining allowance. Any deviation \
         indicates a refund leak through the revert path"
    );
}
