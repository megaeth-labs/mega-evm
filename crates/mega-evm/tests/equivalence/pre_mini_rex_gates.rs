//! Boundary coverage for the `EQUIVALENCE` side of the `MINI_REX` gates in the execution face.
//!
//! Two properties are pinned here:
//!
//! 1. `MegaHandler::before_run` promotes a transaction sent by the runtime system address into the
//!    OP deposit-style path — bypassing fee accounting — and rejects one whose callee is not
//!    whitelisted. Both behaviors start at `MINI_REX`; under `EQUIVALENCE` such a transaction is an
//!    ordinary user transaction that pays its own fees and may call anything.
//! 2. The whole `AdditionalLimit` subsystem is dormant before `MINI_REX`: no reset, no intrinsic
//!    accounting, and revm's stock instruction table, so every metered dimension stays at zero
//!    however much state a transaction touches.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{BlockEnv, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
    Database as _,
};

/// A callee that is deliberately absent from `MEGA_SYSTEM_TX_WHITELIST`.
const NON_WHITELISTED_CALLEE: Address = address!("0000000000000000000000000000000000DEAD01");
/// Externally-owned sender of the tracker-dormancy transaction.
const EOA_CALLER: Address = address!("0000000000000000000000000000000000100000");
/// Plain contract used as the target of the tracker-dormancy transaction.
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
/// Inner contract called by [`CALLEE`] so the transaction spans more than one frame.
const INNER: Address = address!("0000000000000000000000000000000000100002");
const BENEFICIARY: Address = address!("00000000000000000000000000000000000BEEF1");

const GAS_LIMIT: u64 = 1_000_000;
const INITIAL_BALANCE: u64 = 1_000_000_000;

fn build_evm<DB: revm::Database + core::fmt::Debug>(
    db: DB,
) -> MegaEvm<DB, NoOpInspector, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::EQUIVALENCE).with_block(BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 0,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    MegaEvm::new(context)
}

fn system_address_tx(callee: Address, gas_price: u128) -> MegaTransaction {
    let mut tx = MegaTransaction::new(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(callee),
        gas_limit: GAS_LIMIT,
        gas_price,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Under `EQUIVALENCE` a transaction from the system address is never inspected against
/// `MEGA_SYSTEM_TX_WHITELIST`, so a non-whitelisted callee executes normally instead of failing
/// validation with the "callee is not in the whitelist" string error.
#[test]
fn test_equivalence_system_address_tx_skips_whitelist_rejection() {
    let db = MemoryDatabase::default()
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(INITIAL_BALANCE))
        .account_code(NON_WHITELISTED_CALLEE, BytecodeBuilder::default().stop().build());
    let mut evm = build_evm(db);

    let result =
        alloy_evm::Evm::transact_raw(&mut evm, system_address_tx(NON_WHITELISTED_CALLEE, 0))
            .expect("EQUIVALENCE must run a system-address tx as an ordinary transaction");
    assert!(result.result.is_success(), "expected ordinary execution, got {:?}", result.result);
}

/// The other half of the same gate: under `EQUIVALENCE` the system-address transaction is not
/// rewritten into a deposit (which forces `gas_price` to zero and degenerates fee accounting),
/// so the sender pays for its own gas even when the callee *is* whitelisted.
#[test]
fn test_equivalence_system_address_tx_still_pays_its_own_fees() {
    let mut db = MemoryDatabase::default()
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(INITIAL_BALANCE))
        .account_code(ORACLE_CONTRACT_ADDRESS, BytecodeBuilder::default().stop().build());
    {
        let mut evm = build_evm(&mut db);
        let result =
            alloy_evm::Evm::transact_raw(&mut evm, system_address_tx(ORACLE_CONTRACT_ADDRESS, 1))
                .expect("whitelisted callee must execute");
        assert!(result.result.is_success(), "got {:?}", result.result);
        assert!(result.result.tx_gas_used() > 0);
        let sender =
            result.state.get(&MEGA_SYSTEM_ADDRESS).expect("sender must appear in the state diff");
        assert!(
            sender.info.balance < U256::from(INITIAL_BALANCE),
            "EQUIVALENCE must charge the system address for its own gas; balance stayed at {}",
            sender.info.balance,
        );
    }
    // Guard the fixture itself: the account really did start with the full balance.
    let stored = db.basic(MEGA_SYSTEM_ADDRESS).expect("db read").expect("account exists");
    assert_eq!(stored.balance, U256::from(INITIAL_BALANCE));
}

/// Pre-`MINI_REX` the `AdditionalLimit` trackers are never reset, never charged the intrinsic
/// cost, and never fed by an instruction wrapper — revm's stock table runs instead. A transaction
/// that writes storage, emits a log, spans two frames, and deploys a contract must therefore leave
/// every metered dimension at zero.
#[test]
fn test_equivalence_leaves_additional_limit_dormant() {
    // INNER: write one storage slot and emit an empty LOG0.
    let inner_code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(2))
        .push_number(0u64)
        .push_number(0u64)
        .append(LOG0)
        .stop()
        .build();
    // CALLEE: write a slot, CALL INNER, then CREATE a one-byte contract (initcode returns
    // `RETURN` of a single zero byte from memory).
    let callee_code = BytecodeBuilder::default()
        .sstore(U256::from(3), U256::from(4))
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(INNER)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .push_number(1u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .stop()
        .build();

    let db = MemoryDatabase::default()
        .account_balance(EOA_CALLER, U256::from(INITIAL_BALANCE))
        .account_balance(CALLEE, U256::from(INITIAL_BALANCE))
        .account_code(CALLEE, callee_code)
        .account_code(INNER, inner_code);
    let mut evm = build_evm(db);

    let mut tx = MegaTransaction::new(TxEnv {
        caller: EOA_CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 10_000_000,
        gas_price: 0,
        data: Bytes::from(vec![0xAAu8; 64]),
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    assert!(result.result.is_success(), "got {:?}", result.result);

    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    assert_eq!(usage.compute_gas, 0, "pre-MINI_REX must not meter compute gas");
    assert_eq!(usage.data_size, 0, "pre-MINI_REX must not meter data size");
    assert_eq!(usage.kv_updates, 0, "pre-MINI_REX must not meter KV updates");
    assert_eq!(usage.state_growth, 0, "pre-MINI_REX must not meter state growth");
}
