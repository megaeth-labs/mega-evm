//! Cross-driver contract for the block-header authentication of `mega-evme replay`.
//!
//! Every guard the replay applies to a block — the inclusion anchor, the
//! parent/block linkage, the body membership — reads the `hash` the endpoint
//! reported beside the header. That hash is a claim, not a property: an
//! endpoint, or a tampered offline capture, can rewrite the fields the replay
//! actually executes under (`timestamp`, `gasLimit`, `baseFeePerGas`,
//! `parentHash`, …), keep the original `hash`, and every one of those guards
//! still passes — while the target is replayed in a world it never ran in and
//! the run exits 0.
//!
//! A block header is a consensus object whose hash is a function of its own
//! fields, so the claim can be checked without asking the endpoint anything
//! further. These tests drive that check end to end, from outside the process,
//! over both replay drivers: each case rewrites one header field of a committed
//! capture, leaves the served `hash` alone, and requires the run to reject the
//! block naming both the hash the fields produce and the hash the endpoint
//! reported.
//!
//! The mirror image — the fields untouched and the `hash` rewritten — is one of
//! the cases in `replay_coherence.rs`, alongside the guards that consume it.
//!
//! The same fetch also authenticates the header's *height*, and the last test
//! here covers the forgery the hash cannot: a block moved onto another height's
//! request is a real, self-consistent block, and offsetting the parent's answer
//! by the same distance leaves the parent/block linkage satisfied too. The
//! single-height version of that case, over both drivers, is one row of the
//! `replay_coherence.rs` table.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{json, Value};

mod common;

use common::doctor::DoctoredEnvelope;

/// Capture of two whole `MegaETH` blocks, the only one that drives both drivers.
const ENVELOPE_NAME: &str = "replay_batch_blocks.cache.json";

/// Block of the capture every case tampers with.
const BLOCK: u64 = 22_945_844;

/// The upper of the two block pairs the same capture holds, far enough from
/// [`BLOCK`] to be an unmistakably different part of the chain. Captured
/// together with its own parent, so the offset pair is genuinely linked.
const FAR_BLOCK: u64 = 22_945_853;

/// Mid-block call of [`BLOCK`] (index 3), the target every case asks about.
const TARGET: &str = "0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef";

/// Path of the committed capture.
fn envelope() -> String {
    common::fixture(ENVELOPE_NAME).display().to_string()
}

/// One tampered header field, and the capture it produced.
struct Case {
    /// Header field the case rewrote, used in temp paths and assertion context.
    field: &'static str,
    /// The doctored capture.
    envelope: PathBuf,
    /// Hash the endpoint keeps reporting for the block.
    served: String,
    /// Hash the rewritten header actually produces.
    computed: String,
}

/// The header fields a replay reads out of a block, each rewritten to a value
/// the block never carried.
///
/// These are exactly the fields whose forgery is worth catching: `timestamp`
/// selects the spec the block executes under, `gasLimit` sets the block-level
/// resource limits, `baseFeePerGas` prices every transaction, and `parentHash`
/// names the state the replay forks from.
const TAMPERINGS: [(&str, &str); 4] = [
    ("timestamp", "0x6a704600"),
    ("gasLimit", "0x2540be401"),
    ("baseFeePerGas", "0xf4241"),
    ("parentHash", "0x2222222222222222222222222222222222222222222222222222222222222222"),
];

