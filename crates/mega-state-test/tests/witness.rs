//! Fixtures replayed from the witness of their own execution: a fixture written here, always,
//! and a sample of the execution-spec fixtures on demand.
//!
//! The sample needs the fixtures the gate downloads. Point `MEGA_STATE_TEST_FIXTURES` at a
//! `state_tests` directory and run the ignored test:
//!
//! ```text
//! MEGA_STATE_TEST_FIXTURES=fixtures/main/state_tests cargo test -p mega-state-test --release \
//!     --test witness -- --ignored
//! ```
//!
//! `MEGA_STATE_TEST_SAMPLE` is how many fixture files to take, spread over the tree (300 unless
//! set; 0 for all of them), `MEGA_STATE_TEST_FORK` the fork whose entries run (`Osaka` unless
//! set) and `MEGA_STATE_TEST_MODE` the mode (`satin` unless set).

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
};

use state_test::{runner::find_json_files, types::TestSuite, witness::check_replay, Fork, Mode};

/// A fixture of the execution-spec shape: a call to a contract that writes one slot, expected to
/// execute. The post-state hashes are the fixture's own and are not judged here.
const FIXTURE: &str = r#"{
  "a call that writes a slot": {
    "env": {
      "currentCoinbase": "0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba",
      "currentGasLimit": "0x07270e00",
      "currentNumber": "0x01",
      "currentTimestamp": "0x03e8",
      "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000000000",
      "currentDifficulty": "0x00",
      "currentBaseFee": "0x07",
      "currentExcessBlobGas": "0x00"
    },
    "pre": {
      "0x42b24e5d48846699cab68f8318fdff2110dd6d8e": {
        "nonce": "0x01",
        "balance": "0x00",
        "code": "0x600160005500",
        "storage": {}
      },
      "0x9a15735b9b881d3856d5e62a71ecde96976a8443": {
        "nonce": "0x00",
        "balance": "0x3635c9adc5dea00000",
        "code": "0x",
        "storage": {}
      }
    },
    "transaction": {
      "nonce": "0x00",
      "maxPriorityFeePerGas": "0x00",
      "maxFeePerGas": "0x07",
      "gasLimit": ["0x01000000"],
      "to": "0x42b24e5d48846699cab68f8318fdff2110dd6d8e",
      "value": ["0x00"],
      "data": ["0x"],
      "accessLists": [[]],
      "sender": "0x9a15735b9b881d3856d5e62a71ecde96976a8443",
      "secretKey": "0xcd44b7e62d9916d0ee4baf3f42b24e5d48846699cab68f8318fdff2110dd6d8e"
    },
    "post": {
      "Osaka": [
        {
          "hash": "0x6b4267d9ce94c1314036f184414bb7e814651dc524bd20520f7dc41bb22de73e",
          "logs": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
          "txbytes": "0x",
          "indexes": { "data": 0, "gas": 0, "value": 0 }
        }
      ]
    },
    "config": {
      "blobSchedule": {
        "Osaka": { "target": "0x06", "max": "0x09", "baseFeeUpdateFraction": "0x4c6964" }
      },
      "chainid": "0x01"
    }
  }
}"#;

/// The fixture above replays from its witness in both modes.
#[test]
fn test_a_fixture_replays_from_its_witness() {
    let suite: TestSuite = serde_json::from_str(FIXTURE).expect("a fixture");
    let (_, unit) = suite.0.iter().next().expect("one test");
    let tests = unit.post.iter().find(|(spec, _)| Fork::Osaka.is(spec)).expect("Osaka").1;
    for mode in Mode::ALL {
        for test in tests {
            assert_eq!(check_replay(mode, Fork::Osaka, unit, test), Ok(None), "{mode}");
        }
    }
}

/// A sample of the execution-spec fixtures replays from its witness. Ignored: it needs the
/// fixtures, named by `MEGA_STATE_TEST_FIXTURES`.
#[test]
#[ignore = "needs the execution-spec fixtures: set MEGA_STATE_TEST_FIXTURES to a state_tests directory"]
fn test_a_sample_of_the_fixtures_replays_from_its_witness() {
    let root = PathBuf::from(
        std::env::var("MEGA_STATE_TEST_FIXTURES")
            .expect("MEGA_STATE_TEST_FIXTURES names the fixtures"),
    );
    let sample: usize =
        std::env::var("MEGA_STATE_TEST_SAMPLE").map_or(300, |n| n.parse().expect("a count"));
    let fork: Fork =
        std::env::var("MEGA_STATE_TEST_FORK").map_or(Fork::Osaka, |f| f.parse().expect("a fork"));
    let mode: Mode =
        std::env::var("MEGA_STATE_TEST_MODE").map_or(Mode::Satin, |m| m.parse().expect("a mode"));

    let files = find_json_files(&root);
    assert!(!files.is_empty(), "no fixtures under {}", root.display());
    let stride = files.len().checked_div(sample).unwrap_or(1).max(1);
    let sampled: Vec<_> =
        files.iter().step_by(stride).take(if sample == 0 { usize::MAX } else { sample }).collect();

    let (mut executed, mut skipped, mut failed) = (0, 0, Vec::new());
    for path in &sampled {
        let json = std::fs::read_to_string(path).expect("a readable fixture");
        let suite: TestSuite = match serde_json::from_str(&json) {
            Ok(suite) => suite,
            Err(error) => {
                failed.push(format!("{}: parse: {error}", path.display()));
                continue;
            }
        };
        for (name, unit) in &suite.0 {
            for (spec, tests) in &unit.post {
                if !fork.is(spec) {
                    continue;
                }
                for (entry, test) in tests.iter().enumerate() {
                    let outcome =
                        catch_unwind(AssertUnwindSafe(|| check_replay(mode, fork, unit, test)));
                    match outcome {
                        Ok(Ok(None)) => executed += 1,
                        Ok(Ok(Some(_))) => skipped += 1,
                        Ok(Err(why)) => {
                            failed.push(format!("{}::{name}[{entry}]: {why}", path.display()))
                        }
                        Err(_) => {
                            failed.push(format!("{}::{name}[{entry}]: panicked", path.display()))
                        }
                    }
                }
            }
        }
    }
    eprintln!(
        "witness replay of {} fixture files in {mode} mode on {fork}: {executed} replayed, {skipped} skipped, {} failed",
        sampled.len(),
        failed.len()
    );
    for failure in failed.iter().take(50) {
        eprintln!("  {failure}");
    }
    assert!(failed.is_empty(), "{} entries did not replay from their witness", failed.len());
    assert!(executed > 0, "nothing executed");
}
