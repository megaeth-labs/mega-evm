//! Behavior-equivalence baseline for `mega-evme replay`.
//!
//! `replay` reaches the EVM through two drivers — the single-transaction path
//! and the batch path — and prints through two renderers (human and `--json`).
//! Refactoring those drivers onto a shared kernel must not change one byte of
//! what a user sees. This file makes that machine-checkable: it runs an argv
//! matrix against the committed offline RPC captures, normalizes the few things
//! that legitimately differ between runs (measured wall-clock timings, absolute
//! paths), and compares the result against a golden snapshot committed under
//! `tests/golden/`. A refactor is behavior-preserving exactly when every
//! snapshot still matches.
//!
//! A snapshot pins the exit code, stdout, the whole of stderr, and the content
//! of any file the run was asked to write — a fixture dump is part of the
//! observable behavior, and a change there is invisible on stdout. stderr is
//! pinned in full rather than filtered down to its `error:` lines: a failure
//! message may carry continuation lines (an RPC error's re-capture `hint:`),
//! and dropping them would let a rewrite of the hint through the gate.
//!
//! Rows that need a failure the committed captures do not hold manufacture it
//! by doctoring a copy of a capture through the shared typed doctor
//! ([`common::doctor`]). The normalization rules stay local to this file: a
//! snapshot is only as trustworthy as the rules that produced it, and those
//! rules should be readable without following helpers into another module.
//!
//! # Regenerating
//!
//! ```bash
//! MEGA_EVME_BLESS=1 cargo test -p mega-evme --test replay_equivalence
//! ```
//!
//! Blessing is not an approval. The discipline is:
//!
//! 1. Run the command above, which rewrites every golden.
//! 2. Run the suite again *without* `MEGA_EVME_BLESS` and require it green.
//! 3. Read `git diff` line by line and accept each change deliberately.
//!
//! Steps 2 and 3 are the reviewer's job, not the harness's, so the harness
//! makes them as hard as possible to need: the invariant checks and the
//! negative controls run under `MEGA_EVME_BLESS` exactly as they do without it,
//! and every blessed golden is immediately re-derived from a second independent
//! run and compared, so run-to-run nondeterminism fails at the bless rather
//! than becoming a flaky golden.
//!
//! # Declared limits
//!
//! - **Windows paths are not normalized.** Path substitution matches the native separator, and a
//!   JSON-serialized Windows path carries doubled backslashes, so a golden blessed on Windows would
//!   not match one blessed on Unix. This repository has no Windows CI; the gate is a Unix gate.
//! - **Tracing, state dumps and `--override.spec` are not in the matrix.** Trace output is large
//!   enough to swamp the goldens, and the override rows need an online mock rather than an offline
//!   capture. Those flags keep their structured coverage in `replay_override_spec.rs` and the
//!   `run`/`tx` tests.
//! - **Fixture failure branches beyond the two pinned here** — a destination that already exists,
//!   and the fidelity gate refusing a halted replay — stay with the structured tests
//!   (`replay_dump.rs`). An unanswered receipt for the gate, a mid-flight write failure, and a
//!   draft discarded by a block abort are asserted there, not byte-pinned here.

mod common;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};

use tempfile::TempDir;

use common::{doctor::DoctoredEnvelope, is_run_error, json_values};

// ---------------------------------------------------------------------------
// Captures and their transactions
// ---------------------------------------------------------------------------

/// Capture of two whole `MegaETH` blocks (bodies, state and on-chain receipts).
/// The only capture that can drive the batch paths.
const CAPTURE_BLOCKS: &str = "replay_batch_blocks.cache.json";

/// Block of [`CAPTURE_BLOCKS`] replayed whole: 23 transactions, index 0 being a
/// deposit.
const BLOCK: u64 = 22_945_844;

/// Number of transactions [`BLOCK`] holds, and therefore the number of result
/// lines a whole-block batch run reports.
const BLOCK_TX_COUNT: usize = 23;

/// Index-0 deposit of [`BLOCK`]. Deposits are the one transaction class
/// `--dump-fixture` refuses, so this target covers the skip branch.
const BLOCK_DEPOSIT_TX: &str = "0xde3d56dc739484166b8af1bea757bf7e3e9a4b9a0fb62d722703345570dfc1d6";

/// Mid-block call of [`BLOCK`] (index 3). Small enough that its dumped fixture
/// stays reviewable, and the transaction both dump rows share, so the
/// single-file and per-target dumps can be compared against each other.
const BLOCK_MID_TX: &str = "0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef";

/// Last transaction of [`BLOCK`] (index 22).
const BLOCK_LAST_TX: &str = "0xb6a0b7a302c741f64b8e46861a3dcb2d5c1047f6f2cb89a35b5c2183c96296b7";

/// Transaction of the capture's second block, so a `--tx-file` run has to group
/// targets across blocks.
const OTHER_BLOCK_TX: &str = "0x18302160f2395069a44e1654d173fa9eed95ead8f922f12bfe07b6bdcc0a14f2";

/// Single-transaction capture that also carries the on-chain receipt.
const CAPTURE_SINGLE: &str = "replay_offline.cache.json";

/// The transaction [`CAPTURE_SINGLE`] answers: a 75,514-gas Rex5 mainnet call.
const SINGLE_TX: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// Single-transaction capture of a deposit, without an on-chain receipt.
const CAPTURE_SMOKE: &str = "replay_offline_smoke.cache.json";

/// The deposit [`CAPTURE_SMOKE`] answers.
const SMOKE_TX: &str = "0xf0ac58ac093e718e12fb9d00bb79c28c335343f79d8cb4ab804a5e1b6c5c4adf";

/// Single-transaction capture whose envelope carries a non-empty external env
/// (SALT bucket capacities), so its replay exercises dynamic storage pricing.
const CAPTURE_BUCKET_CAP: &str = "replay_offline_bucket_cap.cache.json";

/// The transaction [`CAPTURE_BUCKET_CAP`] answers: a 9-log call.
const BUCKET_CAP_TX: &str = "0x346c9ecd95ea9502e62534b68d8797f592d5f20bf7ed2d7b88f6ca7970e46919";

/// Capture of three mainnet CREATEs that halt, with their on-chain receipts.
const CAPTURE_HALT: &str = "halt_logs_repro.cache.json";

/// One of the halting CREATEs. The other two replay through the same code with
/// the same shape, so one is enough for an equivalence baseline; all three stay
/// pinned against the chain by `replay_halt_logs.rs`.
const HALT_TX: &str = "0x002ecbc328e5259b3756b69a221fc7ff7956dd616a9d872eda1701914bb6f3cc";

// ---------------------------------------------------------------------------
// Normalization placeholders
// ---------------------------------------------------------------------------

/// Stands for the offline capture the run replayed from.
const CAPTURE_PATH: &str = "<CAPTURE>";

/// Stands for the `--dump-fixture` destination.
const DUMP_FIXTURE_PATH: &str = "<DUMP_FIXTURE>";

/// Stands for the `--dump-fixture-dir` destination.
const DUMP_DIR_PATH: &str = "<DUMP_DIR>";

