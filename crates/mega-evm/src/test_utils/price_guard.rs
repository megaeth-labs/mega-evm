//! The note a test leaves when the byte prices in effect leave it nothing to run.

/// The variable naming the file [`note_price_guard`] appends its notes to. The byte-price grid
/// sets it for each point and counts the lines, so a point at which tests returned early is told
/// from one at which they ran.
pub const PRICE_GUARD_LOG_ENV_VAR: &str = "MEGA_PRICE_GUARD_LOG";

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
    use std::{collections::BTreeSet, io::Write, sync::Mutex};

    static NOTED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());
    let test = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
    let mut noted = NOTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !noted.iter().any(|(_, noted_reason)| noted_reason == reason) {
        let _ = writeln!(std::io::stderr(), "note: some tests return early, because {reason}");
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
