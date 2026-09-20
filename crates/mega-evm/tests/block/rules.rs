//! The block rules the Karst base brings, each on its own.

use alloy_evm::block::BlockExecutor;
use alloy_primitives::{Bytes, U256};
use mega_evm::BlockLimits;
use op_revm::{
    constants::{
        DA_FOOTPRINT_GAS_SCALAR_OFFSET, DA_FOOTPRINT_GAS_SCALAR_SLOT, L1_BLOCK_CONTRACT,
        OPERATOR_FEE_CONSTANT_OFFSET, OPERATOR_FEE_RECIPIENT, OPERATOR_FEE_SCALAR_OFFSET,
    },
    L1BlockInfo,
};
use revm::{context::ContextTr, database::State, Database};

use crate::common::{
    self, deposit_tx, executor, unlimited_ctx, user_tx, BLOCK_GAS_LIMIT, BLOCK_NUMBER,
};

/// The word the L1 block contract holds at its scalars slot: the data-availability footprint
/// gas scalar, the operator fee scalar and the operator fee constant, at the offsets the fork
/// reads them from.
fn scalars_word(da_footprint: u16, operator_fee_scalar: u32, operator_fee_constant: u64) -> U256 {
    let mut word = [0_u8; 32];
    word[DA_FOOTPRINT_GAS_SCALAR_OFFSET..DA_FOOTPRINT_GAS_SCALAR_OFFSET + 2]
        .copy_from_slice(&da_footprint.to_be_bytes());
    word[OPERATOR_FEE_SCALAR_OFFSET..OPERATOR_FEE_SCALAR_OFFSET + 4]
        .copy_from_slice(&operator_fee_scalar.to_be_bytes());
    word[OPERATOR_FEE_CONSTANT_OFFSET..OPERATOR_FEE_CONSTANT_OFFSET + 8]
        .copy_from_slice(&operator_fee_constant.to_be_bytes());
    U256::from_be_bytes(word)
}

/// A state whose L1 block contract holds `word` at its scalars slot.
fn state_with_scalars(word: U256) -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_storage(L1_BLOCK_CONTRACT, DA_FOOTPRINT_GAS_SCALAR_SLOT, word);
    State::builder().with_database(db).build()
}

/// Rule 1, alloy-op-evm's `no_user_tx_activation_block`: the block a fork activates in carries
/// the chain's own transactions only, and a user transaction in it is refused before it runs.
#[test]
fn test_activation_block_rejects_a_user_transaction() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx().with_no_user_tx_activation_block(true));
    executor.apply_pre_execution_changes().expect("the block starts");

    let err = executor
        .execute_transaction(&user_tx(0, 100_000))
        .expect_err("a user transaction has no place in an activation block");

    assert!(format!("{err}").contains("non-deposit transaction in fork activation block"), "{err}");
    assert!(executor.receipts().is_empty(), "nothing was packed");
}

/// Rule 1, the other half: the same block executes deposits.
#[test]
fn test_activation_block_executes_deposits() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx().with_no_user_tx_activation_block(true));
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&deposit_tx(Bytes::new(), 100_000))
        .expect("a deposit is what an activation block is for");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
}

/// Rule 2, alloy-op-evm's Jovian DA footprint block limit: with no scalar in state the rule
/// costs nothing and the block reports no blob gas.
#[test]
fn test_da_footprint_is_inert_without_a_scalar() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&user_tx(0, 100_000)).expect("the transaction executes");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.blob_gas_used, 0, "no scalar, no footprint");
}

/// Rule 2: a non-zero scalar makes every non-deposit transaction cost footprint, and the block
/// reports the accumulated footprint as its blob gas used.
#[test]
fn test_da_footprint_within_the_block_budget_is_reported_as_blob_gas() {
    const SCALAR: u16 = 3;
    let mut state = state_with_scalars(scalars_word(SCALAR, 0, 0));
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let first = user_tx(0, 100_000);
    let second = user_tx(1, 100_000);
    let expected = (mega_evm::MegaTransactionExt::estimated_da_size(&first) +
        mega_evm::MegaTransactionExt::estimated_da_size(&second)) *
        u64::from(SCALAR);

    executor.execute_transaction(&first).expect("the first transaction executes");
    executor.execute_transaction(&second).expect("the second transaction executes");

    assert_eq!(executor.limiter().block_da_footprint_used, expected);
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.blob_gas_used, expected, "the block reports its footprint as blob gas");
}

/// Rule 2: a transaction whose footprint does not fit in the block's budget — the block's gas
/// limit — is refused, and the block keeps the footprint it had.
#[test]
fn test_da_footprint_over_the_block_budget_refuses_the_transaction() {
    let mut state = state_with_scalars(scalars_word(u16::MAX, 0, 0));
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let big = common::user_tx_with_input(0, common::incompressible(100_000), 3_000_000);
    let footprint = mega_evm::MegaTransactionExt::estimated_da_size(&big) * u64::from(u16::MAX);
    assert!(footprint > BLOCK_GAS_LIMIT, "the test needs a footprint the block cannot hold");

    let err =
        executor.execute_transaction(&big).expect_err("the footprint does not fit in the block");

    assert!(
        format!("{err}").contains("DA footprint exceeds available block DA footprint"),
        "{err}"
    );
    assert_eq!(executor.limiter().block_da_footprint_used, 0);
}

