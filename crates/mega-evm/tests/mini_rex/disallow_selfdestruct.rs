//! Tests for the disabled SELFDESTRUCT opcode in the Mini-Rex spec.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    op_revm::OpTransaction,
    revm::{
        bytecode::opcode::{DUPN, PUSH0, SELFDESTRUCT},
        context::{
            result::{ExecutionResult, ResultAndState},
            tx::TxEnvBuilder,
            TxEnv,
        },
    },
    test_utils::{transact, MemoryDatabase},
    MegaTransaction, *,
};

/// Specs that disable `SELFDESTRUCT`, dispatching it to `control::invalid`.
const SELFDESTRUCT_DISABLED_SPECS: [MegaSpecId; 3] =
    [MegaSpecId::MINI_REX, MegaSpecId::REX, MegaSpecId::REX1];

/// What a plain call transaction pays before its first opcode runs: the 21,000 base intrinsic,
/// plus the flat intrinsic storage gas Rex adds to every transaction.
fn intrinsic_gas(spec: MegaSpecId) -> u64 {
    let base = 21_000;
    if spec.is_enabled(MegaSpecId::REX) {
        base + mega_evm::constants::rex::TX_INTRINSIC_STORAGE_GAS
    } else {
        base
    }
}

/// Runs `code` under `spec` with a chosen transaction gas limit, so a test can put the frame at a
/// specific remaining balance when it reaches the opcode under test.
fn transact_with_gas_limit(
    spec: MegaSpecId,
    code: Vec<u8>,
    gas_limit: u64,
) -> ResultAndState<MegaHaltReason> {
    const CALLER: Address = address!("0000000000000000000000000000000000100000");
    const CONTRACT: Address = address!("0000000000000000000000000000000000100001");

    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, code.into());
    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx: TxEnv =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(gas_limit).build_fill();
    let mut tx = MegaTransaction(OpTransaction::new(tx));
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not error")
}

/// Test that verifies the SELFDESTRUCT opcode works normally before the Mini-Rex hardfork.
/// This test ensures that contracts using the SELFDESTRUCT opcode execute successfully
/// and consume the expected amount of gas when using the `EQUIVALENCE` spec.
#[test]
fn test_selfdestruct_allowed_before_mini_rex() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::EQUIVALENCE, &mut db, caller, callee, Bytes::default(), U256::ZERO);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().result.tx_gas_used(), 26004);
}

/// Test that verifies the SELFDESTRUCT opcode is disabled and returns `InvalidFEOpcode` after
/// the Mini-Rex hardfork. This test ensures that contracts attempting to use the SELFDESTRUCT
/// opcode are properly rejected with the correct halt reason when using the `MINI_REX` spec.
#[test]
fn test_selfdestruct_disallowed_after_mini_rex() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO);
    assert!(matches!(
        result,
        Ok(ResultAndState {
            result: ExecutionResult::Halt {
                reason: MegaHaltReason::Base(OpHaltReason::Base(EthHaltReason::InvalidFEOpcode)),
                ..
            },
            ..
        })
    ));
}

/// A rejected `SELFDESTRUCT` costs the frame everything it had, whether or not the frame could
/// afford the opcode's static gas.
///
/// revm 40 charges an opcode's static gas before dispatching to its handler, so an underfunded
/// frame is halted out of gas without ever reaching `control::invalid`, and reports `OutOfGas`
/// where revm 27 reported `InvalidFEOpcode`. Which halt is reported is not a `MegaETH` surface — it
/// reaches traces and tooling, never a receipt. What is consensus-visible is pinned here: the
/// transaction fails and consumes its entire gas limit either way, so no receipt, block total, or
/// replay moves with the reported reason.
#[test]
fn test_disallowed_selfdestruct_consumes_the_whole_budget_at_any_funding() {
    for spec in SELFDESTRUCT_DISABLED_SPECS {
        for gas_above_intrinsic in [3_000, 100_000] {
            let gas_limit = intrinsic_gas(spec) + gas_above_intrinsic;
            let result = transact_with_gas_limit(spec, vec![PUSH0, SELFDESTRUCT], gas_limit);

            assert!(
                matches!(result.result, ExecutionResult::Halt { .. }),
                "{spec} at {gas_limit}: a disabled SELFDESTRUCT must halt, got {:?}",
                result.result,
            );
            assert_eq!(
                result.result.tx_gas_used(),
                gas_limit,
                "{spec} at {gas_limit}: the halt must consume the whole gas limit",
            );
            assert!(
                result.result.logs().is_empty(),
                "{spec} at {gas_limit}: a halted transaction emits no logs",
            );
        }
    }
}

/// The same consensus surface for an opcode no `MegaETH` instruction table ever wired, which is
/// dispatched to `control::unknown`. `DUPN` stands in for the group (`SLOTNUM`, `DUPN`, `SWAPN`,
/// `EXCHANGE`); the baseline prices them at 2-3 gas, so the underfunded window is only reachable
/// with a transaction that has almost nothing left after its intrinsic cost.
#[test]
fn test_unwired_opcode_consumes_the_whole_budget_at_any_funding() {
    for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX6, MegaSpecId::REX7] {
        for gas_above_intrinsic in [1, 100_000] {
            let gas_limit = intrinsic_gas(spec) + gas_above_intrinsic;
            let result = transact_with_gas_limit(spec, vec![DUPN, 0x00], gas_limit);

            assert!(
                matches!(result.result, ExecutionResult::Halt { .. }),
                "{spec} at {gas_limit}: an unwired opcode must halt, got {:?}",
                result.result,
            );
            assert_eq!(
                result.result.tx_gas_used(),
                gas_limit,
                "{spec} at {gas_limit}: the halt must consume the whole gas limit",
            );
        }
    }
}
