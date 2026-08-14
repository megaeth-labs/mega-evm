//! Cross-driver contract for the target-coherence guards of `mega-evme replay`.
//!
//! Both replay drivers ask the endpoint the same questions about a target —
//! where is it included, does that block have a parent to fork from, does the
//! fetched parent link to it, does the block body list it — and both answer a
//! self-contradictory reply with the same words. The judgments and their wording
//! live in one shared module; this file pins the result end to end, from
//! outside the process, for both drivers over one case table.
//!
//! Every case doctors a copy of the committed batch capture into exactly one
//! incoherent shape, then runs it through the single-transaction driver
//! (`replay <TX_HASH>`) and the batch driver (`replay --tx-file`). The
//! expectations are literal: the exact message, the reported failure class, and
//! the process exit code. Two rows are deliberately not symmetric and say so —
//! a target that is genuinely pending is a valid placement for the
//! single-transaction driver and a definitive answer for the batch driver — and
//! the undoctored control row proves the table's rejections come from the
//! doctoring rather than from a broken capture.
//!
//! Driver adaptation is visible on purpose: the shared verdict is the message
//! the batch driver reports per target, and the single-transaction driver
//! reports the same text behind its `RPC error: ` envelope.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

mod common;

use common::doctor::DoctoredEnvelope;

/// Capture of two whole `MegaETH` blocks, the only one that drives both drivers.
const ENVELOPE_NAME: &str = "replay_batch_blocks.cache.json";

/// Block of the capture the cases operate on.
const BLOCK: u64 = 22_945_844;

/// Mid-block call of [`BLOCK`] (index 3), the target every case asks about.
const TARGET: &str = "0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef";

/// A hash no block of the capture has, used wherever a case needs a view the
/// endpoint never served.
const FOREIGN_HASH: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";

/// Path of the committed capture.
fn envelope() -> String {
    common::fixture(ENVELOPE_NAME).display().to_string()
}

// ---------------------------------------------------------------------------
// Case table
// ---------------------------------------------------------------------------

/// What a driver must report about the target of a case.
enum Expect {
    /// The target replays: exit 0 and no error entry for it.
    Replays,
    /// The target is rejected with this exact message and failure class.
    Rejects {
        /// Full message the driver reports for the target.
        message: String,
        /// Failure class the driver labels it with.
        kind: &'static str,
        /// Process exit code of the run.
        exit: i32,
    },
    /// The shape is a valid placement for this driver, so it reaches no
    /// coherence verdict at all: the run carries on past the guards and only
    /// then fails on something naming `reached`. None of the shared coherence
    /// messages may appear.
    NoCoherenceVerdict {
        /// Substring proving the run got past the coherence guards.
        reached: &'static str,
    },
}

/// One doctored capture plus the verdict each driver must reach on it.
struct Case {
    /// Name of the scenario, used in temp paths and assertion messages.
    name: &'static str,
    /// The doctored capture this case runs against.
    envelope: PathBuf,
    /// Verdict of the single-transaction driver.
    single: Expect,
    /// Verdict of the batch driver.
    batch: Expect,
}

impl Case {
    /// A case both drivers must reject with the same shared verdict.
    ///
    /// The batch driver reports the verdict as the target's own message; the
    /// single-transaction driver reports the same text behind its `RPC error: `
    /// envelope. Both classify it as an unanswered question (exit 3).
    fn shared(name: &'static str, envelope: PathBuf, verdict: String) -> Self {
        Self {
            name,
            envelope,
            single: Expect::Rejects {
                message: format!("RPC error: {verdict}"),
                kind: "rpc-failure",
                exit: 3,
            },
            batch: Expect::Rejects { message: verdict, kind: "rpc", exit: 3 },
        }
    }
}

