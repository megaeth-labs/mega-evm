//! Regression tests for Finding 3: SELFDESTRUCT State Growth Overcounting.
//!
//! Before the fix, `StateGrowthTracker` recorded +1 for CREATE and +1 per new storage slot,
//! but had no SELFDESTRUCT hook. When a same-TX-created contract selfdestructed (EIP-6780),
//! the committed state had neither the account nor its storage, yet the tracker still
//! reported positive growth.
//!
//! The fix adds an `after_selfdestruct` hook that records refunds for the account and its
//! new storage slots when a `CreatedLocal` account is destroyed. Gated to REX4 only.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    handler::EvmTr,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const CALLEE: Address = address!("0000000000000000000000000000000000500001");
const CONTRACT: Address = address!("0000000000000000000000000000000000500002");

// ============================================================================
// HELPERS
// ============================================================================

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    state_growth_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64), EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(state_growth_limit),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let state_growth = evm.ctx_ref().additional_limit.borrow().get_usage().state_growth;
    Ok((r, state_growth))
}

fn default_tx(to: Address) -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(100_000_000).build_fill()
}

/// Builds init code bytes: `body` opcodes followed by SELFDESTRUCT to CALLER.
fn init_code_selfdestruct(body: BytecodeBuilder) -> Vec<u8> {
    body.push_address(CALLER).append(SELFDESTRUCT).build_vec()
}

/// Builds init code: N SSTOREs then SELFDESTRUCT.
fn init_code_sstore_selfdestruct(n: u64) -> Vec<u8> {
    let mut builder = BytecodeBuilder::default();
    for i in 0..n {
        builder = builder.sstore(U256::from(i), U256::from(i + 1));
    }
    init_code_selfdestruct(builder)
}

/// Builds bytecode that CREATEs a contract with the given init code bytes.
fn create_with_init(builder: BytecodeBuilder, init_code: &[u8]) -> BytecodeBuilder {
    let mut b = builder;
    for (i, chunk) in init_code.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        b = b.mstore(i * 32, word);
    }
    b.push_number(init_code.len() as u64).push_number(0_u64).push_number(0_u64).append(CREATE)
}

/// Builds bytecode that CREATE2s a contract with the given init code bytes and salt.
fn create2_with_init(builder: BytecodeBuilder, init_code: &[u8], salt: u64) -> BytecodeBuilder {
    let mut b = builder;
    for (i, chunk) in init_code.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        b = b.mstore(i * 32, word);
    }
    b.push_number(salt)
        .push_number(init_code.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(CREATE2)
}

/// Builds bytecode for a CALL to `target` with given gas.
fn append_call(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

// ============================================================================
// TEST 1: CREATE + SELFDESTRUCT — net growth = 0
// ============================================================================

#[test]
fn test_create_selfdestruct_zero_growth() {
    let init_code = init_code_selfdestruct(BytecodeBuilder::default());
    let callee_code =
        create_with_init(BytecodeBuilder::default(), &init_code).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "CREATE + SELFDESTRUCT should produce zero net growth");
}

// ============================================================================
// TEST 2: CREATE + SSTORE + SELFDESTRUCT — net growth = 0
// ============================================================================

#[test]
fn test_create_sstore_selfdestruct_zero_growth() {
    let init_code = init_code_sstore_selfdestruct(1);
    let callee_code =
        create_with_init(BytecodeBuilder::default(), &init_code).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "CREATE + 1 SSTORE + SELFDESTRUCT should produce zero net growth");
}

// ============================================================================
// TEST 3: CREATE + N SSTOREs + SELFDESTRUCT — all slots refunded
// ============================================================================

#[test]
fn test_create_multiple_sstore_selfdestruct_zero_growth() {
    let init_code = init_code_sstore_selfdestruct(5);
    let callee_code =
        create_with_init(BytecodeBuilder::default(), &init_code).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "CREATE + 5 SSTOREs + SELFDESTRUCT should produce zero net growth");
}

// ============================================================================
// TEST 4: Cross-frame CREATE then SELFDESTRUCT
// ============================================================================

