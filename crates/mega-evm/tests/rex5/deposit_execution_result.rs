//! The sandbox-only bypass in `MegaHandler::execution_result` must stay sandbox-only.
//!
//! Inside a keyless-deploy sandbox, a deposit-marked transaction bypasses op-revm's
//! `HaltedDepositPostRegolith` conversion so the outer flow can see the real halt reason and gas.
//! An ordinary (non-sandbox) deposit transaction must keep op-revm's conversion: a runtime halt is
//! reported as `FailedDeposit` with the whole gas limit consumed. Widening the bypass to every
//! deposit transaction would change that consensus-visible shape.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::{OpTx, OpTxError},
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EVMError, EmptyExternalEnv, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
};
use revm::{
    bytecode::opcode::INVALID,
    context::{result::ResultAndState, BlockEnv, TxEnv},
    primitives::TxKind,
};

const DEPOSIT_CALLER: Address = address!("0000000000000000000000000000000000500400");
/// Contract whose body is a single `INVALID`, so the frame halts at runtime.
const HALTING_CONTRACT: Address = address!("0000000000000000000000000000000000500401");
const GAS_LIMIT: u64 = 5_000_000;

/// A non-sandbox OP deposit transaction: a non-zero `source_hash` flips `tx_type` to
/// `DEPOSIT_TRANSACTION_TYPE`.
fn deposit_tx() -> mega_evm::MegaTransaction {
    let mut tx = OpTx(OpTransaction {
        base: TxEnv {
            caller: DEPOSIT_CALLER,
            kind: TxKind::Call(HALTING_CONTRACT),
            gas_limit: GAS_LIMIT,
            gas_price: 0,
            ..Default::default()
        },
        ..Default::default()
    });
    tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    tx.deposit.mint = Some(1_000_000);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// A deposit transaction that halts outside a sandbox must surface op-revm's post-Regolith
/// `FailedDeposit` shape, not the raw halt reason the sandbox path exposes.
#[test]
fn test_rex5_non_sandbox_deposit_halt_stays_failed_deposit() {
    let db = MemoryDatabase::default()
        .account_code(HALTING_CONTRACT, BytecodeBuilder::default().append(INVALID).build());

    let mut context = MegaContext::new(db, MegaSpecId::REX5)
        .with_block(BlockEnv { gas_limit: 100_000_000, ..Default::default() });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm: MegaEvm<_, revm::inspector::NoOpInspector, EmptyExternalEnv> =
        MegaEvm::new(context);

    let result: Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> =
        alloy_evm::Evm::transact_raw(&mut evm, deposit_tx());
    let result = result.expect("op-revm converts a halted deposit into an Ok result").result;

    let rendered = format!("{result:?}");
    assert!(
        rendered.contains("FailedDeposit"),
        "a non-sandbox deposit halt must be reported as FailedDeposit, got {rendered}",
    );
    assert_eq!(
        result.tx_gas_used(),
        GAS_LIMIT,
        "FailedDeposit consumes the whole gas limit; the sandbox bypass reports the real \
         gas used instead",
    );
}