/// Build the case table, writing each doctored capture under a `prefix`-scoped
/// temp name so two tests can hold their own copies concurrently.
fn cases(prefix: &str) -> Vec<Case> {
    let src = envelope();
    let name = |case: &str| format!("coherence_{prefix}_{case}");

    // Values the capture itself reports, read before any doctoring.
    let pristine = DoctoredEnvelope::load(&src);
    let block_hash = pristine.listing_block_hash(TARGET);
    let inclusion_hash = pristine.transaction_str(TARGET, "blockHash");
    let parent_hash = pristine.block_hash_at(BLOCK - 1);
    drop(pristine);

    vec![
        // Control: an untouched capture replays under both drivers, so every
        // rejection below is caused by its own doctoring.
        Case {
            name: "coherent",
            envelope: DoctoredEnvelope::load(&src).write_to_temp(&name("coherent")),
            single: Expect::Replays,
            batch: Expect::Replays,
        },
        // Mined, but with no inclusion hash to anchor the replay to.
        Case::shared(
            "unanchored_view",
            DoctoredEnvelope::load(&src)
                .set_transaction_block_hash(TARGET, Value::Null)
                .write_to_temp(&name("unanchored_view")),
            format!(
                "endpoint reported a mined transaction in block {BLOCK} without an inclusion \
                 hash: unanchored view"
            ),
        ),
        // An inclusion hash paired with a null block number: the hash proves
        // inclusion while the null number denies it.
        Case::shared(
            "contradictory_metadata",
            DoctoredEnvelope::load(&src)
                .set_transaction_block_number(TARGET, Value::Null)
                .write_to_temp(&name("contradictory_metadata")),
            format!(
                "endpoint reported inclusion hash {inclusion_hash} without a block number: \
                 contradictory metadata"
            ),
        ),
        // Resolved into genesis, which has no parent to fork the pre-state from.
        Case::shared(
            "genesis_placement",
            DoctoredEnvelope::load(&src)
                .set_transaction_block_number(TARGET, Value::String("0x0".into()))
                .write_to_temp(&name("genesis_placement")),
            "endpoint resolved the target into block 0, which has no parent block to fork from: \
             contradictory endpoint data"
                .to_string(),
        ),
        // The block served at the target's height is not the block the target
        // resolved into.
        Case::shared(
            "inclusion_mismatch",
            DoctoredEnvelope::load(&src)
                .set_transaction_block_hash(TARGET, FOREIGN_HASH)
                .write_to_temp(&name("inclusion_mismatch")),
            format!(
                "block {BLOCK} has hash {block_hash}, but the target transaction was resolved as \
                 included in {FOREIGN_HASH}: the endpoint served divergent views of this block \
                 (reorg in progress, or a load-balanced endpoint); retry once the chain settles"
            ),
        ),
        // The block the target resolved into does not list it.
        Case::shared(
            "absent_from_body",
            DoctoredEnvelope::load(&src)
                .remove_from_listing_block(TARGET)
                .write_to_temp(&name("absent_from_body")),
            format!(
                "block {BLOCK} ({block_hash}) does not list target transaction {TARGET}, which \
                 the endpoint resolved as included in it: the endpoint served divergent views of \
                 this block (reorg in progress, or a load-balanced endpoint); retry once the \
                 chain settles"
            ),
        ),
        // The block served at the parent height is not the parent of the block
        // being replayed.
        Case::shared(
            "parent_linkage",
            DoctoredEnvelope::load(&src)
                .set_block_hash(BLOCK - 1, FOREIGN_HASH)
                .write_to_temp(&name("parent_linkage")),
            format!(
                "parent block hash {FOREIGN_HASH} != block parent_hash {parent_hash}: the parent \
                 block describes a different chain than the block being replayed (reorg in \
                 progress, or a load-balanced endpoint serving divergent views); retry once the \
                 chain settles"
            ),
        ),
        // Neither a block number nor an inclusion hash. This is the one shape
        // the drivers answer differently by design: it is a valid placement for
        // the single-transaction driver (which replays a pending target on top
        // of the latest block, covered end to end in `replay_pending.rs`), and a
        // definitive answer about the target for the batch driver, which reports
        // it in its own class rather than as an unanswered question.
        Case {
            name: "pending",
            envelope: DoctoredEnvelope::load(&src)
                .mark_transaction_pending(TARGET)
                .write_to_temp(&name("pending")),
            // A pending target's state base is the latest block, so the run
            // reaches the height lookup instead of a verdict; offline, that is
            // where it stops.
            single: Expect::NoCoherenceVerdict { reached: "eth_blockNumber" },
            batch: Expect::Rejects {
                message: "Transaction is pending (no block number)".to_string(),
                kind: "pending",
                exit: 1,
            },
        },
    ]
}

/// Markers that identify a shared coherence verdict in a message, used by
/// [`Expect::NoCoherenceVerdict`] to prove no such verdict was reached.
const COHERENCE_MARKERS: [&str; 5] = [
    "unanchored view",
    "contradictory metadata",
    "contradictory endpoint data",
    "divergent views",
    "does not list target transaction",
];

// ---------------------------------------------------------------------------
// Drivers
// ---------------------------------------------------------------------------

