//! Value/behavioral tests that close surviving mutants in `crates/mega-evm/src/block/`.
//!
//! The `src/block/*` modules carry inline `#[cfg(test)] mod tests`, but those unit tests are not
//! exercised by the mutation run (which builds the integration test targets). These integration
//! tests re-assert the same invariants through the crate's public API, using exact-value
//! assertions (never `> 0`, so the `-> 1` constant-return mutants die too).
//!
//! Covered survivors:
//! * `helpers.rs` — `MegaTransactionExt::{tx_size, tx_hash, estimated_da_size}` for
//!   `MegaTxEnvelope`, `Recovered<MegaTxEnvelope>` and `EnrichedMegaTx<T>` (constant-return /
//!   `Default::default()` mutants).
//! * `limit.rs` — `BlockLimits::from_hardfork_and_block_gas_limit` per-field initializers (deleting
//!   a field initializer makes it fall back to `..limits`, i.e. a different value).
//! * `hardfork.rs` — `MegaHardforks::is_*_active_at_timestamp` (`-> false` mutants).
//! * `chain.rs` — `hardfork_schedule` mainnet / testnet match arms (deleting an arm falls through
//!   to the all-activated fallback, which has different activation conditions).

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_hardforks::ForkCondition;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, U256};
use mega_evm::{
    constants, hardfork_schedule, mainnet_hardforks, testnet_hardforks, BlockLimits, MegaHardfork,
    MegaHardforkConfig, MegaHardforks, MegaTransactionExt, MegaTxEnvelope, MAINNET_CHAIN_ID,
    TESTNET_CHAIN_ID,
};

const CALLER: Address = address!("2000000000000000000000000000000000000001");
const CONTRACT: Address = address!("3000000000000000000000000000000000000001");

/// A known, non-zero cached tx hash baked into the test envelope. `MegaTxEnvelope::tx_hash` returns
/// the cached `Signed` hash, so storing a non-zero value here lets us distinguish the real accessor
/// from the `-> Default::default()` (== `TxHash::ZERO`) mutant.
const STORED_TX_HASH: alloy_primitives::B256 =
    alloy_primitives::b256!("00000000000000000000000000000000000000000000000000000000000000aa");

/// A legacy transaction with a small, non-empty calldata so its encoded size and DA size are both
/// strictly greater than 1 (which kills both the `-> 0` and the `-> 1` constant-return mutants),
/// and a known non-zero cached hash so the `tx_hash` mutant is distinguishable.
fn legacy_envelope() -> MegaTxEnvelope {
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce: 7,
        gas_price: 9,
        gas_limit: 21_000,
        to: TxKind::Call(CONTRACT),
        value: U256::from(11),
        input: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
    };
    MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), STORED_TX_HASH))
}

// ============================================================================
// block/helpers.rs — MegaTransactionExt accessors
// ============================================================================

/// `MegaTxEnvelope::tx_size`/`estimated_da_size` must return the real encoded sizes (well above 1),
/// and `tx_hash` must return the real hash (not `Default::default()`).
#[test]
fn mega_tx_envelope_ext_returns_real_size_da_and_hash() {
    let tx = legacy_envelope();

    // A signed legacy tx with 5 bytes of calldata encodes to far more than 1 byte.
    let tx_size = MegaTransactionExt::tx_size(&tx);
    let da_size = MegaTransactionExt::estimated_da_size(&tx);
    assert!(tx_size > 1, "tx_size collapsed to a constant: {tx_size}");
    assert!(da_size > 1, "estimated_da_size collapsed to a constant: {da_size}");

    // tx_hash must return the cached hash, not Default::default() (== TxHash::ZERO).
    let h = MegaTransactionExt::tx_hash(&tx);
    assert_ne!(h, alloy_primitives::TxHash::ZERO, "tx_hash returned Default::default()");
    assert_eq!(h, STORED_TX_HASH, "tx_hash mismatch");
}