/// Stands for the `--tx-file` target list.
const TX_FILE_PATH: &str = "<TX_FILE>";

/// Stands for the per-case scratch directory. A catch-all: the specific
/// placeholders above cover every path the harness passes on the command line,
/// so anything still matching this leaked out some other way.
const SCRATCH_PATH: &str = "<SCRATCH>";

/// Stands for the directory extracted captures live in.
const EXTRACTED_PATH: &str = "<EXTRACTED>";

/// Stands for this crate's source directory.
const CRATE_PATH: &str = "<CRATE>";

/// Stands for a measured execution time.
const DURATION: &str = "<DURATION>";

/// Label of the one human-output field that measures the machine rather than
/// the transaction.
const DURATION_LABEL: &str = "Execution Time:";

// ---------------------------------------------------------------------------
// Fixture resolution
// ---------------------------------------------------------------------------

/// Resolve a capture in `tests/fixtures/` by name, extracting it if it is
/// stored compressed.
///
/// A fixture is either the file itself, or a `<name>.tar.gz` holding exactly
/// that one file. Archives are extracted once per test binary into a temporary
/// directory that lives for the whole run, so the committed fixtures are never
/// written to. Extraction shells out to `tar`: every platform that runs these
/// tests has one.
fn fixture(name: &str) -> PathBuf {
    let plain = fixtures_dir().join(name);
    if plain.is_file() {
        return plain;
    }

    let archive = fixtures_dir().join(format!("{name}.tar.gz"));
    assert!(
        archive.is_file(),
        "no fixture named {name}: neither {} nor {} exists",
        plain.display(),
        archive.display(),
    );

    static EXTRACTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut extracted =
        EXTRACTED.get_or_init(|| Mutex::new(HashSet::new())).lock().expect("fixture lock");

    let root = extracted_root();
    let path = root.join(name);
    if extracted.insert(name.to_string()) {
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(root)
            .status()
            .expect("failed to run tar");
        assert!(status.success(), "failed to extract {}", archive.display());
        assert!(path.is_file(), "{} does not contain {name}", archive.display());
    }
    path
}

/// Directory the committed captures live in.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Directory compressed captures are extracted into, shared by every test in
/// this binary and removed when the process exits.
fn extracted_root() -> &'static Path {
    static ROOT: OnceLock<TempDir> = OnceLock::new();
    ROOT.get_or_init(|| tempfile::tempdir().expect("failed to create a temp dir for captures"))
        .path()
}

/// Directory the golden snapshots are committed in.
fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Whether this run regenerates the goldens instead of checking them.
fn blessing() -> bool {
    std::env::var("MEGA_EVME_BLESS").is_ok_and(|value| value == "1")
}

// ---------------------------------------------------------------------------
// Matrix rows
// ---------------------------------------------------------------------------

/// A rewrite applied to a copy of a row's capture before the run, manufacturing
/// a failure the committed captures do not hold.
///
/// Takes the committed capture and the scratch directory; returns the path of
/// the doctored copy the run replays from.
type Doctoring = fn(&Path, &Path) -> PathBuf;

/// What a run must satisfy whatever its golden says.
///
/// These are the checks that stay meaningful while the goldens are being
/// rewritten, so they are what stands between `MEGA_EVME_BLESS=1` and a
/// laundered regression. They are deliberately structural — an exit class, a
/// line count, a file count — rather than a second copy of the golden.
#[derive(Default)]
struct Expect {
    /// Exit code the row must produce. A row that declares nothing must succeed.
    exit: i32,
    /// Values a `--json` run must print that report an execution result.
    ///
    /// Pins the keep-result contract without going through the golden: a target
    /// that replayed keeps its result line even when its receipt verification
    /// or its fixture dump failed, rather than being replaced by an error line.
    result_lines: Option<usize>,
    /// Values a `--json` run must print that report a per-target failure
    /// instead of a result (`{"tx_hash":…,"error":{…}}`).
    error_lines: Option<usize>,
    /// Whether the row's `--dump-fixture` destination must exist afterwards.
    dump_written: Option<bool>,
    /// Files the row's `--dump-fixture-dir` destination must hold afterwards.
    dir_files: Option<usize>,
}

