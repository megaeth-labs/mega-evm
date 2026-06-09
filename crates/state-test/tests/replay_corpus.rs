//! Offline correctness guard over the committed replay-fixture corpus.
//!
//! Each fixture under `tests/fixtures/replay/` is a self-validating EEST state
//! test produced by `mega-evme replay --dump-fixture` from a real `MegaETH`
//! transaction (its `gasUsed` was checked against the on-chain receipt at dump
//! time). Re-running them through the state-test runner re-executes each
//! transaction in isolation and checks the recorded state root, logs root, gas,
//! and status — with no RPC access, so it is deterministic in CI.
//!
//! A failure here means a code change altered execution for the covered spec
//! (gas, result, or post-state). To extend coverage, dump more fixtures:
//!
//! ```bash
//! mega-evme replay --rpc <archive-url> \
//!   --dump-fixture crates/state-test/tests/fixtures/replay/<name>.json <tx-hash>
//! ```

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use state_test::runner::{execute_test_suite, find_all_json_tests};

#[test]
fn test_replay_corpus_self_validates() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay");
    let fixtures = find_all_json_tests(std::path::Path::new(dir));

    assert!(!fixtures.is_empty(), "replay corpus is empty at {dir}");

    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    let mut passed = 0usize;
    for path in &fixtures {
        execute_test_suite(path, &elapsed, false, false).unwrap_or_else(|e| {
            panic!("replay fixture {} failed to validate: {e}", path.display())
        });
        passed += 1;
    }
    assert_eq!(passed, fixtures.len(), "all corpus fixtures must validate");
}
