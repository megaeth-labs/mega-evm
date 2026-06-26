//! Value/behavioral tests that close surviving mutants in `crates/mega-evm/src/block/executor.rs`
//! and `crates/mega-evm/src/external/`.
//!
//! The `src/**` modules carry inline `#[cfg(test)] mod tests`, but those unit tests are not
//! exercised by the mutation run (which builds the integration test targets). These integration
//! tests re-assert the same invariants through the crate's public / `test-utils`-gated API.
//!
//! Covered survivors:
//! * `block/executor.rs:70` — `Debug::fmt` replaced with `Ok(Default::default())`. The real impl
//!   writes the struct name; the mutant writes nothing. We pin the formatted output.
//! * `block/executor.rs:297` — `post_execution_changes -> Ok(vec![])`. The real body *always*
//!   pushes exactly one `PostBlock(BalanceIncrements)` outcome (because
//!   `eips::transact_balance_increments` unconditionally returns `Some(state)`), so a fresh
//!   executor's `post_execution_changes()` returns a length-1 vec. The mutant returns length 0.
//! * `block/executor.rs:496` — `get_accessed_bucket_ids -> vec![]` and `->
//!   vec![Default::default()]` (i.e. `vec![0]`). After a transaction that performs an `SSTORE`
//!   (zero→non-zero), the dynamic storage gas tracker records the slot's bucket id. With
//!   `TestExternalEnvs`'s default `SimpleBucketHasher`, that id lies in `[NUM_META_BUCKETS,
//!   NUM_BUCKETS)` (≥ `65_536`), so the returned set is non-empty and never `{0}` — killing both
//!   variants.
//! * `external/hasher/fallback.rs:{109,112,120}` — value-changing mutations in
//!   `DeterministicHasher::write`. `AHashBucketHasher` is the public, production-compatible hasher
//!   whose contract is to reproduce SPECIFIC bucket ids matching the `salt` crate. We pin known
//!   input→bucket-id pairs, one per affected `write` branch, against the documented expected values
//!   (the same constants cross-checked against `salt` in `src/external/hasher/mod.rs`). The `125:47
//!   |`→`&` mutant is also killed by `ahash_bucket_id_small_input_is_pinned` (the small-input
//!   combine), but the `125:47 |`→`^` mutant is provably equivalent and is recorded in
//!   `mutants/suppressions.toml`: at line 125 the `|` ORs `value[0] as u128` (occupies only bits
//!   0..63) with `(value[1] as u128) << 64` (occupies only bits 64..127); the operands have
//!   disjoint set bits for every input, so `| == ^` identically and no input distinguishes them.
//! * `external/hasher/fallback.rs:{108,112}` — the `>`→`>=` length-routing boundary mutants in
//!   `write`. The full run hid these because no pinned input had a length sitting exactly on a
//!   boundary; we add `len == {8,32}` inputs whose bucket id flips when the comparison does
//!   (`ahash_bucket_id_len{8,32}_*`). The sibling line-109 `> 16`→`>= 16` is provably equivalent (a
//!   len-16 input hashes identically either way — see the suppression note below) and is recorded
//!   in `mutants/suppressions.toml`.
//!
//! The remaining `fallback.rs` survivors are recorded in `mutants/suppressions.toml` as
//! dead/unreachable code rather than killed here: the typed `write_u8/u16/u32/u64/u128 -> ()`
//! stubs and the two `^`→`|`/`&` mutants inside `update`. `AHashBucketHasher::bucket_id` drives
//! only the slice `write` (→ `large_update`); it never invokes the typed `write_uN` methods nor
//! `update`. Those are reachable solely via `core::hash::Hasher` trait dispatch on the
//! `pub(crate)`, non-re-exported `DeterministicHasher`, which no integration test (separate crate)
//! can construct — and no non-test caller anywhere in `mega-evm` invokes them. So no public-API
//! input distinguishes the mutants.
//!
//! `external/mod.rs:94` (`EmptyExternalEnv::external_envs` →
//! `ExternalEnvs::from(Default::default())`) is provably equivalent and is recorded in
//! `mutants/suppressions.toml` instead of being killed here: `EmptyExternalEnv` is a unit struct,
//! so both the real body (`ExternalEnvs { salt_env: EmptyExternalEnv, oracle_env: EmptyExternalEnv
//! }`) and the mutant (`From<ExternalEnvs<EmptyExternalEnv>>` of the `Default` impl, which yields
//! the identical fields) construct the same value; no observable behavior distinguishes them.

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{
    block::{BlockExecutor, StateChangePostBlockSource, StateChangeSource},
    EvmEnv, EvmFactory,
};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    AHashBucketHasher, BlockLimits, BucketHasher, MegaBlockExecutionCtx, MegaBlockExecutor,
    MegaEvmFactory, MegaHardfork, MegaHardforkConfig, MegaSpecId, MegaTxEnvelope, TestExternalEnvs,
};
use revm::{context::BlockEnv, database::State};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const STORAGE_CONTRACT: Address = address!("1000000000000000000000000000000000000003");