/// Build one doctored capture per tampered field, scoped to `prefix` so two
/// tests can hold their own copies concurrently.
fn cases(prefix: &str) -> Vec<Case> {
    let src = envelope();
    TAMPERINGS
        .iter()
        .map(|(field, value)| {
            let env = DoctoredEnvelope::load(&src).set_block_field(
                BLOCK,
                field,
                Value::String((*value).to_string()),
            );
            let served = env.block_hash_at(BLOCK);
            let computed = env.recomputed_block_hash_at(BLOCK);
            assert_ne!(
                served, computed,
                "rewriting `{field}` must change the hash the header produces, or the case \
                 proves nothing",
            );
            Case {
                field,
                envelope: env.write_to_temp(&format!("header_auth_{prefix}_{field}")),
                served,
                computed,
            }
        })
        .collect()
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

/// Assert one driver's report: the verdict names both hashes and says the header
/// does not authenticate, and nothing was executed under the forged header.
fn assert_rejected(case: &Case, driver: &str, message: &str, stdout: &str, stderr: &str) {
    let context =
        format!("case '{}' via {driver}\nstdout:\n{stdout}\nstderr:\n{stderr}", case.field);
    assert!(
        message.contains(&case.computed),
        "the verdict must name the hash the served header produces ({}).\n{context}",
        case.computed,
    );
    assert!(
        message.contains(&case.served),
        "the verdict must name the hash the endpoint reported ({}).\n{context}",
        case.served,
    );
    assert!(
        message.contains("does not authenticate"),
        "the verdict must say the header does not authenticate.\n{context}",
    );
    assert!(
        !stdout.contains("\"success\""),
        "a forged header must not produce an execution result.\n{context}",
    );
}

/// The single-transaction driver rejects a forged header of the block its target
/// was resolved into.
#[test]
fn test_single_transaction_driver_rejects_a_forged_header() {
    for case in cases("single") {
        let (stdout, stderr, exit) = replay(&case.envelope, &["--json", TARGET]);
        let values = common::json_values(&stdout);
        let error = values
            .last()
            .filter(|value| common::is_run_error(value))
            .unwrap_or_else(|| {
                panic!(
                    "case '{}': a rejected run must end with the error object\nstdout:\n{stdout}",
                    case.field
                )
            })
            .clone();

        assert_eq!(exit, Some(3), "case '{}': an unauthentic header exits 3", case.field);
        assert_eq!(
            error["error"]["kind"].as_str(),
            Some("rpc-failure"),
            "case '{}': an unauthentic header is an unanswered question",
            case.field,
        );
        assert_rejected(
            &case,
            "single-transaction replay",
            error["error"]["message"].as_str().unwrap_or_default(),
            &stdout,
            &stderr,
        );

        let _ = std::fs::remove_file(&case.envelope);
    }
}

/// The batch driver rejects the same forged headers, reporting the target as
/// unanswered rather than replaying it.
#[test]
fn test_batch_driver_rejects_a_forged_header() {
    for case in cases("batch") {
        let list = std::env::temp_dir().join(format!(
            "mega_evme_header_auth_{}_{}.txt",
            case.field,
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
                panic!(
                    "case '{}': the batch run must report the target once\nstdout:\n{stdout}",
                    case.field
                )
            });

        assert_eq!(exit, Some(3), "case '{}': an unauthentic header exits 3", case.field);
        assert_eq!(
            entry["error"]["kind"].as_str(),
            Some("rpc"),
            "case '{}': an unauthentic header leaves the target unanswered",
            case.field,
        );
        assert!(
            entry.get("success").is_none() && entry.get("receipt").is_none(),
            "case '{}': no execution result for a block that never ran: {entry}",
            case.field,
        );
        assert_rejected(
            &case,
            "batch replay",
            entry["error"]["message"].as_str().unwrap_or_default(),
            &stdout,
            &stderr,
        );

        let _ = std::fs::remove_file(&case.envelope);
    }
}

/// Whole-block mode rejects a forged header before it ever names a target: the
/// block is fetched to learn what its targets are, so the run fails as a whole
/// rather than per target.
#[test]
fn test_whole_block_mode_rejects_a_forged_header_before_listing_targets() {
    for case in cases("whole_block") {
        let (stdout, stderr, exit) =
            replay(&case.envelope, &["--block", &BLOCK.to_string(), "--json"]);
        let values = common::json_values(&stdout);
        assert_eq!(
            values.len(),
            1,
            "case '{}': only the run-level error may be printed\nstdout:\n{stdout}",
            case.field,
        );
        let error = values.last().expect("checked above");
        assert!(
            common::is_run_error(error),
            "case '{}': the only printed value must be the run-level error: {error}",
            case.field,
        );

        assert_eq!(exit, Some(3), "case '{}': an unauthentic header exits 3", case.field);
        assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
        assert_rejected(
            &case,
            "whole-block replay",
            error["error"]["message"].as_str().unwrap_or_default(),
            &stdout,
            &stderr,
        );

        let _ = std::fs::remove_file(&case.envelope);
    }
}

/// The authentication costs no request: it is a local recomputation over the
/// answer the run already has in hand.
///
/// The forged capture is stripped down to the block body alone, which is the
/// first and only answer a whole-block run needs to reach the verdict. Anything
/// the run asked for beyond it — the parent header, state, receipts — would miss
/// in the offline capture and fail with a cache-miss message instead.
#[test]
fn test_rejection_needs_no_answer_beyond_the_forged_block() {
    let src = envelope();
    let env = DoctoredEnvelope::load(&src)
        .set_block_field(BLOCK, "timestamp", json!("0x6a704600"))
        .keep_only_transactions_and_block(&[], BLOCK);
    let case = Case {
        field: "timestamp",
        served: env.block_hash_at(BLOCK),
        computed: env.recomputed_block_hash_at(BLOCK),
        envelope: env.write_to_temp("header_auth_minimal"),
    };

    let (stdout, stderr, exit) = replay(&case.envelope, &["--block", &BLOCK.to_string(), "--json"]);
    let _ = std::fs::remove_file(&case.envelope);

    assert_eq!(exit, Some(3), "an unauthentic header exits 3\nstdout:\n{stdout}");
    let values = common::json_values(&stdout);
    let error = values.last().filter(|value| common::is_run_error(value)).expect("error object");
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("cache miss"),
        "the verdict must be reached without any further answer: {message}",
    );
    assert_rejected(&case, "whole-block replay", message, &stdout, &stderr);
}