/// One row of the argv matrix, resolved against a scratch directory.
///
/// A row is output-mode agnostic: `--json` is appended by the runner so the two
/// renderers see exactly the same command line otherwise.
struct Row {
    /// Capture to replay from, by fixture name.
    capture: &'static str,
    /// Rewrite applied to a copy of that capture before the run.
    doctoring: Option<Doctoring>,
    /// Arguments after `replay --rpc.replay-file <capture>`.
    args: Vec<String>,
    /// Files the row writes for the run to read. Echoed into the snapshot, so a
    /// golden says which targets produced it rather than pointing at a
    /// temporary file that no longer exists.
    inputs: Vec<(&'static str, String)>,
    /// Files the run is expected to write. Their content joins the snapshot.
    artifacts: Vec<Artifact>,
    /// Absolute paths this row puts on the command line, with the placeholder
    /// each is rewritten to before comparison.
    paths: Vec<(PathBuf, &'static str)>,
    /// Golden-independent invariants of this row.
    expect: Expect,
}

/// A file, or a directory of files, whose content joins the snapshot.
enum Artifact {
    /// One file at a known path.
    File(&'static str, PathBuf),
    /// Every file in a directory, in name order.
    Dir(&'static str, PathBuf),
}

/// Content the harness seeds a dump destination with, to be refused or replaced.
///
/// Deliberately not a valid fixture: if the run ever mistook it for one, the
/// snapshot would say so.
const SEEDED_FIXTURE: &str = "{\"note\":\"a fixture was already written at this path\"}\n";

impl Row {
    /// A row that only replays, writing nothing.
    fn new(capture: &'static str, args: &[&str]) -> Self {
        Self {
            capture,
            doctoring: None,
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            inputs: Vec::new(),
            artifacts: Vec::new(),
            paths: Vec::new(),
            expect: Expect::default(),
        }
    }

    /// Replay from a doctored copy of the capture rather than the committed one.
    fn doctored(mut self, doctoring: Doctoring) -> Self {
        self.doctoring = Some(doctoring);
        self
    }

    /// Declare the exit code this row must produce.
    fn exits(mut self, code: i32) -> Self {
        self.expect.exit = code;
        self
    }

    /// Declare how many result values a `--json` run of this row must print.
    fn result_lines(mut self, count: usize) -> Self {
        self.expect.result_lines = Some(count);
        self
    }

    /// Declare how many per-target error values a `--json` run must print.
    fn error_lines(mut self, count: usize) -> Self {
        self.expect.error_lines = Some(count);
        self
    }

    /// Declare whether the `--dump-fixture` destination must exist afterwards.
    fn dump_written(mut self, written: bool) -> Self {
        self.expect.dump_written = Some(written);
        self
    }

    /// Declare how many files the `--dump-fixture-dir` must hold afterwards.
    fn dir_files(mut self, count: usize) -> Self {
        self.expect.dir_files = Some(count);
        self
    }

    /// Add `--tx-file`, writing `hashes` into the scratch directory.
    ///
    /// The list is deliberately written in the shapes the parser has to
    /// tolerate — a comment, a blank line, indentation, an out-of-order entry
    /// and a duplicate — so the snapshot also pins how the parser resolves
    /// them.
    fn with_tx_file(mut self, scratch: &Path, hashes: &[&str]) -> Self {
        let mut body = String::from("# equivalence corpus\n");
        for (index, hash) in hashes.iter().enumerate() {
            match index {
                0 => body.push_str(&format!("{hash}\n\n")),
                1 => body.push_str(&format!("  {hash}\n")),
                _ => body.push_str(&format!("{hash}\n")),
            }
        }
        // Repeat the last target: duplicates are dropped, not replayed twice.
        if let Some(last) = hashes.last() {
            body.push_str(&format!("{last}\n"));
        }

        let path = scratch.join("targets.txt");
        fs::write(&path, &body).expect("failed to write the target list");
        self.args.push("--tx-file".into());
        self.args.push(display(&path));
        self.inputs.push((TX_FILE_PATH, body));
        self.paths.push((path, TX_FILE_PATH));
        self
    }

    /// Add `--dump-fixture`, pointed at a path in the scratch directory.
    fn with_dump_fixture(mut self, scratch: &Path) -> Self {
        let path = scratch.join("fixture.json");
        self.args.push("--dump-fixture".into());
        self.args.push(display(&path));
        self.paths.push((path.clone(), DUMP_FIXTURE_PATH));
        self.artifacts.push(Artifact::File(DUMP_FIXTURE_PATH, path));
        self
    }

    /// Add `--dump-fixture-dir`, pointed at a directory in the scratch
    /// directory. The directory is created first, the way a caller would.
    fn with_dump_fixture_dir(mut self, scratch: &Path) -> Self {
        let path = dump_dir(scratch);
        fs::create_dir_all(&path).expect("failed to create the fixture directory");
        self.args.push("--dump-fixture-dir".into());
        self.args.push(display(&path));
        self.paths.push((path.clone(), DUMP_DIR_PATH));
        self.artifacts.push(Artifact::Dir(DUMP_DIR_PATH, path));
        self
    }

    /// Seed the `--dump-fixture-dir` destination with a file where `tx_hash`'s
    /// dump would land, so the run has something to refuse or to replace.
    fn with_seeded_fixture(self, scratch: &Path, tx_hash: &str) -> Self {
        let dir = dump_dir(scratch);
        fs::create_dir_all(&dir).expect("failed to create the fixture directory");
        fs::write(dir.join(format!("{tx_hash}.json")), SEEDED_FIXTURE)
            .expect("failed to seed the fixture destination");
        self
    }

    /// Command-line arguments of this row in one output mode.
    fn argv(&self, json: bool) -> Vec<String> {
        let mut args = self.args.clone();
        if json {
            args.push("--json".into());
        }
        args
    }

    /// The capture this row replays from, doctored into `scratch` if it asked
    /// for a manufactured failure.
    fn capture_path(&self, scratch: &Path) -> PathBuf {
        let source = fixture(self.capture);
        match self.doctoring {
            Some(doctoring) => doctoring(&source, scratch),
            None => source,
        }
    }

    /// Whether this row drives the batch path.
    fn is_batch(&self) -> bool {
        self.args.iter().any(|arg| arg == "--tx-file" || arg == "--block")
    }
}

/// The `--dump-fixture-dir` destination of a scratch directory.
fn dump_dir(scratch: &Path) -> PathBuf {
    scratch.join("fixtures")
}

/// Replay one mined transaction: the baseline of the single-transaction path.
fn row_single_offline_mined(_scratch: &Path) -> Row {
    Row::new(CAPTURE_SINGLE, &[SINGLE_TX])
}

/// Verify one mined transaction against the on-chain receipt the capture holds.
fn row_single_offline_verify(_scratch: &Path) -> Row {
    Row::new(CAPTURE_SINGLE, &["--verify-receipt", SINGLE_TX])
}

/// Verify a replay against a receipt it cannot reproduce.
///
/// The one row that reaches exit `2`. `verification-mismatch` is the code a
/// verification pipeline branches on, and nothing else in the matrix produces
/// it: the capture's receipt is rewritten to advertise a gas usage the replay
/// will not match, which is a divergence rather than an unanswered question.
fn row_single_verify_mismatch(_scratch: &Path) -> Row {
    Row::new(CAPTURE_SINGLE, &["--verify-receipt", SINGLE_TX])
        .doctored(doctor_receipt_gas_used)
        .exits(2)
}

/// Verify and dump in the same run: the two single-transaction post-processing
/// steps interact through the same replay result, and each is normally pinned
/// alone.
fn row_single_verify_dump(scratch: &Path) -> Row {
    Row::new(CAPTURE_SINGLE, &["--verify-receipt", SINGLE_TX])
        .with_dump_fixture(scratch)
        .dump_written(true)
}

/// Replay a deposit: a different transaction type, receipt shape and fee path.
fn row_single_smoke_deposit(_scratch: &Path) -> Row {
    Row::new(CAPTURE_SMOKE, &[SMOKE_TX])
}

/// Ask for verification against a capture that holds no receipt.
///
/// Not a gap in the matrix but a pinned failure: the run classifies the missing
/// answer as an infrastructure failure rather than a mismatch, and that
/// classification is exactly the kind of thing a driver refactor can move.
fn row_single_smoke_verify_unavailable(_scratch: &Path) -> Row {
    Row::new(CAPTURE_SMOKE, &["--verify-receipt", SMOKE_TX]).exits(3)
}

/// Replay against a capture with a non-empty external env, so the snapshot
/// covers SALT-priced storage.
fn row_single_bucket_cap_mined(_scratch: &Path) -> Row {
    Row::new(CAPTURE_BUCKET_CAP, &[BUCKET_CAP_TX])
}

/// Replay a transaction that halts: the failure arm of the human summary and of
/// the JSON one.
fn row_single_halt_mined(_scratch: &Path) -> Row {
    Row::new(CAPTURE_HALT, &[HALT_TX])
}

/// Verify a halted transaction against its on-chain receipt.
fn row_single_halt_verify(_scratch: &Path) -> Row {
    Row::new(CAPTURE_HALT, &["--verify-receipt", HALT_TX])
}

/// Ask for a fixture dump of a halted transaction.
///
/// A second pinned failure: the fixture's isolated re-execution does not
/// reproduce the halt, so the dump is refused and no file is written. The
/// snapshot records both the refusal and the absent artifact.
fn row_single_halt_dump_refused(scratch: &Path) -> Row {
    Row::new(CAPTURE_HALT, &[HALT_TX]).with_dump_fixture(scratch).exits(1).dump_written(false)
}

/// Replay one transaction of the batch capture through the single-transaction
/// path. Paired with `batch_block`, which replays the same transaction through
/// the batch path, so the two drivers stay comparable.
fn row_single_block_tx_mined(_scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &[BLOCK_MID_TX])
}

/// Verify that same transaction against its on-chain receipt.
fn row_single_block_tx_verify(_scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &["--verify-receipt", BLOCK_MID_TX])
}

/// Dump a fixture for that same transaction through the single-file writer.
fn row_single_block_tx_dump(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &[BLOCK_MID_TX]).with_dump_fixture(scratch).dump_written(true)
}

/// Replay a whole block: every transaction of `BLOCK` in order.
fn row_batch_block(_scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &["--block", &BLOCK.to_string()])
        .result_lines(BLOCK_TX_COUNT)
        .error_lines(0)
}

