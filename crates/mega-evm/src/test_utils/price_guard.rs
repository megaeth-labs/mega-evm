//! The notes a test leaves when the byte prices in effect leave it something it does not run.

/// The variable naming the file [`note_price_guard`] and [`note_snapshot_skipped`] append their
/// notes to. The byte-price grid sets it for each point and counts the lines, so a point at which
/// tests returned early is told from one at which they ran, and both from one at which tests ran
/// whole and only left their snapshots uncompared.
pub const PRICE_GUARD_LOG_ENV_VAR: &str = "MEGA_PRICE_GUARD_LOG";

/// The reason [`note_snapshot_skipped`] notes. The byte-price grid counts the lines that start
/// with it apart from the price guards'.
pub const SNAPSHOT_SKIPPED: &str = "snapshot comparison skipped";

/// Notes that the running test took a price guard: it returned early, or left out the cases the
/// byte prices in effect leave nothing to run for, because of `reason`.
///
/// The first note of a reason goes to stderr, written straight to it: the harness captures the
/// print macros of a test that passes, so a line written with them would never be read. Every
/// test and reason is appended once, as `<reason>: <test>`, to the file
/// [`PRICE_GUARD_LOG_ENV_VAR`] names, when it names one; a file of its own, since a line written
/// into the harness's output can land inside a test's result line.
#[cfg(feature = "std")]
pub fn note_price_guard(reason: &str) {
    note(reason, &format!("some tests return early, because {reason}"));
}

/// Notes that the running test skipped a snapshot comparison, as [`note_price_guard`] notes a
/// guard, under the reason [`SNAPSHOT_SKIPPED`].
///
/// It is not a price guard: snapshots are pinned at the spec's byte prices, so at other prices a
/// test leaves its snapshot uncompared and runs every assertion it makes, as it does at the
/// spec's prices.
#[cfg(feature = "std")]
pub fn note_snapshot_skipped() {
    note(
        SNAPSHOT_SKIPPED,
        "some snapshot comparisons are skipped, because snapshots are pinned at the spec's byte \
         prices; the tests run every assertion",
    );
}

/// Writes `first` to stderr the first time `reason` is noted, and `<reason>: <test>` to the log
/// once per test and reason.
#[cfg(feature = "std")]
fn note(reason: &str, first: &str) {
    use std::{collections::BTreeSet, io::Write, sync::Mutex};

    static NOTED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());
    let test = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
    let mut noted = NOTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !noted.iter().any(|(_, noted_reason)| noted_reason == reason) {
        let _ = writeln!(std::io::stderr(), "note: {first}");
    }
    if !noted.insert((test.clone(), reason.to_owned())) {
        return;
    }
    let Some(path) = std::env::var_os(PRICE_GUARD_LOG_ENV_VAR) else { return };
    // One appending write per line, so the test threads' lines do not interleave.
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = log.write_all(format!("{reason}: {test}\n").as_bytes());
    }
}