/// What a run reported about the case's target.
struct Reported {
    /// Message carried by the target's failure, if it failed.
    message: Option<String>,
    /// Failure class of that failure.
    kind: Option<String>,
    /// Process exit code.
    exit: Option<i32>,
    /// Raw stdout, for assertion context.
    stdout: String,
    /// Raw stderr, for assertion context.
    stderr: String,
}

/// Run `replay` offline against `envelope_path` with the given extra arguments.
fn replay(envelope_path: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mega-evme"));
    cmd.args(["replay", "--rpc.replay-file", envelope_path.to_str().expect("path is utf-8")]);
    cmd.args(args);
    let output = cmd.output().expect("failed to run mega-evme");
    (
        String::from_utf8(output.stdout).expect("stdout is utf-8"),
        String::from_utf8(output.stderr).expect("stderr is utf-8"),
        output.status.code(),
    )
}

/// Drive the single-transaction path and read back what it said about the target.
///
/// A single-transaction run fails as a whole, so the target's verdict is the
/// run-level error object of the `--json` output.
fn run_single(case: &Case) -> Reported {
    let (stdout, stderr, exit) = replay(&case.envelope, &["--json", TARGET]);
    let values = common::json_values(&stdout);
    let error = values.last().filter(|value| common::is_run_error(value)).cloned();
    let (message, kind) = match &error {
        Some(error) => (
            error["error"]["message"].as_str().map(str::to_string),
            error["error"]["kind"].as_str().map(str::to_string),
        ),
        None => (None, None),
    };
    Reported { message, kind, exit, stdout, stderr }
}

/// Drive the batch path over a one-entry `--tx-file` and read back the target's
/// NDJSON entry.
fn run_batch(case: &Case) -> Reported {
    let list = std::env::temp_dir().join(format!(
        "mega_evme_coherence_{}_{}.txt",
        case.name,
        std::process::id()
    ));
    std::fs::write(&list, format!("{TARGET}\n")).expect("write tx list");
    let (stdout, stderr, exit) =
        replay(&case.envelope, &["--tx-file", list.to_str().expect("path is utf-8"), "--json"]);
    let _ = std::fs::remove_file(&list);

    let entry = common::json_values(&stdout)
        .into_iter()
        .filter(|value| !common::is_run_error(value))
        .find(|value| value["tx_hash"].as_str() == Some(TARGET))
        .unwrap_or_else(|| {
            panic!("the batch run must report the target once:\n{stdout}\nstderr:\n{stderr}")
        });
    let message = entry["error"]["message"].as_str().map(str::to_string);
    let kind = entry["error"]["kind"].as_str().map(str::to_string);
    Reported { message, kind, exit, stdout, stderr }
}

/// Assert one driver's report against the case's expectation.
fn assert_reported(case: &Case, driver: &str, expect: &Expect, reported: &Reported) {
    let context = format!(
        "case '{}' via {driver}\nstdout:\n{}\nstderr:\n{}",
        case.name, reported.stdout, reported.stderr
    );
    match expect {
        Expect::Replays => {
            assert_eq!(reported.exit, Some(0), "the target must replay cleanly.\n{context}");
            assert_eq!(reported.message, None, "a replayed target has no error.\n{context}");
        }
        Expect::Rejects { message, kind, exit } => {
            assert_eq!(
                reported.message.as_deref(),
                Some(message.as_str()),
                "unexpected message.\n{context}"
            );
            assert_eq!(reported.kind.as_deref(), Some(*kind), "unexpected class.\n{context}");
            assert_eq!(reported.exit, Some(*exit), "unexpected exit code.\n{context}");
        }
        Expect::NoCoherenceVerdict { reached } => {
            let message = reported.message.clone().unwrap_or_default();
            for marker in COHERENCE_MARKERS {
                assert!(
                    !message.contains(marker),
                    "the shape is a valid placement here, so no coherence verdict may be \
                     reported, but the message carries '{marker}'.\n{context}"
                );
            }
            assert!(
                message.contains(reached),
                "the run must carry on past the coherence guards, up to '{reached}'.\n{context}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The single-transaction driver's verdict on every case of the table.
#[test]
fn test_single_transaction_driver_coherence_verdicts() {
    for case in cases("single") {
        let reported = run_single(&case);
        assert_reported(&case, "single-transaction replay", &case.single, &reported);
        let _ = std::fs::remove_file(&case.envelope);
    }
}

/// The batch driver's verdict on the same cases, over the same doctored shapes.
#[test]
fn test_batch_driver_coherence_verdicts() {
    for case in cases("batch") {
        let reported = run_batch(&case);
        assert_reported(&case, "batch replay", &case.batch, &reported);
        let _ = std::fs::remove_file(&case.envelope);
    }
}
