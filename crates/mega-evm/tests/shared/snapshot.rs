//! The snapshot assertion the integration test targets share.
//!
//! A target includes this file once, from its `main.rs`:
//!
//! ```ignore
//! #[path = "../shared/snapshot.rs"]
//! mod snapshot;
//! ```
//!
//! and asserts with `crate::assert_sorted_json_snapshot!(value)`. The directory holds no
//! `main.rs`, so Cargo builds no target of its own from it.
//!
//! A snapshot is a net under a test's explicit assertions, not a replacement for them: the test
//! asserts the figures that matter against the schedule and the named constants first, and ends
//! with one snapshot of the whole result, which catches what no assertion names.
//!
//! The snapshot files live in `snapshots/` next to the file that asserts. A mismatch fails the
//! test; outside CI insta also writes the new value beside the old one as a `.snap.new` file,
//! which `cargo insta review` shows and accepts or rejects one by one.

/// Asserts a JSON snapshot of `value` with every map's keys sorted, so the snapshot does not
/// depend on a hasher's order. An optional first argument names the snapshot; without it insta
/// names it after the test.
///
/// The snapshots are pinned at the spec's byte prices. At other prices, which only a
/// measurement build fixes (the `satin-price-override` feature), the comparison is skipped and
/// the test leaves a note of the skip (`note_snapshot_skipped`), which the byte-price grid counts
/// apart from the price guards; the rest of the test runs as it does at the spec's prices.
#[macro_export]
macro_rules! assert_sorted_json_snapshot {
    ($value:expr $(,)?) => {
        if ::mega_evm::active_satin_prices().is_constants() {
            ::insta::with_settings!({ sort_maps => true }, {
                ::insta::assert_json_snapshot!($value);
            })
        } else {
            ::mega_evm::test_utils::note_snapshot_skipped();
        }
    };
    ($name:expr, $value:expr $(,)?) => {
        if ::mega_evm::active_satin_prices().is_constants() {
            ::insta::with_settings!({ sort_maps => true }, {
                ::insta::assert_json_snapshot!($name, $value);
            })
        } else {
            ::mega_evm::test_utils::note_snapshot_skipped();
        }
    };
}
