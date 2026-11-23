//! Checks the artifacts and generated Rust sources when they are shipped together.

use std::path::Path;

use anyhow::Result;

mod artifact_checks;

fn main() -> Result<()> {
    let crate_dir = Path::new(".");
    let artifacts = crate_dir.join("artifacts");
    if artifacts.try_exists()? {
        println!("cargo::rerun-if-changed=artifacts");
        println!("cargo::rerun-if-changed=src/generated");
        artifact_checks::check_generated(crate_dir)?;
    } else {
        // Published packages ship the generated Rust files without the JSON artifacts.
        println!("cargo::rerun-if-changed=build.rs");
    }
    Ok(())
}