/// `Recovered<MegaTxEnvelope>::tx_hash` must return the inner envelope hash, not the default.
#[test]
fn recovered_mega_tx_envelope_ext_returns_real_hash() {
    let tx = legacy_envelope();
    let recovered = Recovered::new_unchecked(tx, CALLER);

    let h = MegaTransactionExt::tx_hash(&recovered);
    assert_ne!(h, alloy_primitives::TxHash::ZERO, "recovered tx_hash returned Default::default()");
    assert_eq!(h, STORED_TX_HASH, "recovered tx_hash mismatch");
}

// `EnrichedMegaTx::{tx_size, estimated_da_size}` (helpers.rs:89/93) are NOT tested here: the
// stored-field overrides are unreachable — the trait method's `where Self: Encodable2718` bound is
// unsatisfiable for the wrapper (it does not implement `Encodable2718`), and `.method()` resolves
// via `Deref` to the inner tx instead, so the override never dispatches. Those mutants are recorded
// as dead/equivalent in `mutants/suppressions.toml`. (This dead override looks like a latent
// perf bug: callers recompute size/da instead of using the precomputed fields.)

// ============================================================================
// block/limit.rs — BlockLimits::from_hardfork_and_block_gas_limit
// ============================================================================

/// For a Rex hardfork, all three block-level field initializers must be set to their constants.
/// Deleting any initializer drops back to `..limits` (== `no_limits()` => `u64::MAX` for
/// state-growth, and the same `MiniRex` constants for data/kv), so asserting the exact value kills
/// the field-deletion mutants on lines 399/400/401.
#[test]
fn block_limits_from_rex_sets_all_three_block_fields() {
    let limits = BlockLimits::from_hardfork_and_block_gas_limit(MegaHardfork::Rex4, 50_000_000);

    assert_eq!(
        limits.block_txs_data_limit,
        constants::mini_rex::BLOCK_DATA_LIMIT,
        "block_txs_data_limit initializer dropped"
    );
    assert_eq!(
        limits.block_kv_update_limit,
        constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
        "block_kv_update_limit initializer dropped"
    );
    assert_eq!(
        limits.block_state_growth_limit,
        constants::rex::BLOCK_STATE_GROWTH_LIMIT,
        "block_state_growth_limit initializer dropped"
    );
    assert_eq!(limits.block_gas_limit, 50_000_000, "block gas limit not applied");
}

/// For the `MiniRex` arm, `block_txs_data_limit` and `block_kv_update_limit` are set to the
/// `MiniRex` constants while `block_state_growth_limit` stays at the `no_limits()` default
/// (`u64::MAX`). Asserting the exact data/kv values kills the field-deletion mutants on lines
/// 405/406; the state-growth assertion pins the arm's distinct shape.
#[test]
fn block_limits_from_mini_rex_sets_data_and_kv_fields() {
    let limits = BlockLimits::from_hardfork_and_block_gas_limit(MegaHardfork::MiniRex, 30_000_000);

    assert_eq!(
        limits.block_txs_data_limit,
        constants::mini_rex::BLOCK_DATA_LIMIT,
        "MiniRex block_txs_data_limit initializer dropped"
    );
    assert_eq!(
        limits.block_kv_update_limit,
        constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
        "MiniRex block_kv_update_limit initializer dropped"
    );
    // MiniRex does not set a block state-growth limit; it inherits no_limits().
    assert_eq!(
        limits.block_state_growth_limit,
        u64::MAX,
        "MiniRex must not set a block state-growth limit"
    );
}

// ============================================================================
// block/hardfork.rs — is_*_active_at_timestamp
// ============================================================================

/// Build a config where every named fork activates at a distinct, increasing timestamp so each
/// `is_*_active_at_timestamp` can be probed both before and at activation.
fn staged_config() -> MegaHardforkConfig {
    MegaHardforkConfig::new()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100))
        .with(MegaHardfork::MiniRex1, ForkCondition::Timestamp(200))
        .with(MegaHardfork::MiniRex2, ForkCondition::Timestamp(300))
        .with(MegaHardfork::Rex, ForkCondition::Timestamp(400))
        .with(MegaHardfork::Rex1, ForkCondition::Timestamp(500))
        .with(MegaHardfork::Rex2, ForkCondition::Timestamp(600))
        .with(MegaHardfork::Rex3, ForkCondition::Timestamp(700))
        .with(MegaHardfork::Rex4, ForkCondition::Timestamp(800))
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(900))
}

