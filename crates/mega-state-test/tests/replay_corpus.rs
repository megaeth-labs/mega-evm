//! Offline correctness guard over the shared replay corpus (`bench/replay/`).
//!
//! The fixtures under `bench/replay/fixtures/` are a single corpus used two
//! ways: this test validates their recorded post-state (every PR), and the
//! replay-throughput benchmark (`bench/replay/run.py`) times them (on demand).
//! Each is a self-contained EEST state test — most produced by `mega-evme
//! replay --dump-fixture` from a real `MegaETH` transaction (gas and status
//! checked against the on-chain receipt at dump time) — so re-executing them in
//! isolation checks the state root, logs root, gas, and status with no RPC,
//! deterministically in CI. Fixtures without an on-chain origin (e.g. the
//! `attack_deploy` prestate snapshot) have their `post` filled offline with
//! `state-test --fill`, so they are validated here just like the dumped ones.
//!
//! A failure here means a code change altered execution for the covered spec
//! (gas, result, or post-state). To extend coverage, dump more fixtures:
//!
//! ```bash
//! mega-evme replay --rpc <archive-url> \
//!   --dump-fixture bench/replay/fixtures/<name>.json <tx-hash>
//! ```

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use state_test::runner::{execute_test_suite, find_all_json_tests};

#[test]
fn test_replay_corpus_self_validates() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/replay/fixtures");
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