/// Verify a whole block against the on-chain receipts: the batch driver's two
/// entry points differ in how they resolve targets, so `--block` needs its own
/// verification row rather than inheriting `--tx-file`'s.
fn row_batch_block_verify(_scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &["--block", &BLOCK.to_string(), "--verify-receipt"])
        .result_lines(BLOCK_TX_COUNT)
        .error_lines(0)
}

/// Replay a target subset spanning both blocks of the capture.
fn row_batch_tx_file(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &[])
        .with_tx_file(scratch, &[OTHER_BLOCK_TX, BLOCK_LAST_TX, BLOCK_DEPOSIT_TX, BLOCK_MID_TX])
        .result_lines(4)
        .error_lines(0)
}

/// Verify that subset against the on-chain receipts: one verdict per target.
fn row_batch_tx_file_verify(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &["--verify-receipt"])
        .with_tx_file(scratch, &[OTHER_BLOCK_TX, BLOCK_LAST_TX, BLOCK_DEPOSIT_TX, BLOCK_MID_TX])
        .result_lines(4)
        .error_lines(0)
}

/// Replay a target list in which two targets never resolve and two do.
///
/// The ordering contract of a batch `--json` run is not derivable from the
/// clean rows: unresolved targets are emitted first in the order they were
/// requested, then each block's results in transaction-index order, and the
/// run-level error object last. This row pins all three at once, together with
/// the exit precedence — a `not_found` target is execution-class and outranks
/// the `rpc` one, so the run exits `1` rather than `3`.
fn row_batch_mixed_failures(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &[])
        .doctored(doctor_batch_mixed)
        .with_tx_file(scratch, &[OTHER_BLOCK_TX, BLOCK_LAST_TX, BLOCK_DEPOSIT_TX, BLOCK_MID_TX])
        .exits(1)
        .result_lines(2)
        .error_lines(2)
}

/// Verify a batch target whose receipt the endpoint no longer answers.
///
/// The keep-result contract: the target replayed, so its result line stays and
/// carries the failure under `verification.error` instead of being replaced by
/// an error line. An unanswered receipt is rpc-class, so the run exits `3`.
fn row_batch_verify_unavailable(scratch: &Path) -> Row {
    Row::new(CAPTURE_SINGLE, &["--verify-receipt"])
        .doctored(doctor_null_receipt)
        .with_tx_file(scratch, &[SINGLE_TX])
        .exits(3)
        .result_lines(1)
        .error_lines(0)
}

/// Dump per-target fixtures for a two-target subset: the deposit is skipped,
/// the call is written.
fn row_batch_tx_file_dump_dir(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &[])
        .with_tx_file(scratch, &[BLOCK_DEPOSIT_TX, BLOCK_MID_TX])
        .with_dump_fixture_dir(scratch)
        .result_lines(2)
        .error_lines(0)
        .dir_files(1)
}

/// Dump into a directory that already holds one of the destination files.
///
/// The other half of keep-result: the target replayed and keeps its line, the
/// refusal lands in `fixture.error`, and the seeded file is left byte-for-byte
/// alone — which the artifact section is what proves.
fn row_batch_dump_dir_refused(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &[])
        .with_tx_file(scratch, &[BLOCK_DEPOSIT_TX, BLOCK_MID_TX])
        .with_dump_fixture_dir(scratch)
        .with_seeded_fixture(scratch, BLOCK_MID_TX)
        .exits(1)
        .result_lines(2)
        .error_lines(0)
        .dir_files(1)
}

/// The same run with `--overwrite`: the seeded file is replaced and the run
/// succeeds. Paired with `batch_dump_dir_refused`, the two rows pin both sides
/// of the flag.
fn row_batch_dump_dir_overwrite(scratch: &Path) -> Row {
    Row::new(CAPTURE_BLOCKS, &["--overwrite"])
        .with_tx_file(scratch, &[BLOCK_DEPOSIT_TX, BLOCK_MID_TX])
        .with_dump_fixture_dir(scratch)
        .with_seeded_fixture(scratch, BLOCK_MID_TX)
        .result_lines(2)
        .error_lines(0)
        .dir_files(1)
}

// ---------------------------------------------------------------------------
// Doctored captures
// ---------------------------------------------------------------------------

/// Apply `rewrite` to a copy of `source` and place the result in `scratch`.
///
/// The doctored capture lives beside the row's other scratch inputs so it is
/// removed with them, and so its path normalizes to `<CAPTURE>` like any other.
/// The shared doctor stages through the process temp directory; the name is
/// made unique because the two output modes of one row doctor concurrently.
fn write_doctored_capture(
    source: &Path,
    scratch: &Path,
    name: &str,
    rewrite: impl FnOnce(DoctoredEnvelope) -> DoctoredEnvelope,
) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = format!("equivalence_{name}_{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let staged = rewrite(DoctoredEnvelope::load(source)).write_to_temp(&unique);
    let path = scratch.join("capture.json");
    fs::copy(&staged, &path).expect("failed to place the doctored capture");
    fs::remove_file(&staged).expect("failed to remove the staged capture");
    path
}

/// Rewrite the served receipt's `gasUsed`, so the replay's gas cannot match it.
///
/// Chosen because gas is one of the dimensions the comparison reports, and a
/// value of one is far enough from any real usage that no future capture
/// refresh can make it accidentally correct.
fn doctor_receipt_gas_used(source: &Path, scratch: &Path) -> PathBuf {
    write_doctored_capture(source, scratch, "receipt_gas", |envelope| {
        envelope.rewrite_receipt(|receipt| {
            receipt["gasUsed"] = serde_json::Value::String("0x1".into());
        })
    })
}

/// Null the served receipt: the endpoint answers, and answers "no receipt".
fn doctor_null_receipt(source: &Path, scratch: &Path) -> PathBuf {
    write_doctored_capture(source, scratch, "null_receipt", DoctoredEnvelope::null_receipt)
}

/// Make two of the four `--tx-file` targets unresolvable, in two different ways.
///
/// [`OTHER_BLOCK_TX`]'s lookup is answered with null — a definitive "unknown
/// transaction", which is execution-class — while [`BLOCK_LAST_TX`]'s entry is
/// removed entirely, so the offline lookup misses and the question goes
/// unanswered (rpc-class). The remaining two targets still replay, so the run
/// mixes both failure classes with real results.
fn doctor_batch_mixed(source: &Path, scratch: &Path) -> PathBuf {
    write_doctored_capture(source, scratch, "batch_mixed", |envelope| {
        envelope.null_transaction(OTHER_BLOCK_TX).drop_transaction(BLOCK_LAST_TX)
    })
}