/// A whole-block run is rejected when the endpoint answers the replayed block
/// and its parent from another, equally consistent pair.
///
/// This is the forgery the hash cannot catch: nothing is rewritten, so both
/// answers hash to the hashes they are served under, and the pair really is a
/// parent and its child — so the linkage guard has nothing to say either.
/// Whole-block mode takes its targets from the served body, so the membership
/// guard is satisfied by construction as well. Without the height check the run
/// would execute [`FAR_BLOCK`]'s body under [`FAR_BLOCK`]'s environment while
/// forking the state at `BLOCK - 1` and reporting every receipt under [`BLOCK`].
#[test]
fn test_whole_block_mode_rejects_an_offset_pair_of_authentic_blocks() {
    let src = envelope();

    // Read the properties that make this case worth having before the capture is
    // doctored: both answers authenticate, and the pair is genuinely linked.
    let pristine = DoctoredEnvelope::load(&src);
    for number in [FAR_BLOCK, FAR_BLOCK - 1] {
        assert_eq!(
            pristine.block_hash_at(number),
            pristine.recomputed_block_hash_at(number),
            "block {number} must authenticate before it is moved onto another request",
        );
    }
    assert_eq!(
        pristine.block_parent_hash_at(FAR_BLOCK),
        pristine.block_hash_at(FAR_BLOCK - 1),
        "the offset pair must be genuinely linked, or the linkage guard would catch it",
    );
    drop(pristine);

    let doctored = DoctoredEnvelope::load(&src)
        .answer_block_with(BLOCK, FAR_BLOCK)
        .answer_block_with(BLOCK - 1, FAR_BLOCK - 1)
        .write_to_temp("header_auth_offset_pair");

    let (stdout, stderr, exit) = replay(&doctored, &["--block", &BLOCK.to_string(), "--json"]);
    let _ = std::fs::remove_file(&doctored);

    let context = format!("stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(exit, Some(3), "a block served for another height exits 3.\n{context}");
    let values = common::json_values(&stdout);
    assert_eq!(values.len(), 1, "only the run-level error may be printed.\n{context}");
    let error = values.last().expect("checked above");
    assert!(common::is_run_error(error), "the only printed value must be the error: {error}");
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"), "\n{context}");
    assert_eq!(
        error["error"]["message"].as_str(),
        Some(
            format!(
                "RPC error: the endpoint answered the fetch of block {BLOCK} with block \
                 {FAR_BLOCK}: a numbered fetch was served a header from another height (an \
                 inconsistent backend, or a tampered capture); the block environment it describes \
                 is not the one the run asked for"
            )
            .as_str()
        ),
        "the verdict must name the height asked for and the height served.\n{context}",
    );
    assert!(
        !stdout.contains("\"success\""),
        "a block served for another height must not produce an execution result.\n{context}",
    );
}
