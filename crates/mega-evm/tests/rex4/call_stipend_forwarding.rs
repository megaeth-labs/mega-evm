//! Pins the `check_call_has_transfer` predicate used by the 98/100 gas-forwarding
//! wrappers for CALL / CALLCODE.
//!
//! When a value-transferring CALL is re-capped to 98/100 of parent remaining, the
//! revm-side `CALL_STIPEND` (`2_300`) must be excluded from the forwardable amount and
//! re-added after the cap. Mutating `check_call_has_transfer` to always return
//! `false` subjects the stipend to the 98% cut and under-funds the child by
//! `floor(0.02 * CALL_STIPEND)` = 46 gas when the cap binds.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::OpTxError,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const SENDER: Address = address!("0000000000000000000000000000000000200001");
const RECEIVER: Address = address!("0000000000000000000000000000000000200002");

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> {
    let mut context =
        MegaContext::new(db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Child returns its gas remaining at entry (GAS → MSTORE → RETURN 32 bytes).
fn build_gas_reporter() -> Bytes {
    BytecodeBuilder::default()
        .append(GAS)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// Parent performs CALL(to, value=1) forwarding all available gas via GAS, then
/// returns the child's returndata (the child's entry gas).
fn build_value_call_all_gas_and_return_returndata(to: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(32_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(to)
        .append(GAS) // forward all remaining gas — 98/100 will bind
        .append(CALL)
        .append(POP)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// Value-transferring CALL must protect `CALL_STIPEND` from the 98/100 cut.
///
/// Spec is REX3 (post-MINI_REX 98/100, pre-REX4 `STORAGE_CALL_STIPEND`). The parent
/// forwards all remaining gas so the 98/100 rule binds. Correct child entry gas is
/// pinned absolutely for this fixture; the `check_call_has_transfer → false` mutant
/// under-funds the child by 46 gas (`floor(0.02 * 2300)`).
#[test]
fn test_value_call_preserves_call_stipend_under_98_100_cap() {
    // Absolute pin for this fixture (tx gas_limit=100_000_000, REX3, value=1, GAS-forward).
    // Derived from the correct implementation; the has_transfer=false mutant yields 97_932_062.
    const EXPECTED_CHILD_ENTRY_GAS: u64 = 97_932_108;

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000_000_000u128))
        .account_balance(SENDER, U256::from(10_000_000_000u128))
        .account_code(SENDER, build_value_call_all_gas_and_return_returndata(RECEIVER))
        .account_code(RECEIVER, build_gas_reporter());

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(SENDER).gas_limit(100_000_000).build_fill();
    let result = transact(MegaSpecId::REX3, &mut db, tx).expect("tx must not error");
    let child_gas = match result.result {
        ExecutionResult::Success { output, .. } => {
            let data = output.into_data();
            assert_eq!(data.len(), 32, "child must return a 32-byte gas word");
            U256::from_be_slice(&data).to::<u64>()
        }
        other => panic!("expected Success, got {other:?}"),
    };

    assert_eq!(
        child_gas, EXPECTED_CHILD_ENTRY_GAS,
        "value CALL under a binding 98/100 cap must protect CALL_STIPEND from the cut;          got {child_gas} (has_transfer=false mutant under-funds by ~46 gas)",
    );
}
