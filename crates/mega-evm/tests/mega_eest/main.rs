//! The Mega EEST conformance suite on this checkout of mega-evm.
//!
//! Mega EEST holds the state-transition cases both Satin implementations share, each pinned to
//! rules of the Satin specification. `scripts/mega_eest.sh` takes the suite at the commit
//! `scripts/mega_eest.env` pins, builds the suite's revm adapter against this checkout and consumes
//! every case, in the modes `MEGA_EEST_MODES` names (plain and instrumented by default); this test
//! runs that script and fails when it does not pass.
//!
//! It is ignored: it builds a second copy of mega-evm for the adapter and runs for a minute or
//! more, and CI runs the script itself as the Mega EEST gate. The mutation gate runs it in plain
//! mode alone (`MEGA_EEST_MODES=plain`, nextest's `--run-ignored only`) for each mutant the rest of
//! the suite does not catch. The script's summary line is printed and repeated in the failure
//! message, because the mutation gate counts a mutant as caught only when that line, in the
//! mutant's log, says the cases ran and failed (`result=FAIL`): a run that never reached the cases
//! (a build or fetch that failed, exit 2) fails this test too, and the gate refuses it as no
//! verdict rather than counting a kill.

use std::{
    path::Path,
    process::{Command, Stdio},
};

#[test]
#[ignore = "runs the Mega EEST suite through scripts/mega_eest.sh, which takes minutes"]
fn test_mega_eest_cases_pass_on_this_checkout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("scripts/mega_eest.sh");
    let output = Command::new("bash")
        .arg(&script)
        .current_dir(&root)
        .stderr(Stdio::inherit())
        .output()
        .unwrap_or_else(|err| panic!("cannot run {}: {err}", script.display()));
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    let summary = stdout
        .lines()
        .rfind(|line| line.starts_with("MEGA-EEST "))
        .unwrap_or("(no MEGA-EEST summary line)");
    match output.status.code() {
        Some(0) => assert!(
            summary.ends_with(" result=pass"),
            "{} exited 0 without a passing summary: {summary}",
            script.display()
        ),
        Some(1) => panic!("Mega EEST cases failed on this checkout: {summary}"),
        _ => panic!("the Mega EEST run reached no verdict ({}): {summary}", output.status),
    }
}
