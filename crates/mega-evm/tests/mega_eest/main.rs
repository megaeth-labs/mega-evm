//! The Mega EEST conformance suite on this checkout of mega-evm.
//!
//! Mega EEST holds the state-transition cases both Satin implementations share, each pinned to
//! rules of the Satin specification. `scripts/mega_eest.sh` takes the suite at the commit
//! `scripts/mega_eest.env` pins, builds the suite's revm adapter against this checkout and consumes
//! every case, plain and instrumented; this test runs that script and fails when it fails.
//!
//! It is ignored: it builds a second copy of mega-evm for the adapter and runs for a minute or
//! more, and CI runs the script itself as the Mega EEST gate. The mutation gate runs it, with
//! nextest's `--run-ignored only`, for each mutant the rest of the suite does not catch, so a
//! mutant that only a Mega EEST case catches counts as caught.

use std::{path::Path, process::Command};

#[test]
#[ignore = "runs the Mega EEST suite through scripts/mega_eest.sh, which takes minutes"]
fn test_mega_eest_cases_pass_on_this_checkout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("scripts/mega_eest.sh");
    let status = Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .status()
        .unwrap_or_else(|err| panic!("cannot run {}: {err}", script.display()));
    assert!(
        status.success(),
        "{} failed ({status}): the lines above name what failed",
        script.display()
    );
}