// ---------------------------------------------------------------------------
// Running and normalizing
// ---------------------------------------------------------------------------

/// A path as the command line carries it.
fn display(path: &Path) -> String {
    path.to_str().expect("paths in this test are utf-8").to_string()
}

/// Run the CLI against `capture`, in an environment scrubbed of everything that
/// could move its output.
///
/// A developer's `RUST_LOG` would add tracing output to stderr, `RUST_BACKTRACE`
/// would paste addresses into a panic, and the fault-injection hooks would make
/// one snapshot depend on how another test left the environment. The home and
/// cache directories are pointed inside the scratch directory because the CLI
/// derives its default RPC cache location from them: an offline replay should
/// not be able to read, write, or name a developer's real cache.
fn run_cli(capture: &Path, args: &[String], scratch: &Path) -> Output {
    let home = scratch.join("home");
    fs::create_dir_all(&home).expect("failed to create the scratch home directory");

    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .arg("replay")
        .arg("--rpc.replay-file")
        .arg(capture)
        .args(args)
        .env_remove("RUST_LOG")
        .env_remove("RUST_BACKTRACE")
        .env_remove("MEGA_EVME_INJECT_PANIC")
        .env_remove("MEGA_EVME_INJECT_FIXTURE_PRE_STATE_ERROR")
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .output()
        .expect("failed to run mega-evme")
}

/// Run one row against `capture` and render its normalized snapshot.
fn snapshot(name: &str, row: &Row, json: bool, capture: &Path, scratch: &Path) -> String {
    let args = row.argv(json);
    let output = run_cli(capture, &args, scratch);
    render(name, row, &args, capture, scratch, &output)
}

/// Render the normalized snapshot of a finished run.
fn render(
    name: &str,
    row: &Row,
    args: &[String],
    capture: &Path,
    scratch: &Path,
    output: &Output,
) -> String {
    let subs = substitutions(row, capture, scratch);
    let normalize = |text: &str| normalize_durations(&normalize_paths(text, &subs));

    let mut snap = String::new();
    snap.push_str("# mega-evme replay equivalence baseline\n");
    snap.push_str(&format!("# case:    {name}\n"));
    snap.push_str(&format!("# capture: {}\n", row.capture));
    snap.push_str(&format!(
        "# argv:    replay --rpc.replay-file {CAPTURE_PATH} {}\n",
        normalize_paths(&args.join(" "), &subs)
    ));

    for (label, body) in &row.inputs {
        section(&mut snap, &format!("input {label}"), body);
    }
    section(
        &mut snap,
        "exit code",
        &output.status.code().map_or_else(|| "killed by a signal".to_string(), |c| c.to_string()),
    );
    section(&mut snap, "stdout", &normalize(&String::from_utf8_lossy(&output.stdout)));
    section(&mut snap, "stderr", &normalize(&String::from_utf8_lossy(&output.stderr)));
    for artifact in &row.artifacts {
        append_artifact(&mut snap, artifact, &normalize);
    }
    snap
}

/// Every path substitution that applies to one run.
///
/// The command-line paths come first because they are the ones a reader wants
/// named; the last three are catch-alls for a path that reached the output some
/// other way, and exist so a leak fails the same on every machine instead of
/// only on the machine that blessed the golden.
fn substitutions(row: &Row, capture: &Path, scratch: &Path) -> Vec<(String, &'static str)> {
    let mut subs = vec![(display(capture), CAPTURE_PATH)];
    for (path, placeholder) in &row.paths {
        subs.push((display(path), placeholder));
    }
    subs.push((display(scratch), SCRATCH_PATH));
    subs.push((display(extracted_root()), EXTRACTED_PATH));
    subs.push((display(Path::new(env!("CARGO_MANIFEST_DIR"))), CRATE_PATH));
    subs
}