/// Each `is_*_active_at_timestamp` returns `true` at/after activation and `false` before. This
/// kills the `-> false` mutants on the MiniRex1/MiniRex2/Rex1/Rex3 predicates (and pins the rest).
#[test]
fn hardfork_activation_predicates_are_true_at_activation() {
    let cfg = staged_config();

    // MiniRex1 (line 144) — false strictly before, true at activation.
    assert!(!cfg.is_mini_rex_1_active_at_timestamp(199));
    assert!(cfg.is_mini_rex_1_active_at_timestamp(200));

    // MiniRex2 (line 149).
    assert!(!cfg.is_mini_rex_2_active_at_timestamp(299));
    assert!(cfg.is_mini_rex_2_active_at_timestamp(300));

    // Rex1 (line 159).
    assert!(!cfg.is_rex_1_active_at_timestamp(499));
    assert!(cfg.is_rex_1_active_at_timestamp(500));

    // Rex3 (line 169).
    assert!(!cfg.is_rex_3_active_at_timestamp(699));
    assert!(cfg.is_rex_3_active_at_timestamp(700));

    // Sanity on the neighbouring predicates so the staged config is self-consistent.
    assert!(cfg.is_mini_rex_active_at_timestamp(100));
    assert!(cfg.is_rex_active_at_timestamp(400));
    assert!(cfg.is_rex_2_active_at_timestamp(600));
    assert!(cfg.is_rex_4_active_at_timestamp(800));
    assert!(cfg.is_rex_5_active_at_timestamp(900));
}

// ============================================================================
// block/chain.rs — hardfork_schedule match arms
// ============================================================================

/// Deleting the `MAINNET_CHAIN_ID` arm makes `hardfork_schedule(4326)` fall through to the
/// all-activated fallback, which activates `MiniRex1` at genesis. The real mainnet schedule
/// activates `MiniRex1` at a specific non-zero timestamp, so the per-fork conditions distinguish
/// the two.
#[test]
fn hardfork_schedule_mainnet_arm_returns_mainnet_schedule() {
    let from_dispatch = hardfork_schedule(MAINNET_CHAIN_ID);
    let expected = mainnet_hardforks();

    // MiniRex1 activation timestamp is mainnet-specific (1764845637), differs from the genesis
    // activation of the all-activated fallback.
    assert_eq!(
        from_dispatch.get(MegaHardfork::MiniRex1),
        expected.get(MegaHardfork::MiniRex1),
        "mainnet arm did not return the mainnet schedule"
    );
    assert_eq!(
        from_dispatch.get(MegaHardfork::MiniRex1),
        Some(&ForkCondition::Timestamp(1764845637)),
        "mainnet MiniRex1 activation timestamp changed"
    );
    assert_eq!(
        from_dispatch.get(MegaHardfork::Rex5),
        Some(&ForkCondition::Timestamp(1780632000)),
        "mainnet Rex5 activation timestamp changed"
    );
}

/// Deleting the `TESTNET_CHAIN_ID` arm makes `hardfork_schedule(6343)` fall through to the
/// all-activated fallback. The testnet schedule sets MiniRex1/MiniRex2 to `Never` and Rex5 to a
/// testnet-specific timestamp, none of which the fallback reproduces.
#[test]
fn hardfork_schedule_testnet_arm_returns_testnet_schedule() {
    let from_dispatch = hardfork_schedule(TESTNET_CHAIN_ID);
    let expected = testnet_hardforks();

    assert_eq!(
        from_dispatch.get(MegaHardfork::MiniRex1),
        expected.get(MegaHardfork::MiniRex1),
        "testnet arm did not return the testnet schedule"
    );
    // Testnet disables MiniRex1 (Never) — the all-activated fallback would activate it at genesis.
    assert_eq!(
        from_dispatch.get(MegaHardfork::MiniRex1),
        Some(&ForkCondition::Never),
        "testnet MiniRex1 should be Never"
    );
    assert_eq!(
        from_dispatch.get(MegaHardfork::Rex5),
        Some(&ForkCondition::Timestamp(1780459200)),
        "testnet Rex5 activation timestamp changed"
    );
}

