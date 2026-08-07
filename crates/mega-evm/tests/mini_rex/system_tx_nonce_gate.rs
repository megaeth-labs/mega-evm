//! Pre-REX5 side of the system-transaction validation gate in `MegaHandler::before_run`.
//!
//! REX5 restored the nonce / EIP-3607 / chain-id checks for a legacy transaction sent by the
//! runtime system address before it is promoted onto the deposit path. Every spec before REX5
//! promotes without any of those checks, and that is the frozen replay behavior: a system
//! transaction whose `nonce` does not match the system account's state nonce still executes.
//!
//! `tests/rex5/system_tx_replay.rs` pins the REX5 rejections; this module pins the stable-spec
//! acceptance the gate must keep.

use alloy_primitives::{Bytes, U256};
use mega_evm::{
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MEGA_SYSTEM_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    context::{BlockEnv, TxEnv},
    inspector::NoOpInspector,
    primitives::TxKind,
};

/// The system account's on-chain nonce; the transaction below deliberately carries a different one.
const STATE_NONCE: u64 = 7;

fn build_evm(
    db: MemoryDatabase,
    spec: MegaSpecId,
) -> MegaEvm<MemoryDatabase, NoOpInspector, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, spec).with_block(BlockEnv {
        number: U256::from(10),
        gas_limit: 100_000_000,
        basefee: 0,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    MegaEvm::new(context)
}

/// A legacy system transaction with a stale nonce, targeting the whitelisted oracle contract.
fn stale_nonce_system_tx() -> mega_evm::MegaTransaction {
    let mut tx = MegaTransaction(OpTransaction::new(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        gas_limit: 1_000_000,
        gas_price: 0,
        nonce: 0,
        ..Default::default()
    }));
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Pre-REX5 specs promote the system transaction without validating its nonce against state.
/// Running the same transaction under REX5 is a `NonceTooLow` rejection, so this is the exact
/// boundary the REX5 gate draws.
#[test]
fn test_pre_rex5_system_tx_ignores_stale_nonce() {
    for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX4] {
        let db = MemoryDatabase::default()
            .account_nonce(MEGA_SYSTEM_ADDRESS, STATE_NONCE)
            .account_code(ORACLE_CONTRACT_ADDRESS, BytecodeBuilder::default().stop().build());
        let mut evm = build_evm(db, spec);

        let result = alloy_evm::Evm::transact_raw(&mut evm, stale_nonce_system_tx())
            .unwrap_or_else(|e| panic!("{spec:?} must promote without a nonce check, got {e:?}"));
        assert!(
            result.result.is_success(),
            "{spec:?} system tx with a stale nonce must still execute, got {:?}",
            result.result,
        );
    }
}

/// REX5 companion assertion so the two sides of the gate are pinned side by side: the same
/// transaction is rejected once the restored nonce check is active.
#[test]
fn test_rex5_system_tx_rejects_stale_nonce() {
    let db = MemoryDatabase::default()
        .account_nonce(MEGA_SYSTEM_ADDRESS, STATE_NONCE)
        .account_code(ORACLE_CONTRACT_ADDRESS, BytecodeBuilder::default().stop().build());
    let mut evm = build_evm(db, MegaSpecId::REX5);

    let err = alloy_evm::Evm::transact_raw(&mut evm, stale_nonce_system_tx())
        .expect_err("REX5 must reject a stale system-tx nonce");
    let rendered = format!("{err:?}");
    assert!(rendered.contains("NonceTooLow"), "expected NonceTooLow, got {rendered}");
}
