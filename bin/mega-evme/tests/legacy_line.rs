//! The legacy leg runs the line the chain ran, at the versions it shipped with.
//!
//! The released `mega-evme` 1.7.1 and `mega-evm` 1.7.1 are pinned exactly in the manifest, but
//! what they depend on is resolved in this workspace's lockfile, where a `cargo update` could
//! move it. Every package below is pinned to the version the 1.7.1 release's own lockfile holds;
//! a change of any of them changes how history replays and has to be a decision, made here.

/// The workspace lockfile.
const LOCKFILE: &str = include_str!("../../../Cargo.lock");

/// The legacy line: every package whose version decides what the legacy leg executes or prints,
/// at the version `v1.7.1`'s `Cargo.lock` holds.
const LEGACY_LINE: &[(&str, &str)] = &[
    // The released engine, CLI and the crates they are published with.
    ("mega-evm", "1.7.1"),
    ("mega-evme", "1.7.1"),
    ("mega-state-test", "1.7.1"),
    ("mega-system-contracts", "1.7.1"),
    // revm 27 and op-revm 8: what executes.
    ("revm", "27.1.0"),
    ("revm-bytecode", "6.2.1"),
    ("revm-context", "8.0.4"),
    ("revm-context-interface", "9.0.0"),
    ("revm-database", "7.0.4"),
    ("revm-database-interface", "7.0.4"),
    ("revm-handler", "8.1.0"),
    ("revm-inspector", "8.1.0"),
    ("revm-interpreter", "24.0.0"),
    ("revm-precompile", "25.0.0"),
    ("revm-primitives", "20.2.1"),
    ("revm-state", "7.0.4"),
    ("op-revm", "8.1.0"),
    // The block executor's layer and the hardfork tables.
    ("alloy-evm", "0.15.0"),
    ("alloy-op-evm", "0.15.0"),
    ("alloy-hardforks", "0.2.13"),
    ("alloy-op-hardforks", "0.2.13"),
    // What the legacy CLI decodes, encodes, fetches and prints.
    ("alloy-consensus", "1.1.0"),
    ("alloy-eips", "1.8.3"),
    ("alloy-network", "1.1.0"),
    ("alloy-provider", "1.1.0"),
    ("alloy-rpc-types-eth", "1.1.0"),
    ("alloy-rpc-types-trace", "1.0.24"),
    ("op-alloy-consensus", "0.18.14"),
    ("op-alloy-network", "0.18.14"),
    ("op-alloy-rpc-types", "0.18.14"),
    ("revm-inspectors", "0.27.3"),
];

/// Every `(name, version)` package in `lockfile`.
fn locked_packages(lockfile: &str) -> Vec<(&str, &str)> {
    lockfile
        .split("[[package]]")
        .skip(1)
        .filter_map(|entry| {
            let field = |key: &str| {
                entry.lines().find_map(|line| {
                    line.strip_prefix(key)?.strip_prefix(" = \"")?.strip_suffix('"')
                })
            };
            Some((field("name")?, field("version")?))
        })
        .collect()
}

/// The semver-compatibility line of `version`: its major, or `0.minor` below 1.
fn compatibility_line(version: &str) -> &str {
    let mut parts = version.splitn(3, '.');
    let major = parts.next().unwrap_or_default();
    if major == "0" {
        let minor_len = parts.next().map_or(0, str::len);
        &version[..major.len() + 1 + minor_len]
    } else {
        major
    }
}

#[test]
fn test_the_legacy_line_is_at_its_released_versions() {
    let locked = locked_packages(LOCKFILE);
    for &(name, pinned) in LEGACY_LINE {
        let line = compatibility_line(pinned);
        let versions: Vec<&str> = locked
            .iter()
            .filter(|(n, v)| *n == name && compatibility_line(v) == line)
            .map(|(_, v)| *v)
            .collect();
        assert_eq!(
            versions,
            [pinned],
            "{name}: the legacy line resolves {versions:?}, the 1.7.1 release shipped {pinned}"
        );
    }
}

#[test]
fn test_the_lockfile_parser_reads_packages() {
    let lock = "[[package]]\nname = \"revm\"\nversion = \"27.1.0\"\nsource = \"registry\"\n\n\
                [[package]]\nname = \"revm\"\nversion = \"40.0.3\"\n";
    assert_eq!(locked_packages(lock), [("revm", "27.1.0"), ("revm", "40.0.3")]);
    assert_eq!(compatibility_line("27.1.0"), "27");
    assert_eq!(compatibility_line("0.18.14"), "0.18");
    assert_eq!(compatibility_line("2.0.0-alpha.1"), "2");
}