/// Child frame creates a contract (returns runtime code with SELFDESTRUCT).
/// Parent frame then calls the created contract, triggering SELFDESTRUCT.
#[test]
fn test_cross_frame_create_then_selfdestruct() {
    // Runtime code: PUSH20(CALLER) SELFDESTRUCT
    let runtime = BytecodeBuilder::default().push_address(CALLER).append(SELFDESTRUCT).build_vec();

    // Init code: store runtime in memory, RETURN it.
    let mut init = BytecodeBuilder::default();
    let runtime_len = runtime.len();
    let mut word = [0u8; 32];
    word[..runtime_len.min(32)].copy_from_slice(&runtime[..runtime_len.min(32)]);
    init = init.mstore(0, word);
    let init_code =
        init.push_number(runtime_len as u64).push_number(0_u64).append(RETURN).build_vec();

    // CALLEE: CREATE → store address in memory → CALL it → STOP
    let callee_code = create_with_init(BytecodeBuilder::default(), &init_code)
        // Stack: [created_addr]. Store it.
        .push_number(0x100_u64)
        .append(MSTORE)
        // CALL the created contract
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0x100_u64)
        .append(MLOAD) // load created_addr
        .push_number(50_000_u64)
        .append(CALL)
        .append(POP)
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Cross-frame create+selfdestruct should succeed");
    assert_eq!(
        state_growth, 0,
        "Cross-frame CREATE then SELFDESTRUCT should produce zero net growth"
    );
}

// ============================================================================
// TEST 5: Ancestor revert discards SELFDESTRUCT refund
// ============================================================================

/// Child frame creates a contract and successfully selfdestructs it, then returns
/// to parent. Parent then REVERTs. Both the creation growth and the refund from
/// the destruction must be discarded together.
#[test]
fn test_ancestor_revert_discards_selfdestruct_refund() {
    // Runtime code for the created contract: SELFDESTRUCT to CALLER.
    let runtime = BytecodeBuilder::default().push_address(CALLER).append(SELFDESTRUCT).build_vec();

    // Init code: store runtime in memory, RETURN it.
    let mut init = BytecodeBuilder::default();
    let runtime_len = runtime.len();
    let mut word = [0u8; 32];
    word[..runtime_len.min(32)].copy_from_slice(&runtime[..runtime_len.min(32)]);
    init = init.mstore(0, word);
    let init_code =
        init.push_number(runtime_len as u64).push_number(0_u64).append(RETURN).build_vec();

    // CONTRACT: CREATE a child, store its address, CALL it (triggers SELFDESTRUCT),
    // then REVERT the entire frame.
    let contract_code = create_with_init(BytecodeBuilder::default(), &init_code)
        .push_number(0x100_u64)
        .append(MSTORE) // store created addr
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0x100_u64)
        .append(MLOAD) // load created addr
        .push_number(50_000_u64)
        .append(CALL)
        .append(POP)
        // Now REVERT the entire frame — should discard both creation growth and refund.
        .push_number(0_u64)
        .push_number(0_u64)
        .append(REVERT)
        .build();

    // CALLEE: CALL CONTRACT (which will revert), POP, STOP.
    let callee_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 1_000_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(CONTRACT, contract_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success(), "Top-level should succeed after child revert");
    assert_eq!(
        state_growth, 0,
        "Ancestor revert should discard both creation growth and SELFDESTRUCT refund"
    );
}

// ============================================================================
// TEST 6: Pre-existing account SELFDESTRUCT — no refund
// ============================================================================

#[test]
fn test_pre_existing_account_selfdestruct_no_refund() {
    let contract_code =
        BytecodeBuilder::default().push_address(CALLER).append(SELFDESTRUCT).stop().build();
    let callee_code =
        append_call(BytecodeBuilder::default(), CONTRACT, 50_000).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code)
        .account_code(CONTRACT, contract_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "Pre-existing account SELFDESTRUCT should not produce refund");
}

// ============================================================================
// TEST 7: Ephemeral create-destroy loop
// ============================================================================

