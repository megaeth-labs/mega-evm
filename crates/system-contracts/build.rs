//! Checks the artifacts and generated Rust sources when they are shipped together.

use std::path::Path;

use anyhow::{ensure, Result};

mod artifact_checks;

fn main() -> Result<()> {
    let crate_dir = Path::new(".");
    let artifacts = crate_dir.join("artifacts");
    if artifacts.try_exists()? {
        println!("cargo::rerun-if-changed=artifacts");
        println!("cargo::rerun-if-changed=src/generated");
        artifact_checks::check_generated(crate_dir)?;
    } else {
        ensure!(
            !crate_dir.join("scripts").try_exists()? &&
                !crate_dir.join("foundry.toml").try_exists()?,
            "System contract checkout is missing artifacts/. Restore the checked-in artifacts."
        );
        // Published and vendored packages contain the generated Rust sources and ABI interfaces.
        println!("cargo::rerun-if-changed=build.rs");
    }
    Ok(())
}
