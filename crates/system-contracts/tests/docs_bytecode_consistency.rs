//! Consistency check between the spec's static bytecode files and the versioned artifacts.
//!
//! The specification publishes the full deployed bytecode of every system-contract version as a
//! static file under `docs/spec/static/bytecode/<Contract>-<version>.txt`, while the versioned
//! artifact JSONs under `artifacts/` are the implementation's source of truth and are
//! hash-attested by `build.rs` on every repository build.
//!
//! This test pins the two copies to each other: the set of static files must correspond 1:1 to
//! the set of versioned artifacts, and each file's content must equal the artifact's
//! `deployedBytecode` (and hash to its `codeHash`). Adding a new contract version without
//! publishing its static bytecode file — or leaving a stale file behind — fails here.
//!
//! Both directories exist only in a repository checkout; when building from a published crate
//! (which excludes `artifacts/` and has no `docs/`), the test skips.

use std::{collections::BTreeSet, fs, path::Path};

use mega_system_contracts::alloy_primitives::{hex, keccak256, B256};

#[test]
fn test_docs_static_bytecode_matches_artifacts() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = manifest_dir.join("artifacts");
    let docs_dir = manifest_dir.join("../../docs/spec/static/bytecode");
    if !artifacts_dir.is_dir() || !docs_dir.is_dir() {
        eprintln!("skipping: not a repository checkout (artifacts/ or docs/ unavailable)");
        return;
    }

    let mut artifact_names = BTreeSet::new();
    for entry in fs::read_dir(&artifacts_dir).expect("failed to read artifacts directory") {
        let path = entry.expect("failed to read artifacts directory entry").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = name.strip_suffix(".json") else { continue };
        if stem.ends_with("-latest") {
            continue;
        }
        artifact_names.insert(stem.to_string());
    }
    assert!(
        !artifact_names.is_empty(),
        "no versioned artifacts found in {}",
        artifacts_dir.display()
    );

    let mut published_names = BTreeSet::new();
    for entry in fs::read_dir(&docs_dir).expect("failed to read docs static bytecode directory") {
        let path = entry.expect("failed to read docs static bytecode directory entry").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if name.starts_with('.') {
            continue;
        }
        let stem = name.strip_suffix(".txt").unwrap_or_else(|| {
            panic!("unexpected non-.txt file in docs/spec/static/bytecode: {name}")
        });
        published_names.insert(stem.to_string());
    }

    assert_eq!(
        published_names, artifact_names,
        "docs/spec/static/bytecode/*.txt must correspond 1:1 to versioned artifacts/*.json"
    );

    for name in &artifact_names {
        let artifact: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(artifacts_dir.join(format!("{name}.json")))
                .expect("failed to read artifact"),
        )
        .expect("failed to parse artifact JSON");
        let expected_bytecode = artifact["deployedBytecode"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: artifact has no string deployedBytecode field"));
        let code_hash: B256 = artifact["codeHash"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: artifact has no string codeHash field"))
            .parse()
            .unwrap_or_else(|e| panic!("{name}: invalid codeHash: {e}"));

        let published = fs::read_to_string(docs_dir.join(format!("{name}.txt")))
            .expect("failed to read static bytecode file");
        let published = published.trim_end();

        assert_eq!(
            published, expected_bytecode,
            "{name}: static bytecode file differs from artifact deployedBytecode"
        );
        let bytes =
            hex::decode(published).unwrap_or_else(|e| panic!("{name}: invalid bytecode hex: {e}"));
        assert_eq!(
            keccak256(&bytes),
            code_hash,
            "{name}: keccak256 of static bytecode != artifact codeHash"
        );
    }
}