/// Lower bound of the valid SALT bucket range (`NUM_BUCKETS / MIN_BUCKET_SIZE`, i.e. 256^3 / 256).
/// Both `SimpleBucketHasher` and `AHashBucketHasher` map keys into `[NUM_META_BUCKETS,
/// NUM_BUCKETS)`.
const NUM_META_BUCKETS: u32 = (1u32 << 24) / 256; // 65_536

fn create_transaction(
    nonce: u64,
    to: Address,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit: 1_000_000,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Wires a `MegaBlockExecutor` over `state` at the `MiniRex` spec, with a `TestExternalEnvs` salt
/// environment (default `SimpleBucketHasher`). Mirrors
/// `tests/block_executor/accessed_block_hashes`.
macro_rules! build_executor {
    ($state:expr) => {{
        let external_envs = TestExternalEnvs::<Infallible>::new();
        let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

        let mut cfg_env = revm::context::CfgEnv::default();
        cfg_env.spec = MegaSpecId::MINI_REX;
        let block_env = BlockEnv {
            number: U256::from(1000),
            timestamp: U256::from(1_800_000_000),
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let evm = evm_factory.create_evm($state, EvmEnv::new(cfg_env, block_env));

        let block_ctx =
            MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());
        let chain_spec =
            MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
        MegaBlockExecutor::new(evm, block_ctx, chain_spec, OpAlloyReceiptBuilder::default())
    }};
}

// ============================================================================
// block/executor.rs:70 — Debug::fmt
// ============================================================================

/// `Debug` for `MegaBlockExecutor` must write the struct name. The mutant returns `Ok(())`
/// without writing anything, producing an empty string.
#[test]
fn block_executor_debug_writes_struct_name() {
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();
    let executor = build_executor!(&mut state);

    let rendered = format!("{executor:?}");
    assert!(
        rendered.contains("MegaethBlockExecutor"),
        "Debug output must name the struct, got {rendered:?}"
    );
    assert!(!rendered.is_empty(), "Debug output must not be empty");
}

// ============================================================================
// block/executor.rs:297 — post_execution_changes
// ============================================================================

/// `post_execution_changes` always produces exactly one `PostBlock(BalanceIncrements)` outcome,
/// because `eips::transact_balance_increments` unconditionally returns `Some(state)`. The mutant
/// drops it to an empty vec.
#[test]
fn post_execution_changes_yields_balance_increment_outcome() {
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = build_executor!(&mut state);

    let outcomes =
        executor.post_execution_changes().expect("post_execution_changes should succeed");

    assert_eq!(
        outcomes.len(),
        1,
        "post_execution_changes must yield exactly one system-call outcome; the mutant drops it"
    );
    assert!(
        matches!(
            outcomes[0].source,
            StateChangeSource::PostBlock(StateChangePostBlockSource::BalanceIncrements)
        ),
        "the single outcome must be the post-block balance-increment source, got {:?}",
        outcomes[0].source
    );
}

// ============================================================================
// block/executor.rs:496 — get_accessed_bucket_ids
// ============================================================================

/// A contract that writes a non-zero value to a previously-zero storage slot, then stops.
/// The `SSTORE` triggers the dynamic SALT gas hook, recording the slot's bucket id.
fn sstore_contract() -> Bytes {
    BytecodeBuilder::default().sstore(U256::from(7), U256::from(42)).stop().build()
}

/// After a transaction performs an `SSTORE`, `get_accessed_bucket_ids` returns the non-empty set
/// of buckets touched. With the default `SimpleBucketHasher` every recorded id is in
/// `[NUM_META_BUCKETS, NUM_BUCKETS)` (≥ `65_536`), so the result is neither empty (kills `vec![]`)
/// nor `{0}` (kills `vec![Default::default()]`).
#[test]
fn get_accessed_bucket_ids_records_sstore_bucket() {
    let mut db = MemoryDatabase::default();
    db.set_account_code(STORAGE_CONTRACT, sstore_contract());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();
    let mut executor = build_executor!(&mut state);

    executor
        .execute_transaction(&create_transaction(0, STORAGE_CONTRACT))
        .expect("sstore tx should succeed");

    let ids = executor.get_accessed_bucket_ids();
    assert!(!ids.is_empty(), "an SSTORE must record at least one accessed bucket id");
    assert!(
        ids.iter().all(|&id| id >= NUM_META_BUCKETS),
        "every recorded bucket id must lie in the valid KV range (>= {NUM_META_BUCKETS}); \
         a `vec![0]` mutant would violate this, got {ids:?}"
    );
}

// ============================================================================
// external/hasher/fallback.rs:{109,112,120,125} — DeterministicHasher::write branches
// ============================================================================
//
// `AHashBucketHasher::bucket_id(key)` = `hash(key) % NUM_KV_BUCKETS + NUM_META_BUCKETS`, where
// `hash` is the deterministic AHash fallback. Its `write` routes by input length:
//
//   * len <= 8       -> small branch; line 125 `|` combine
//   * 9 <= len <= 16 -> mid branch;  line 120 `|` combine, and the line-109 `> 16` outer guard
//     (false here) selects this branch
//   * len >= 17      -> large branch; the line-109 `> 16` outer guard (true) and the line-112
//     `while data.len() > 16` loop guard both gate the 16-byte block reads
//
// The four mutations (`>`→`==` on 109/112, `|`→`&` on 120/125) all change the produced hash for an
// input that hits their branch, hence change the bucket id. We pin one input per branch against the
// expected unmutated value. Expected hashes are the documented cross-checked values from
// `src/external/hasher/mod.rs` (`hash("hello") = 1027176506268606463`, etc.); expected bucket ids
// for the >=17-byte inputs are the documented `bucket_id` cross-check values from that same module.

/// `NUM_KV_BUCKETS = NUM_BUCKETS - NUM_META_BUCKETS = 16_777_216 - 65_536`.
const NUM_KV_BUCKETS: u64 = (1u64 << 24) - ((1u64 << 24) / 256);
const NUM_META_BUCKETS_U64: u64 = (1u64 << 24) / 256;

/// Maps a documented `hash(key)` value to the expected bucket id.
const fn bucket_id_from_hash(hash: u64) -> u32 {
    (hash % NUM_KV_BUCKETS + NUM_META_BUCKETS_U64) as u32
}

/// `len <= 8` (small branch, line 125): `"hello"` is 5 bytes.
/// `hash("hello") = 1_027_176_506_268_606_463` (documented in `external/hasher/mod.rs`).
#[test]
fn ahash_bucket_id_small_input_is_pinned() {
    assert_eq!(
        AHashBucketHasher::bucket_id(b"hello"),
        bucket_id_from_hash(1_027_176_506_268_606_463),
        "small-input (<=8B) bucket id changed: a `|`->`&` mutant on the line-125 combine alters it"
    );
}

/// `9 <= len <= 16` (mid branch, lines 120 and 109): `"hash test"` is 9 bytes.
/// `hash("hash test") = 2_116_618_212_096_523_432` (documented in `external/hasher/mod.rs`).
/// A `> 16`→`== 16` mutant on line 109 would route a 9-byte input identically, but a `>`→`==`
/// at line 109 also misroutes the boundary len 16 and all len >= 17 — covered by the large test —
/// while the `|`->`&` mutant on line 120 directly changes this 9-byte result.
#[test]
fn ahash_bucket_id_mid_input_is_pinned() {
    assert_eq!(
        AHashBucketHasher::bucket_id(b"hash test"),
        bucket_id_from_hash(2_116_618_212_096_523_432),
        "mid-input (9..=16B) bucket id changed: a `|`->`&` mutant on the line-120 combine alters it"
    );
}

/// `len >= 17` (large branch, lines 109 + 112).
///
/// `[0u8; 20]` -> bucket id `12_666_336`; the 52-byte address+slot key -> `9_450_146`; the 20-byte
/// `deadbeef` address -> `12_196_828`. All three are documented cross-check values from
/// `external/hasher/mod.rs`. At these lengths the loop `while data.len() > 16` runs at least once
/// (and three times for the 52-byte key); the `> 16`→`== 16` mutants on lines 109/112 skip or
/// misroute those reads, changing the hash.
#[test]
fn ahash_bucket_id_large_input_is_pinned() {
    // 20-byte all-zero key: single loop iteration after the tail update.
    assert_eq!(
        AHashBucketHasher::bucket_id(&[0u8; 20]),
        12_666_336,
        "large-input (20B) bucket id changed: a `> 16`->`== 16` mutant on line 109/112 alters it"
    );

    // 20-byte non-zero address: distinct branch input, documented cross-check value.
    let addr: [u8; 20] = [
        0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
        0x07, 0x08, 0x09, 0x0a, 0x0b,
    ];
    assert_eq!(AHashBucketHasher::bucket_id(&addr), 12_196_828, "20B address bucket id changed");

    // 52-byte address+slot key: the loop iterates multiple times (52 -> 36 -> 20), so the
    // line-112 `while data.len() > 16` guard is exercised across several reads.
    let mut slot_key = [0u8; 52];
    slot_key[..20].copy_from_slice(&addr);
    slot_key[51] = 1;
    assert_eq!(
        AHashBucketHasher::bucket_id(&slot_key),
        9_450_146,
        "52B slot key bucket id changed: a `> 16`->`== 16` mutant truncates the block loop"
    );
}

// ============================================================================
// external/hasher/fallback.rs:{108,112} — length-routing boundary mutants
// ============================================================================
//
// `write` routes by input length with three `>` comparisons. The full run hid the
// `>`→`>=` boundary variants because no pinned input had a length sitting exactly on a
// boundary. Each test below uses an input whose length lands on the relevant boundary so
// the `>`→`>=` flip changes which branch (or how many loop iterations) executes, and thus
// the produced hash and bucket id:
//
//   * line 108 `data.len() > 8`  : at len == 8 the original takes the small branch (read_small);
//     `>= 8` reroutes len-8 input into the mid branch (read_u64 front + read_last_u64 back), a
//     different combine.
//   * line 112 `while data.len() > 16` : with a len that is a multiple of 16 (== 32), the loop hits
//     `data.len() == 16` mid-iteration. `>` stops there (1 pass); `>= 16` runs a second read_u128
//     pass, changing the hash.
//
// The line-109 `> 16`→`>= 16` mutant is NOT here: it is provably equivalent (a len-16 input
// produces the identical single `large_update(le_u128(bytes[0..16]))` under both branches —
// the mid branch's `front | (back << 64)` reassembles the same little-endian u128 that the
// large branch's `read_last_u128` reads), so it is recorded in `mutants/suppressions.toml`.
//
// Expected bucket ids are the unmutated `AHashBucketHasher::bucket_id` outputs for these
// exact inputs (the production hasher's contract is to reproduce specific salt-compatible
// bucket ids), pinned the same way as the documented large-input cross-check values above.

/// `len == 8` boundary (line 108 `> 8`). The small branch (`read_small`) is taken; a `>= 8`
/// mutant misroutes this input into the mid branch.
#[test]
fn ahash_bucket_id_len8_boundary_is_pinned() {
    let key: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    assert_eq!(
        AHashBucketHasher::bucket_id(&key),
        1_978_187,
        "len-8 bucket id changed: a `> 8`->`>= 8` mutant on line 108 reroutes the small branch"
    );
}

/// `len == 32` boundary (line 112 `while data.len() > 16`). The loop reaches `data.len()
/// == 16` after one 16-byte read; `>` stops (one pass) while a `>= 16` mutant runs a second
/// `read_u128` pass, changing the hash.
#[test]
fn ahash_bucket_id_len32_loop_boundary_is_pinned() {
    let key: [u8; 32] = [0x22; 32];
    assert_eq!(
        AHashBucketHasher::bucket_id(&key),
        10_093_090,
        "len-32 bucket id changed: a `> 16`->`>= 16` mutant on the line-112 loop guard adds a pass"
    );
}
