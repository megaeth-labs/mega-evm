//! Tests for precompile error path compute gas.
//!
//! REX5 fixes compute gas accounting for failing precompile calls. When a precompile
//! fails with `PrecompileOOG` (e.g., ecrecover called with insufficient gas), the full
//! forwarded gas amount is permanently lost by the parent. REX5 records this lost gas
//! as compute gas via `gas.limit()`. Pre-REX5 specs record only `gas.spent()` which
//! is 0 on error paths because revm does not call `record_cost()` for failed precompiles.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ResultAndState, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const CONTRACT: Address = address!("0000000000000000000000000000000000600001");
const ECRECOVER: Address = address!("0000000000000000000000000000000000000001");
/// Blake2f precompile (EIP-152). Requires exactly 213 bytes of input.
/// Wrong-length input triggers `PrecompileError::Other` (not OOG).
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");

// ============================================================================
// HELPERS
// ============================================================================

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, u64) {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    (r, compute_gas)
}

// ============================================================================
// TESTS
// ============================================================================

/// A precompile CALL that fails with `PrecompileOOG` should record the full forwarded gas
/// as compute gas in REX5, but 0 in REX4.
///
/// ecrecover (address 0x01) costs 3000 gas. Forwarding only 2999 gas triggers
/// `PrecompileOOG`. The parent permanently loses the 2999 gas (`is_ok_or_revert()` is
/// false -> no refund), but pre-REX5 specs record 0 compute gas because `spent()` = 0.
/// REX5 fixes this by recording `limit()` (= forwarded gas) on error paths.
#[test]
fn test_rex5_precompile_oog_records_forwarded_gas_as_compute_gas() {
    // ecrecover requires 3000 gas. Forward only 2999 to trigger PrecompileOOG.
    let forwarded_gas: u64 = 2_999;

    // Contract: store 128 bytes of input in memory (ecrecover expects 128 bytes),
    // then CALL ecrecover with insufficient gas.
    let code = BytecodeBuilder::default()
        // Write 128 bytes of zeros at memory offset 0 (valid-length but invalid-content
        // input for ecrecover, but irrelevant because it will OOG before processing).
        .mstore(0, [0u8; 32])
        .mstore(32, [0u8; 32])
        .mstore(64, [0u8; 32])
        .mstore(96, [0u8; 32])
        // CALL ecrecover with 128-byte input but only 2999 gas
        .push_number(32_u64) // retSize
        .push_number(128_u64) // retOffset
        .push_number(128_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ECRECOVER)
        .push_number(forwarded_gas) // gas (< 3000, triggers OOG)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result_rex5, compute_gas_rex5) = transact(MegaSpecId::REX5, &mut db, tx.clone());
    assert!(
        result_rex5.result.is_success(),
        "outer tx should succeed (precompile failure is caught by CALL)"
    );

    let (result_rex4, compute_gas_rex4) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(
        result_rex4.result.is_success(),
        "outer tx should succeed (precompile failure is caught by CALL)"
    );

    // REX5 should record more compute gas because the failing precompile's forwarded gas
    // is now counted. REX4 records 0 for the failing precompile path (spent() = 0).
    assert!(
        compute_gas_rex5 > compute_gas_rex4,
        "REX5 compute gas ({compute_gas_rex5}) should be higher than REX4 ({compute_gas_rex4}) \
         due to PrecompileOOG compute gas accounting"
    );

    // The difference should be approximately the forwarded gas amount.
    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert!(
        diff >= forwarded_gas,
        "compute gas difference ({diff}) should be at least the forwarded gas ({forwarded_gas})"
    );
}

/// A precompile CALL that fails with `PrecompileError` (not OOG) should also record
/// the full forwarded gas as compute gas in REX5.
///
/// Blake2f (0x09) requires exactly 213 bytes of input. Calling it with wrong-length
/// input triggers `PrecompileError::Other`, which is distinct from `PrecompileOOG`.
/// Both error types hit the same `!is_ok_or_revert()` branch, but this test guards
/// against future regressions that might split the handling.
#[test]
fn test_rex5_precompile_error_records_forwarded_gas_as_compute_gas() {
    let forwarded_gas: u64 = 50_000;

    // Contract: CALL blake2f with 1 byte of input (wrong length → PrecompileError).
    let code = BytecodeBuilder::default()
        // Store 1 byte at memory[0]
        .mstore(0, [0xffu8; 1])
        // CALL blake2f with 1-byte input
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(1_u64) // argsSize = 1 (invalid: blake2f requires 213)
        .push_number(31_u64) // argsOffset (last byte of the 32-byte word)
        .push_number(0_u64) // value
        .push_address(BLAKE2F)
        .push_number(forwarded_gas)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result_rex5, compute_gas_rex5) = transact(MegaSpecId::REX5, &mut db, tx.clone());
    assert!(result_rex5.result.is_success(), "outer tx should succeed");

    let (result_rex4, compute_gas_rex4) = transact(MegaSpecId::REX4, &mut db, tx);
    assert!(result_rex4.result.is_success(), "outer tx should succeed");

    // Same pattern as OOG test: REX5 records forwarded gas, REX4 records 0.
    assert!(
        compute_gas_rex5 > compute_gas_rex4,
        "REX5 compute gas ({compute_gas_rex5}) should be higher than REX4 ({compute_gas_rex4}) \
         due to PrecompileError compute gas accounting"
    );

    let diff = compute_gas_rex5 - compute_gas_rex4;
    assert!(
        diff >= forwarded_gas,
        "compute gas difference ({diff}) should be at least the forwarded gas ({forwarded_gas})"
    );
}
