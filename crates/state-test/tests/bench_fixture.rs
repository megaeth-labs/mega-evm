//! Covers `bench_test_suite` — the offline fixture-benchmark path that lets any
//! self-contained state-test fixture (no RPC) be timed, used by the replay-bench
//! driver's `fixture` cases and by `state-test --bench`.

use std::path::Path;

use state_test::runner::bench_test_suite;

/// Benchmarking a self-contained corpus fixture times its isolated execution and
/// reports the same gas it validates with, using the fixture's own `post` spec.
#[test]
fn test_bench_test_suite_times_a_fixture() {
    let path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/replay/rex5_945e3fafdcb3.json"
    ));

    let results = bench_test_suite(path, 3, 1, None).expect("bench should succeed");

    assert_eq!(results.len(), 1, "one unit in the fixture");
    let r = &results[0];
    assert!(r.success, "the corpus transaction succeeds");
    assert_eq!(r.gas_used, 726_870, "gas matches the recorded fixture gas");
    assert_eq!(r.runs, 3);
    assert!(r.mgas_per_sec() > 0.0, "throughput is positive");
    assert!(r.min <= r.median, "min ≤ median");
}