/// Rule 2: the budget is an inclusive bound — a footprint that exactly fills what the block has
/// is admitted, and the block has nothing left for the next transaction.
#[test]
fn test_da_footprint_that_exactly_fills_the_block_budget_is_admitted() {
    const SCALAR: u16 = 64;
    const GAS_LIMIT: u64 = 900_000;

    let tx = common::user_tx_with_input(0, common::incompressible(20_000), GAS_LIMIT);
    let footprint = mega_evm::MegaTransactionExt::estimated_da_size(&tx) * u64::from(SCALAR);
    assert!(footprint >= GAS_LIMIT, "the block's gas limit must also fit the transaction's gas");

    // The block's gas limit is the footprint's budget, so a block with exactly this gas limit
    // has exactly this transaction's footprint.
    let mut env = common::evm_env();
    env.block_env.gas_limit = footprint;

    let mut state = state_with_scalars(scalars_word(SCALAR, 0, 0));
    let mut executor = common::executor_with_env(&mut state, unlimited_ctx(), env);
    executor.apply_pre_execution_changes().expect("the block starts");

    executor.execute_transaction(&tx).expect("a footprint that exactly fills the budget fits");

    assert_eq!(executor.limiter().block_da_footprint_used, footprint);
    assert_eq!(executor.limiter().available_da_footprint(), 0, "the block has no footprint left");
}

/// Rule 2: a deposit is exempt from the footprint, as it is from the data-availability size.
#[test]
fn test_deposits_do_not_count_towards_the_da_footprint() {
    let mut state = state_with_scalars(scalars_word(u16::MAX, 0, 0));
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    executor
        .execute_transaction(&deposit_tx(common::incompressible(100_000), 3_000_000))
        .expect("a deposit is exempt from the footprint rule");

    assert_eq!(executor.limiter().block_da_footprint_used, 0);
}

/// Rule 3, alloy-op-evm's `l1_block_info`: the read at the start of the block is failable, and
/// an empty L1 block contract answers with zeroes rather than an error.
#[test]
fn test_l1_block_info_reads_an_empty_contract_as_zero() {
    let mut state = common::state();
    let mut executor = executor(&mut state, unlimited_ctx());

    executor.apply_pre_execution_changes().expect("an empty L1 block contract is not an error");

    let chain = executor.evm().ctx().chain();
    assert_eq!(chain.l2_block, Some(U256::from(BLOCK_NUMBER)));
    assert_eq!(chain.da_footprint_gas_scalar, Some(0));
    assert_eq!(chain.operator_fee_scalar, Some(U256::ZERO));
    assert_eq!(chain.l1_base_fee, U256::ZERO);
}

/// Rule 3: the read is conditioned on the block the info is for, so info a caller placed for
/// this very block is not overwritten by what state holds.
#[test]
fn test_l1_block_info_of_this_block_is_not_overwritten() {
    let mut state = state_with_scalars(scalars_word(7, 0, 0));
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.evm_mut().ctx_mut().modify_chain(|chain| {
        *chain = L1BlockInfo {
            l2_block: Some(U256::from(BLOCK_NUMBER)),
            operator_fee_scalar: Some(U256::from(11)),
            operator_fee_constant: Some(U256::ZERO),
            da_footprint_gas_scalar: Some(5),
            ..Default::default()
        };
    });

    executor.apply_pre_execution_changes().expect("the block starts");

    let chain = executor.evm().ctx().chain();
    assert_eq!(chain.operator_fee_scalar, Some(U256::from(11)), "the caller's info stands");
    assert_eq!(chain.da_footprint_gas_scalar, Some(5));
}

/// Rule 3: the operator fee the reward path pays is the one op-revm computes for the Karst
/// spec — gas x scalar x 100 — and not the pre-Jovian formula, which would divide it to zero.
#[test]
fn test_operator_fee_is_charged_on_the_karst_formula() {
    const OPERATOR_FEE_SCALAR: u32 = 1;
    let mut state = state_with_scalars(scalars_word(0, OPERATOR_FEE_SCALAR, 0));
    let mut executor = executor(&mut state, unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let gas = executor.execute_transaction(&user_tx(0, 100_000)).expect("the transaction executes");
    let (evm, _) = executor.finish_with_counters().expect("the block finishes");

    let (db, _) = alloy_evm::Evm::finish(evm);
    let paid = db
        .basic(OPERATOR_FEE_RECIPIENT)
        .expect("the recipient is readable")
        .expect("the operator fee created the recipient")
        .balance;
    assert_eq!(
        paid,
        U256::from(gas.tx_gas_used()) * U256::from(OPERATOR_FEE_SCALAR) * U256::from(100),
        "the Karst formula multiplies by 100 where the pre-Jovian one divides by 1e6"
    );
}

/// The block's gas limit is the block environment's, whatever the context carries: it is the
/// number consensus holds the block to, and the budget the footprint is held to.
#[test]
fn test_block_gas_limit_comes_from_the_block_environment() {
    let mut state = common::state();
    let ctx = common::block_ctx(BlockLimits::no_limits().with_block_gas_limit(1));
    let executor = executor(&mut state, ctx);

    assert_eq!(executor.limiter().limits.block_gas_limit, BLOCK_GAS_LIMIT);
    assert_eq!(executor.limiter().available_da_footprint(), BLOCK_GAS_LIMIT);
}