/// Rewrite every absolute path a run could print into a stable placeholder.
///
/// Captures are extracted into a temporary directory and the scratch inputs and
/// outputs live in another one, so their paths differ on every run and every
/// machine. Substitutions are applied longest-first so a nested path (a file
/// inside the dump directory, the dump directory inside the scratch directory)
/// is rewritten by its own placeholder rather than by its parent's.
fn normalize_paths(text: &str, subs: &[(String, &'static str)]) -> String {
    let mut ordered: Vec<&(String, &'static str)> = subs.iter().collect();
    ordered.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));

    let mut out = text.to_string();
    for (from, to) in ordered {
        out = out.replace(from.as_str(), to);
    }
    out
}

/// Replace the measured execution time in human output.
///
/// `Execution Time:` is the one field of the human summary that describes the
/// machine rather than the transaction. Only the measured value goes: the
/// label, its column padding and anything printed after the value are kept, so
/// a renderer that stops printing the field — or starts printing more on that
/// line — is still a visible difference.
fn normalize_durations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // `split_inclusive` keeps each line's terminator, so a stream that does not
    // end in a newline is not silently given one.
    for line in text.split_inclusive('\n') {
        match line.split_once(DURATION_LABEL) {
            Some((indent, rest)) => {
                let padding = rest.len() - rest.trim_start().len();
                let value = &rest[padding..];
                let measured = value.find(char::is_whitespace).unwrap_or(value.len());
                out.push_str(indent);
                out.push_str(DURATION_LABEL);
                out.push_str(&rest[..padding]);
                out.push_str(DURATION);
                out.push_str(&value[measured..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Append one snapshot section, marking an empty body rather than leaving a
/// hole a later section could be mistaken for.
fn section(snap: &mut String, title: &str, body: &str) {
    snap.push_str(&format!("\n== {title} ==\n"));
    if body.is_empty() {
        snap.push_str("(empty)\n");
        return;
    }
    snap.push_str(body);
    if !body.ends_with('\n') {
        snap.push('\n');
    }
}

/// Append the content of a run's artifacts.
///
/// A missing file is recorded, not skipped: "the dump was refused" and "the
/// dump changed" must not produce the same snapshot.
fn append_artifact(snap: &mut String, artifact: &Artifact, normalize: &dyn Fn(&str) -> String) {
    match artifact {
        Artifact::File(label, path) => {
            let body = if path.exists() {
                normalize(&fs::read_to_string(path).expect("failed to read artifact"))
            } else {
                "(not written)\n".to_string()
            };
            section(snap, &format!("artifact {label}"), &body);
        }
        Artifact::Dir(label, path) => {
            let Ok(entries) = fs::read_dir(path) else {
                section(snap, &format!("artifact {label}/"), "(directory missing)\n");
                return;
            };
            // Directory order is filesystem-dependent; name order is not.
            let mut names: Vec<String> = entries
                .map(|entry| entry.expect("failed to read a dump directory entry").file_name())
                .map(|name| name.to_str().expect("artifact names are utf-8").to_string())
                .collect();
            names.sort();
            if names.is_empty() {
                section(snap, &format!("artifact {label}/"), "(no files)\n");
                return;
            }
            for name in names {
                let content =
                    fs::read_to_string(path.join(&name)).expect("failed to read artifact");
                section(snap, &format!("artifact {label}/{name}"), &normalize(&content));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Golden-independent invariants
// ---------------------------------------------------------------------------

/// Check what must hold of a run whatever its golden says.
///
/// Runs on every case in both modes, blessing included: while the goldens are
/// being rewritten these are the only assertions left, so they are what keeps
/// `MEGA_EVME_BLESS=1` from accepting a regression wholesale.
fn check_invariants(case: &str, row: &Row, json: bool, output: &Output) {
    let exit = output
        .status
        .code()
        .unwrap_or_else(|| panic!("{case}: the run was killed by a signal, not by an exit code"));
    assert!(
        (0..=3).contains(&exit),
        "{case}: exit {exit} is outside the documented taxonomy (0, 1, 2, 3)",
    );
    assert_eq!(exit, row.expect.exit, "{case}: the row declares exit {}", row.expect.exit);

    if json {
        check_json_stream(case, row, exit, &String::from_utf8_lossy(&output.stdout));
    }
    check_artifacts(case, row);
}

/// Check the machine-readable contract of a `--json` run's stdout.
///
/// Every value must parse, a batch run must keep one value per line, and the
/// run-level error object must be last and appear exactly on the failing runs —
/// the ordering contract a consumer reads the stream by, pinned independently
/// of what any golden happens to hold.
fn check_json_stream(case: &str, row: &Row, exit: i32, stdout: &str) {
    let values = json_values(stdout);
    if row.is_batch() {
        let lines = stdout.lines().filter(|line| !line.trim().is_empty()).count();
        assert_eq!(
            values.len(),
            lines,
            "{case}: a batch --json run must print exactly one JSON value per line",
        );
    }

    let run_errors = values.iter().filter(|value| is_run_error(value)).count();
    if exit == 0 {
        assert_eq!(run_errors, 0, "{case}: a successful run must not print a run error object");
    } else {
        assert_eq!(
            run_errors, 1,
            "{case}: a failing --json run prints exactly one run error object",
        );
        assert!(
            values.last().is_some_and(is_run_error),
            "{case}: the run error object must be the last value on stdout",
        );
    }

    // A per-target failure carries its transaction hash alongside `error`; a
    // result line carries no top-level `error` at all.
    let error_lines =
        values.iter().filter(|value| !is_run_error(value) && value.get("error").is_some()).count();
    if let Some(expected) = row.expect.error_lines {
        assert_eq!(
            error_lines, expected,
            "{case}: the row declares {expected} target error line(s)"
        );
    }
    if let Some(expected) = row.expect.result_lines {
        assert_eq!(
            values.len() - run_errors - error_lines,
            expected,
            "{case}: the row declares {expected} target result line(s)",
        );
    }
}

/// Check that the run wrote the files the row says it must.
fn check_artifacts(case: &str, row: &Row) {
    for artifact in &row.artifacts {
        match artifact {
            Artifact::File(label, path) => {
                if let Some(written) = row.expect.dump_written {
                    assert_eq!(
                        path.exists(),
                        written,
                        "{case}: the row declares that {label} is{} written",
                        if written { "" } else { " not" },
                    );
                }
            }
            Artifact::Dir(label, path) => {
                if let Some(expected) = row.expect.dir_files {
                    let found = fs::read_dir(path)
                        .unwrap_or_else(|e| panic!("{case}: cannot read {label} ({e})"))
                        .count();
                    assert_eq!(
                        found, expected,
                        "{case}: the row declares {expected} file(s) in {label}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Golden comparison
// ---------------------------------------------------------------------------

/// Lines of context shown on either side of the first difference.
const CONTEXT: usize = 3;

/// Longest line rendered in a mismatch report before it is excerpted.
const EXCERPT: usize = 160;

/// Path of the golden of `case`.
fn golden_path(case: &str) -> PathBuf {
    golden_dir().join(format!("{case}.snap"))
}

/// Compare `snap` against the golden of `case`.
fn check_golden(case: &str, snap: &str) {
    let path = golden_path(case);
    let golden = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{case}: cannot read {} ({e})\n\
             run `MEGA_EVME_BLESS=1 cargo test -p mega-evme --test replay_equivalence` \
             to create it",
            path.display(),
        )
    });
    assert!(golden == snap, "{}", mismatch_report(case, &path, &golden, snap));
}

/// Write a golden through a temporary file, so a concurrent reader sees either
/// the whole old snapshot or the whole new one.
fn write_golden(path: &Path, snap: &str) {
    let dir = path.parent().expect("golden paths have a parent");
    fs::create_dir_all(dir).expect("failed to create the golden directory");
    let staging = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().expect("golden paths have a name").to_string_lossy(),
        std::process::id(),
    ));
    fs::write(&staging, snap).expect("failed to write the golden");
    fs::rename(&staging, path).expect("failed to install the golden");
}

/// Report the first place a snapshot diverges from its golden, and say how to
/// accept the new behavior.
fn mismatch_report(case: &str, path: &Path, golden: &str, actual: &str) -> String {
    let mut out = diff_report(case, path, golden, actual);
    out.push_str(
        "accept the new behavior with \
         `MEGA_EVME_BLESS=1 cargo test -p mega-evme --test replay_equivalence`\n",
    );
    out
}

/// Report the first place two snapshots diverge.
///
/// A whole-block snapshot runs to thousands of lines, so the report shows the
/// first difference with a little context instead of both documents.
fn diff_report(case: &str, path: &Path, golden: &str, actual: &str) -> String {
    let expected: Vec<&str> = golden.lines().collect();
    let found: Vec<&str> = actual.lines().collect();
    if expected == found {
        // Every line matches, so the difference is in what `lines()` drops.
        return format!(
            "{case}: snapshot differs from {} only in its trailing newline\n",
            path.display(),
        );
    }
    let first = (0..expected.len().max(found.len()))
        .find(|&i| expected.get(i) != found.get(i))
        .unwrap_or(0);

    let mut out = format!(
        "{case}: replay behavior changed against {}\n\
         golden has {} line(s), this run produced {}; first difference on line {}\n",
        path.display(),
        expected.len(),
        found.len(),
        first + 1,
    );
    let end = (first + CONTEXT + 1).min(expected.len().max(found.len()));
    for i in first.saturating_sub(CONTEXT)..end {
        match (expected.get(i), found.get(i)) {
            (Some(a), Some(b)) if a == b => {
                out.push_str(&format!("  {:>6} | {}\n", i + 1, excerpt(a, 0)));
            }
            (a, b) => {
                let at = common_prefix(a.unwrap_or(&""), b.unwrap_or(&""));
                if let Some(a) = a {
                    out.push_str(&format!("- {:>6} | {}\n", i + 1, excerpt(a, at)));
                }
                if let Some(b) = b {
                    out.push_str(&format!("+ {:>6} | {}\n", i + 1, excerpt(b, at)));
                }
            }
        }
    }
    out
}

/// Byte offset of the first difference between two lines.
fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// Render at most [`EXCERPT`] bytes of `line`, in a window around `around` so a
/// difference deep inside a long JSON line is still visible.
fn excerpt(line: &str, around: usize) -> String {
    if line.len() <= EXCERPT {
        return line.to_string();
    }
    let start = floor_boundary(line, around.saturating_sub(EXCERPT / 2));
    let end = floor_boundary(line, (start + EXCERPT).min(line.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&line[start..end]);
    if end < line.len() {
        out.push('…');
    }
    out
}

/// Largest character boundary at or below `index`.
fn floor_boundary(line: &str, index: usize) -> usize {
    let mut index = index.min(line.len());
    while !line.is_char_boundary(index) {
        index -= 1;
    }
    index
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// Run one row in one output mode from a fresh scratch directory, check the
/// invariants, and render the snapshot.
fn run_row(case: &str, json: bool, build: fn(&Path) -> Row) -> String {
    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let row = build(scratch.path());
    let capture = row.capture_path(scratch.path());
    let args = row.argv(json);
    let output = run_cli(&capture, &args, scratch.path());
    check_invariants(case, &row, json, &output);
    render(case, &row, &args, &capture, scratch.path(), &output)
}

/// Run one row in one output mode and check it against its golden.
///
/// Blessing rewrites the golden and then re-derives it: the row runs a second
/// time from a fresh scratch directory, with fresh temporary paths, and the
/// just-installed golden must hold for it. A run-dependent or path-dependent
/// snapshot therefore fails at the bless, where the author can still see what
/// they were about to commit, rather than as a flaky golden later.
fn check_row(case: &str, mode: &str, json: bool, build: fn(&Path) -> Row) {
    let case = format!("{case}_{mode}");
    let snap = run_row(&case, json, build);

    if !blessing() {
        check_golden(&case, &snap);
        return;
    }

    let path = golden_path(&case);
    write_golden(&path, &snap);
    let installed = fs::read_to_string(&path).expect("failed to read back the blessed golden");
    let again = run_row(&case, json, build);
    assert!(
        installed == again,
        "{case}: two runs of this row produced different snapshots, so the golden just \
         written pins something run- or path-dependent rather than behavior; fix the \
         normalization before committing it\n{}",
        diff_report(&case, &path, &installed, &again),
    );
}

/// Declare the two snapshot tests — human and `--json` — of one matrix row.
///
/// One test per case rather than one test per row, so cargo runs them in
/// parallel and a failure names exactly one snapshot.
macro_rules! matrix {
    ($($case:ident : $build:path),* $(,)?) => {
        $(
            mod $case {
                use super::*;

                #[test]
                fn test_human() {
                    check_row(stringify!($case), "human", false, $build);
                }

                #[test]
                fn test_json() {
                    check_row(stringify!($case), "json", true, $build);
                }
            }
        )*
    };
}

matrix! {
    single_offline_mined: row_single_offline_mined,
    single_offline_verify: row_single_offline_verify,
    single_verify_mismatch: row_single_verify_mismatch,
    single_verify_dump: row_single_verify_dump,
    single_smoke_deposit: row_single_smoke_deposit,
    single_smoke_verify_unavailable: row_single_smoke_verify_unavailable,
    single_bucket_cap_mined: row_single_bucket_cap_mined,
    single_halt_mined: row_single_halt_mined,
    single_halt_verify: row_single_halt_verify,
    single_halt_dump_refused: row_single_halt_dump_refused,
    single_block_tx_mined: row_single_block_tx_mined,
    single_block_tx_verify: row_single_block_tx_verify,
    single_block_tx_dump: row_single_block_tx_dump,
    batch_block: row_batch_block,
    batch_block_verify: row_batch_block_verify,
    batch_tx_file: row_batch_tx_file,
    batch_tx_file_verify: row_batch_tx_file_verify,
    batch_mixed_failures: row_batch_mixed_failures,
    batch_verify_unavailable: row_batch_verify_unavailable,
    batch_tx_file_dump_dir: row_batch_tx_file_dump_dir,
    batch_dump_dir_refused: row_batch_dump_dir_refused,
    batch_dump_dir_overwrite: row_batch_dump_dir_overwrite,
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------

/// Read the committed golden of `case`, if it is there.
fn golden_of(case: &str) -> Option<String> {
    fs::read_to_string(golden_path(case)).ok()
}

/// Snapshot a row under the name of an existing case, against a verbatim copy
/// of that case's capture at a fresh temporary path.
///
/// The copy is what makes the result meaningful: a snapshot that reproduces the
/// committed golden from a capture that never sat where the golden's capture sat
/// cannot be depending on its inputs' location, which is the property that lets
/// a golden blessed on one machine hold on another.
fn clean_snapshot(case: &str, json: bool, build: fn(&Path) -> Row) -> String {
    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let row = build(scratch.path());
    assert!(row.doctoring.is_none(), "{case}: a control's baseline row must not be doctored");
    let copy = scratch.path().join("capture.json");
    fs::copy(fixture(row.capture), &copy).expect("failed to copy the capture");
    snapshot(case, &row, json, &copy, scratch.path())
}

/// Snapshot a row under the name of an existing case, against a copy of its
/// capture that `doctor` has modified.
fn doctored_snapshot(
    case: &str,
    json: bool,
    build: fn(&Path) -> Row,
    doctor: fn(&mut serde_json::Value),
) -> String {
    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let row = build(scratch.path());
    let source = fixture(row.capture);
    let mut capture: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&source).expect("failed to read the capture"))
            .expect("the capture is JSON");
    doctor(&mut capture);
    let copy = scratch.path().join("capture.json");
    fs::write(&copy, capture.to_string()).expect("failed to write the doctored capture");
    snapshot(case, &row, json, &copy, scratch.path())
}

/// Cross-check a control's clean snapshot against the case's golden.
///
/// Blessing rewrites that golden from another test thread, so reading it here
/// would race a concurrent update. The property is checked without the file
/// instead: a second independent clean run, from another temporary path, must
/// produce the same bytes. That is what "the golden is reproducible elsewhere"
/// means, minus the file the bless is in the middle of replacing.
fn check_clean_against_golden(case: &str, json: bool, build: fn(&Path) -> Row, clean: &str) {
    if blessing() {
        let second = clean_snapshot(case, json, build);
        assert!(clean == second, "{}", diff_report(case, &golden_path(case), clean, &second),);
        return;
    }
    let golden = golden_of(case).expect("the baseline case must have a committed golden");
    assert!(clean == golden, "{}", mismatch_report(case, &golden_path(case), &golden, clean));
}

/// Rewrite the `gas` of [`BLOCK_MID_TX`]'s served body to zero.
///
/// Entries are keyed by the request, not the response, so the tampered answer
/// still resolves — but the body no longer hashes to the requested hash. Only
/// the transaction's own response carries it as the `hash` field; the block body
/// lists bare hashes and a receipt names it `transactionHash`.
fn doctor_transaction_gas(capture: &mut serde_json::Value) {
    let marker = format!("\"hash\":\"{BLOCK_MID_TX}\"");
    let mut doctored = 0;
    for entry in capture["cache"].as_array_mut().expect("cache entries") {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        let result = response.get_mut("result").expect("transaction result");
        assert!(result.is_object(), "expected a transaction object for {BLOCK_MID_TX}");
        result["gas"] = serde_json::Value::String("0x0".into());
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "the capture must hold exactly one response for {BLOCK_MID_TX}");
}

/// Raise the gas limit [`BLOCK`]'s header advertises by one.
///
/// Chosen because it is invisible to the replay itself — every transaction of
/// the block still fits, so stdout does not move — and lands only in the
/// dumped fixture's `currentGasLimit`.
fn doctor_block_gas_limit(capture: &mut serde_json::Value) {
    let number = format!("0x{BLOCK:x}");
    let mut doctored = 0;
    for entry in capture["cache"].as_array_mut().expect("cache entries") {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains("\"transactions\"") {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse block response");
        let result = &mut response["result"];
        if result["number"].as_str() != Some(number.as_str()) {
            continue;
        }
        let limit = u64::from_str_radix(
            result["gasLimit"].as_str().expect("gasLimit").trim_start_matches("0x"),
            16,
        )
        .expect("hex gas limit");
        result["gasLimit"] = serde_json::Value::String(format!("0x{:x}", limit + 1));
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "the capture must hold exactly one header for block {BLOCK}");
}

/// A tampered transaction body must change the snapshot.
///
/// The first half of the gate's honesty check: a doctored capture the harness
/// cannot tell apart from a clean one would let any refactor through. Both
/// captures are rewritten to the same `<CAPTURE>` placeholder and neither sits
/// where the golden's capture sat, so their paths cannot be what makes the
/// snapshots differ — nor what makes the clean one match.
#[test]
fn test_negative_control_tampered_transaction_changes_the_snapshot() {
    const CASE: &str = "single_block_tx_mined_json";

    let clean = clean_snapshot(CASE, true, row_single_block_tx_mined);
    let doctored = doctored_snapshot(CASE, true, row_single_block_tx_mined, doctor_transaction_gas);

    assert!(
        clean != doctored,
        "zeroing a transaction's gas left the snapshot unchanged: the baseline cannot \
         detect a behavior change",
    );

    check_clean_against_golden(CASE, true, row_single_block_tx_mined, &clean);
    if !blessing() {
        let golden = golden_of(CASE).expect("the baseline case must have a committed golden");
        assert!(doctored != golden, "the doctored capture reproduced the golden of {CASE}");
    }
}

/// A change no renderer prints must still change the snapshot, through the
/// artifact.
///
/// The second half: the doctored gas limit leaves the exit code, stdout and
/// stderr identical, so the only thing that can catch it is the dumped fixture.
/// Without this, dropping the artifact section from the snapshot would go
/// unnoticed.
#[test]
fn test_negative_control_tampered_block_gas_limit_changes_the_dumped_fixture() {
    const CASE: &str = "single_block_tx_dump_json";

    let clean = clean_snapshot(CASE, true, row_single_block_tx_dump);
    let doctored = doctored_snapshot(CASE, true, row_single_block_tx_dump, doctor_block_gas_limit);

    let (clean_head, clean_artifact) = split_at_artifact(&clean);
    let (doctored_head, doctored_artifact) = split_at_artifact(&doctored);
    assert!(
        clean_head == doctored_head,
        "the doctored gas limit was expected to be invisible to the renderers, but the \
         reported output moved; pick another doctoring for this control",
    );
    assert!(
        clean_artifact != doctored_artifact,
        "the doctored gas limit never reached the snapshot: the artifact section is not \
         load-bearing",
    );

    check_clean_against_golden(CASE, true, row_single_block_tx_dump, &clean);
}

/// Split a snapshot into what the run reported and what it wrote.
fn split_at_artifact(snap: &str) -> (&str, &str) {
    let marker = format!("\n== artifact {DUMP_FIXTURE_PATH} ==\n");
    snap.split_once(marker.as_str()).expect("the snapshot must carry a dumped fixture")
}

/// The stderr section must carry a failure's continuation lines.
///
/// The CLI writes a failure as `error: <message>`, and a message may run to
/// several lines — an offline cache miss appends a `hint:` line telling the
/// caller how to re-capture. A snapshot that kept only the `error:` line would
/// let that hint be rewritten, reordered or dropped without failing the gate,
/// so this pins that the section is the whole stream rather than a filter over
/// it. Checked against the raw output, so it does not depend on a golden being
/// current.
#[test]
fn test_negative_control_stderr_section_keeps_continuation_lines() {
    const CASE: &str = "single_smoke_verify_unavailable_human";

    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let row = row_single_smoke_verify_unavailable(scratch.path());
    let capture = fixture(row.capture);
    let args = row.argv(false);
    let output = run_cli(&capture, &args, scratch.path());
    let raw = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        raw.lines().any(|line| line.starts_with("error:")),
        "the failure must be reported under the error: prefix:\n{raw}",
    );
    let hint = raw
        .lines()
        .find(|line| line.starts_with("hint:"))
        .expect("the offline cache-miss failure must print a hint continuation line");

    let snap = render(CASE, &row, &args, &capture, scratch.path(), &output);
    let (_, stderr) =
        snap.split_once("\n== stderr ==\n").expect("the snapshot must carry a stderr section");
    assert!(
        stderr.contains(&normalize_paths(hint, &substitutions(&row, &capture, scratch.path()))),
        "the snapshot dropped the hint continuation line:\n{stderr}",
    );
}

/// The timing normalizer must still be rewriting something.
///
/// If human output stopped carrying `Execution Time:` — or the normalizer
/// stopped matching it — the goldens would either go flaky or silently lose the
/// field. This checks the raw output against the normalized one, so it does not
/// depend on a golden being current.
#[test]
fn test_negative_control_duration_normalizer_rewrites_human_timings() {
    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let row = row_single_offline_mined(scratch.path());
    let capture = fixture(row.capture);
    let output = run_cli(&capture, &row.argv(false), scratch.path());
    let raw = String::from_utf8(output.stdout).expect("stdout is utf-8");

    let measured: Vec<&str> = raw.lines().filter(|line| line.contains(DURATION_LABEL)).collect();
    assert_eq!(measured.len(), 1, "human output must carry one measured time:\n{raw}");
    assert!(
        !measured[0].ends_with(DURATION),
        "the measured time must not already be a placeholder: {}",
        measured[0],
    );

    let normalized = normalize_durations(&raw);
    assert!(
        normalized != raw,
        "the timing normalizer rewrote nothing, so the goldens are pinning a measured time",
    );
    let expected = format!("{DURATION_LABEL}   {DURATION}");
    let placeholders: Vec<&str> =
        normalized.lines().filter(|line| line.contains(DURATION_LABEL)).collect();
    assert_eq!(
        placeholders,
        vec![expected.as_str()],
        "the label and its padding must survive normalization",
    );
}
