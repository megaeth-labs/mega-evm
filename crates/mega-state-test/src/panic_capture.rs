//! Turning a panic inside one fixture unit into a recorded result.
//!
//! A sweep over tens of thousands of fixtures is only useful if a single fixture that trips a
//! `debug_assert!` costs that one fixture rather than the whole run. The alternative used before
//! this module — one process per fixture — isolates perfectly but pays a process spawn and a
//! fixture parse per case, which is what made a full-corpus sweep an overnight job.
//!
//! Two pieces are needed. [`catch`] contains the unwind, and the hook installed by
//! [`install_capture_hook`] records the panic's location and message, which the payload alone does
//! not carry.

use std::{
    cell::RefCell,
    panic::{self, AssertUnwindSafe},
    string::{String, ToString},
    sync::atomic::{AtomicBool, Ordering},
};

thread_local! {
    /// The report of the most recent panic on this thread, written by the capture hook and taken
    /// by [`catch`]. Thread-local because worker threads panic independently.
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Whether [`install_capture_hook`] has run. Read by [`catch`] to decide whether a taken-empty
/// report means "no hook" or "hook installed but the panic carried no location".
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Replaces the process-wide panic hook with one that records each panic instead of printing it.
///
/// This is process-wide and permanent: it silences the default hook (message, location and
/// backtrace) for every panic in the process, caught or not. Call it only from a driver that
/// reports the captured reports itself — a caught panic that nobody prints is a panic nobody
/// sees.
///
/// Idempotent: a second call is a no-op, so several drivers in one process cannot stack hooks.
pub fn install_capture_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    panic::set_hook(Box::new(|info| {
        // The `Display` form is `panicked at <file>:<line>:<col>:\n<message>` — the same shape the
        // default hook prints, minus the backtrace.
        let report = info.to_string();
        LAST_PANIC.with(|slot| *slot.borrow_mut() = Some(report));
    }));
}

/// Runs `f`, converting a panic into `Err(<panic report>)`.
///
/// The report is the one the capture hook recorded when [`install_capture_hook`] has run;
/// otherwise it falls back to the panic payload, which carries the message but not the location.
///
/// `f` is treated as unwind-safe. Every caller in this crate builds the state it touches from
/// scratch for each fixture unit, so a half-updated value cannot outlive the panic; do not use
/// this to wrap work that mutates state shared with the next unit.
pub fn catch<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    // Clear first: a report left by an earlier panic on this thread must not be attributed to
    // this call.
    LAST_PANIC.with(|slot| *slot.borrow_mut() = None);
    panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        LAST_PANIC
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_else(|| payload_message(&payload))
    })
}

/// Best-effort message from a panic payload, for the no-hook path.
fn payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catch_returns_the_value_when_nothing_panics() {
        assert_eq!(catch(|| 41 + 1).expect("no panic"), 42);
    }

    #[test]
    fn test_catch_converts_a_panic_into_an_error() {
        let err = catch(|| panic!("boom {}", 7)).expect_err("panic must be caught");
        assert!(err.contains("boom 7"), "report should carry the message: {err}");
    }

    #[test]
    fn test_catch_reports_a_panic_after_the_hook_is_installed() {
        // The hook adds the location, which the payload alone does not carry. Installing it is
        // process-wide, so this test also fixes what the other tests in this module observe —
        // both accept either report shape.
        install_capture_hook();
        let err = catch(|| panic!("located")).expect_err("panic must be caught");
        assert!(err.contains("located"), "report should carry the message: {err}");
        assert!(err.contains("panicked at"), "hook report should carry the location: {err}");
    }

    #[test]
    fn test_catch_does_not_attribute_an_earlier_panic_to_a_later_call() {
        let _ = catch(|| panic!("first"));
        assert_eq!(catch(|| "second").expect("no panic"), "second");
    }
}