#[test]
fn test_ephemeral_create_destroy_loop() {
    let init_code = init_code_selfdestruct(BytecodeBuilder::default());

    let mut callee = BytecodeBuilder::default();
    for _ in 0..3 {
        callee = create_with_init(callee, &init_code).append(POP);
    }
    let callee_code = callee.stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    // Limit = 1: would fail if any net growth accumulated.
    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 1, tx).unwrap();

    assert!(result.result.is_success(), "Ephemeral create-destroy loop should succeed");
    assert_eq!(state_growth, 0, "Ephemeral loop should produce zero net growth");
}

// ============================================================================
// TEST 8: CREATE2 + SELFDESTRUCT — zero growth
// ============================================================================

#[test]
fn test_create2_selfdestruct_zero_growth() {
    let init_code = init_code_selfdestruct(BytecodeBuilder::default());
    let callee_code =
        create2_with_init(BytecodeBuilder::default(), &init_code, 42).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(state_growth, 0, "CREATE2 + SELFDESTRUCT should produce zero net growth");
}

// ============================================================================
// TEST 9: Pre-REX4 — overcounting preserved
// ============================================================================

/// On REX3, the refund hook is not active. Same-TX CREATE+SELFDESTRUCT still
/// shows +1 state growth (the old overcounting behavior preserved for replay).
#[test]
fn test_pre_rex4_selfdestruct_overcounting_preserved() {
    let init_code = init_code_selfdestruct(BytecodeBuilder::default());
    let callee_code =
        create_with_init(BytecodeBuilder::default(), &init_code).append(POP).stop().build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX3, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    assert_eq!(
        state_growth, 1,
        "Pre-REX4: CREATE + SELFDESTRUCT should still show +1 (overcounting preserved)"
    );
}

// ============================================================================
// TEST 10: Repeated SELFDESTRUCT must not double-refund
// ============================================================================

/// A same-TX-created contract is selfdestructed twice in the same transaction.
/// The second SELFDESTRUCT must not produce an additional refund.
/// After both destructions, one new SSTORE should bring net growth to 1.
#[test]
fn test_repeated_selfdestruct_no_double_refund() {
    // Runtime code: SELFDESTRUCT to CALLER.
    let runtime = BytecodeBuilder::default().push_address(CALLER).append(SELFDESTRUCT).build_vec();

    // Init code: return runtime.
    let mut init = BytecodeBuilder::default();
    let runtime_len = runtime.len();
    let mut word = [0u8; 32];
    word[..runtime_len.min(32)].copy_from_slice(&runtime[..runtime_len.min(32)]);
    init = init.mstore(0, word);
    let init_code =
        init.push_number(runtime_len as u64).push_number(0_u64).append(RETURN).build_vec();

    // CALLEE: CREATE contract, store addr, CALL it twice (two SELFDESTRUCTs),
    // then write one new SSTORE to produce +1 growth.
    let callee_code = create_with_init(BytecodeBuilder::default(), &init_code)
        // Store created address
        .push_number(0x100_u64)
        .append(MSTORE)
        // First CALL: triggers SELFDESTRUCT (first effective destruction)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0x100_u64)
        .append(MLOAD)
        .push_number(50_000_u64)
        .append(CALL)
        .append(POP)
        // Second CALL: triggers SELFDESTRUCT again (previously_destroyed = true)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0x100_u64)
        .append(MLOAD)
        .push_number(50_000_u64)
        .append(CALL)
        .append(POP)
        // Write one new storage slot to produce +1 net growth
        .sstore(U256::from(999), U256::from(1))
        .stop()
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CALLEE, callee_code);

    let tx = default_tx(CALLEE);
    let (result, state_growth) = transact(MegaSpecId::REX4, &mut db, 100, tx).unwrap();

    assert!(result.result.is_success());
    // CREATE +1, first SELFDESTRUCT -1, second SELFDESTRUCT should NOT -1 again,
    // new SSTORE +1. Net = 1.
    assert_eq!(
        state_growth, 1,
        "Repeated SELFDESTRUCT must not double-refund. Expected net growth = 1 \
         (create +1, first destroy -1, SSTORE +1), got {state_growth}"
    );
}