// ============================================================================
// block/limit.rs — BlockLimiter
// ============================================================================
//
// `BlockLimiter` (`pub`, re-exported via `block::*`) is constructed directly with
// `BlockLimiter::new(BlockLimits)` and its `pub` usage counters are written directly,
// mirroring the inline `#[cfg(test)]` pattern at `limit.rs:1027+`. We start from
// `BlockLimits::no_limits()` (every limit == `u64::MAX`) and tighten exactly the one
// dimension under test, so the other clauses/checks cannot interfere.
//
// Covered survivors:
// * `pre_execution_check` boundary `>` checks (778/789/802/839): an at-limit value must return `Ok`
//   (kills both `>`→`==` and `>`→`>=`, which error at equality); a strictly over-limit value must
//   return the expected `Err`.
// * `post_execution_update` (947) `==`→`!=` on the deposit-type check: a non-deposit tx must
//   advance `block_da_size_used`; a deposit tx must not. The `!=` mutant inverts both.
// * `is_block_limit_reached` (1011-1019): one per-dimension test with only that dimension at `used
//   == limit` (others strictly below) asserting `true` kills that clause's `>=`→`<` and the
//   adjacent `||`→`&&` (only one true clause ⇒ `&&` collapses to false); an all-below test
//   asserting `false` kills the whole-fn `->true`, and the per-dimension trues kill the whole-fn
//   `->false`.

use alloy_consensus::transaction::Recovered as MegaRecovered;
use alloy_primitives::B256;
use mega_evm::{BlockLimiter, BlockMegaTransactionOutcome, MegaHaltReason, MegaTransactionOutcome};
use revm::{
    context::result::{ExecutionResult, Output, SuccessReason},
    state::EvmState,
};

/// Builds a `BlockMegaTransactionOutcome` wrapping the given recovered tx, with the supplied
/// `da_size` and otherwise-zero usage, and a trivial `Success` execution result. Used to drive
/// `post_execution_update` and observe how the deposit-type branch routes the da accounting.
fn outcome_for(
    tx: MegaRecovered<MegaTxEnvelope>,
    da_size: u64,
) -> BlockMegaTransactionOutcome<MegaRecovered<MegaTxEnvelope>> {
    BlockMegaTransactionOutcome {
        tx,
        tx_size: 0,
        da_size,
        depositor: None,
        inner: MegaTransactionOutcome {
            result: ExecutionResult::<MegaHaltReason>::Success {
                reason: SuccessReason::Stop,
                gas_used: 0,
                gas_refunded: 0,
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state: EvmState::default(),
            data_size: 0,
            kv_updates: 0,
            compute_gas_used: 0,
            state_growth_used: 0,
        },
    }
}

/// A non-deposit (legacy, tx type 0) recovered transaction.
fn legacy_recovered() -> MegaRecovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce: 0,
        gas_price: 1,
        gas_limit: 21_000,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx, Signature::test_signature(), B256::ZERO);
    MegaRecovered::new_unchecked(MegaTxEnvelope::Legacy(signed), CALLER)
}

// ---------------------------------------------------------------------------
// pre_execution_check boundaries
// ---------------------------------------------------------------------------

/// 778:22 (`gas_limit > self.limits.tx_gas_limit`). A gas limit *equal* to the per-tx cap must
/// pass (kills `>`→`==` and `>`→`>=`, which both error at equality); one over must error.
#[test]
fn pre_execution_check_tx_gas_limit_boundary() {
    let mut limits = BlockLimits::no_limits();
    limits.tx_gas_limit = 1_000;
    let limiter = BlockLimiter::new(limits);

    assert!(
        limiter.pre_execution_check(B256::ZERO, 1_000, 0, 0, false).is_ok(),
        "gas_limit == tx_gas_limit must be admitted (boundary is strict `>`)"
    );
    let over = limiter.pre_execution_check(B256::ZERO, 1_001, 0, 0, false);
    assert!(over.is_err(), "gas_limit == tx_gas_limit + 1 must be rejected");
    assert!(
        format!("{:?}", over.unwrap_err()).contains("TransactionGasLimit"),
        "over-limit gas must report the per-tx gas-limit error"
    );
}

/// 789:58 (`block_gas_used + gas_limit > block_gas_limit`). With `block_gas_used == 0`, a
/// `gas_limit` exactly equal to `block_gas_limit` must pass (kills `>`→`>=`); one over errors.
#[test]
fn pre_execution_check_block_gas_boundary() {
    let mut limits = BlockLimits::no_limits();
    limits.block_gas_limit = 1_000;
    let limiter = BlockLimiter::new(limits);

    assert!(
        limiter.pre_execution_check(B256::ZERO, 1_000, 0, 0, false).is_ok(),
        "block_gas_used + gas_limit == block_gas_limit must be admitted"
    );
    let over = limiter.pre_execution_check(B256::ZERO, 1_001, 0, 0, false);
    assert!(over.is_err(), "block_gas_used + gas_limit == block_gas_limit + 1 must be rejected");
    assert!(
        format!("{:?}", over.unwrap_err()).contains("TransactionGasLimitMoreThanAvailableBlockGas"),
        "over-limit block gas must report the available-block-gas error"
    );
}

/// 802:20 (`tx_size > self.limits.tx_encode_size_limit`). A `tx_size` equal to the cap must pass
/// (kills `>`→`==` and `>`→`>=`); one over errors.
#[test]
fn pre_execution_check_tx_size_boundary() {
    let mut limits = BlockLimits::no_limits();
    limits.tx_encode_size_limit = 500;
    let limiter = BlockLimiter::new(limits);

    assert!(
        limiter.pre_execution_check(B256::ZERO, 0, 500, 0, false).is_ok(),
        "tx_size == tx_encode_size_limit must be admitted"
    );
    let over = limiter.pre_execution_check(B256::ZERO, 0, 501, 0, false);
    assert!(over.is_err(), "tx_size == tx_encode_size_limit + 1 must be rejected");
    assert!(
        format!("{:?}", over.unwrap_err()).contains("TransactionEncodeSizeLimit"),
        "over-limit tx size must report the per-tx encode-size error"
    );
}

/// 839:64 (`da_size + block_da_size_used > block_da_size_limit`). With `block_da_size_used == 0`,
/// a non-deposit `da_size` equal to `block_da_size_limit` must pass (kills `>`→`>=`); one over
/// errors. (`tx_da_size_limit` stays `u64::MAX`, so the per-tx da check at 828 never triggers.)
#[test]
fn pre_execution_check_block_da_size_boundary() {
    let mut limits = BlockLimits::no_limits();
    limits.block_da_size_limit = 800;
    let limiter = BlockLimiter::new(limits);

    assert!(
        limiter.pre_execution_check(B256::ZERO, 0, 0, 800, false).is_ok(),
        "da_size + block_da_size_used == block_da_size_limit must be admitted"
    );
    let over = limiter.pre_execution_check(B256::ZERO, 0, 0, 801, false);
    assert!(
        over.is_err(),
        "da_size + block_da_size_used == block_da_size_limit + 1 must be rejected"
    );
    assert!(
        format!("{:?}", over.unwrap_err()).contains("DataAvailabilitySizeLimit"),
        "over-limit block da must report the data-availability error"
    );
}

// ---------------------------------------------------------------------------
// post_execution_update — deposit-type branch (947 `==`→`!=`)
// ---------------------------------------------------------------------------

/// 947 (`outcome.tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE`). A non-deposit (legacy) tx must
/// advance `block_da_size_used` by `da_size`. Under the `==`→`!=` mutant the legacy type is
/// (mis)classified as a deposit, so the da counter would stay at 0.
#[test]
fn post_execution_update_advances_da_for_non_deposit() {
    let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
    let outcome = outcome_for(legacy_recovered(), 1_234);

    limiter.post_execution_update(&outcome).expect("post_execution_update should succeed");

    assert_eq!(
        limiter.block_da_size_used, 1_234,
        "a non-deposit tx must accumulate da_size; the `==`→`!=` mutant would skip it (0)"
    );
}

// ---------------------------------------------------------------------------
// is_block_limit_reached — per-dimension boundary + all-clause OR (1011-1019)
// ---------------------------------------------------------------------------

/// All counters strictly below their limits ⇒ `false`. Kills the whole-fn `->true` mutant.
#[test]
fn is_block_limit_reached_all_below_is_false() {
    let mut limits = BlockLimits::no_limits();
    limits.block_gas_limit = 10;
    limits.block_txs_encode_size_limit = 10;
    limits.block_da_size_limit = 10;
    limits.block_txs_data_limit = 10;
    limits.block_kv_update_limit = 10;
    limits.block_compute_gas_limit = 10;
    limits.block_state_growth_limit = 10;
    let mut limiter = BlockLimiter::new(limits);
    limiter.block_gas_used = 9;
    limiter.block_tx_size_used = 9;
    limiter.block_da_size_used = 9;
    limiter.block_data_used = 9;
    limiter.block_kv_updates_used = 9;
    limiter.block_compute_gas_used = 9;
    limiter.block_state_growth_used = 9;

    assert!(
        !limiter.is_block_limit_reached(),
        "with every counter strictly below its limit the block is not full"
    );
}

/// Helper: a limiter where exactly one dimension sits at `used == limit` and all others are
/// strictly below (limit 10, used 0). Asserting `true` kills that clause's `>=`→`<` (equality
/// would become false) and the adjacent `||`→`&&` (with a single true clause, `&&` ⇒ false).
macro_rules! only_dimension_at_limit {
    ($limit_field:ident, $used_field:ident) => {{
        let mut limits = BlockLimits::no_limits();
        limits.block_gas_limit = 10;
        limits.block_txs_encode_size_limit = 10;
        limits.block_da_size_limit = 10;
        limits.block_txs_data_limit = 10;
        limits.block_kv_update_limit = 10;
        limits.block_compute_gas_limit = 10;
        limits.block_state_growth_limit = 10;
        limits.$limit_field = 5;
        let mut limiter = BlockLimiter::new(limits);
        // all counters strictly below their (10) limits...
        limiter.block_gas_used = 0;
        limiter.block_tx_size_used = 0;
        limiter.block_da_size_used = 0;
        limiter.block_data_used = 0;
        limiter.block_kv_updates_used = 0;
        limiter.block_compute_gas_used = 0;
        limiter.block_state_growth_used = 0;
        // ...except the one under test, which sits exactly at its (5) limit.
        limiter.$used_field = 5;
        limiter
    }};
}

#[test]
fn is_block_limit_reached_gas_dimension() {
    let limiter = only_dimension_at_limit!(block_gas_limit, block_gas_used);
    assert!(limiter.is_block_limit_reached(), "gas at limit ⇒ block full");
}

#[test]
fn is_block_limit_reached_tx_size_dimension() {
    let limiter = only_dimension_at_limit!(block_txs_encode_size_limit, block_tx_size_used);
    assert!(limiter.is_block_limit_reached(), "tx encode size at limit ⇒ block full");
}

#[test]
fn is_block_limit_reached_da_size_dimension() {
    let limiter = only_dimension_at_limit!(block_da_size_limit, block_da_size_used);
    assert!(limiter.is_block_limit_reached(), "da size at limit ⇒ block full");
}

#[test]
fn is_block_limit_reached_data_dimension() {
    let limiter = only_dimension_at_limit!(block_txs_data_limit, block_data_used);
    assert!(limiter.is_block_limit_reached(), "tx data at limit ⇒ block full");
}

#[test]
fn is_block_limit_reached_kv_updates_dimension() {
    let limiter = only_dimension_at_limit!(block_kv_update_limit, block_kv_updates_used);
    assert!(limiter.is_block_limit_reached(), "kv updates at limit ⇒ block full");
}

#[test]
fn is_block_limit_reached_compute_gas_dimension() {
    let limiter = only_dimension_at_limit!(block_compute_gas_limit, block_compute_gas_used);
    assert!(limiter.is_block_limit_reached(), "compute gas at limit ⇒ block full");
}

#[test]
fn is_block_limit_reached_state_growth_dimension() {
    let limiter = only_dimension_at_limit!(block_state_growth_limit, block_state_growth_used);
    assert!(limiter.is_block_limit_reached(), "state growth at limit ⇒ block full");
}
