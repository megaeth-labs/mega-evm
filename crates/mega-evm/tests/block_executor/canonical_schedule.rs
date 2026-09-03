//! Pins the canonical per-chain schedules (`block/chain.rs`) to the activation timestamps
//! published in `docs/spec/upgrades/overview.md`.
//!
//! The two are separate copies of the same numbers: the overview is what operators and
//! integrators read, the schedule is what `mega-evme replay` executes. A digit mistyped in
//! either is self-consistent with its own tests, so this test reads the published table and
//! compares every fork on both networks — a backticked timestamp in the overview must be the
//! schedule entry, and a fork the overview marks as not activated (`N/A`, `Not yet scheduled`)
//! must be [`ForkCondition::Never`].

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use alloy_hardforks::ForkCondition;
use mega_evm::{mainnet_hardforks, testnet_hardforks, MegaHardfork, MegaHardforks};

/// Every fork heading in the published upgrade overview and the hardfork it documents. A new
/// fork page must be added here to be pinned; the test fails if the two sets differ.
const FORK_PAGES: &[(&str, MegaHardfork)] = &[
    ("MiniRex", MegaHardfork::MiniRex),
    ("MiniRex1", MegaHardfork::MiniRex1),
    ("MiniRex2", MegaHardfork::MiniRex2),
    ("Rex", MegaHardfork::Rex),
    ("Rex1", MegaHardfork::Rex1),
    ("Rex2", MegaHardfork::Rex2),
    ("Rex3", MegaHardfork::Rex3),
    ("Rex4", MegaHardfork::Rex4),
    ("Rex5", MegaHardfork::Rex5),
    ("Rex6", MegaHardfork::Rex6),
    ("Rex7", MegaHardfork::Rex7),
];

/// Published activation per network as written in the overview: `Some(ts)` for a backticked
/// timestamp, `None` for the prose that marks a fork as not activated on that network.
#[derive(Debug, Default)]
struct Published {
    testnet: Option<u64>,
    mainnet: Option<u64>,
}

#[derive(Clone, Copy)]
enum Network {
    Testnet,
    Mainnet,
}

/// The overview lives outside the crate root, so it is not part of the published package.
/// Returns `None` when this crate is not laid out as a repository checkout (a crates.io tarball
/// or a vendored copy), where the pin has nothing to compare against; in a checkout the file is
/// mandatory and its absence fails the test.
fn overview() -> Option<String> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !workspace_root.join("crates/mega-evm/Cargo.toml").is_file() {
        eprintln!(
            "skipping: not a repository checkout, docs/spec/upgrades/overview.md is not packaged"
        );
        return None;
    }
    let path: PathBuf = workspace_root.join("docs/spec/upgrades/overview.md");
    Some(
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is part of the repository: {e}", path.display())),
    )
}

/// Parses the `## Hardfork History` section. Each `### <Fork>` heading (linked or bare) opens an
/// entry; the line after `{% tab title="Testnet" %}` / `{% tab title="Mainnet" %}` carries either
/// a backticked timestamp or a not-activated marker.
fn parse_published(overview: &str) -> BTreeMap<String, Published> {
    let mut out: BTreeMap<String, Published> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut in_history = false;
    let mut lines = overview.lines();
    while let Some(line) = lines.next() {
        if let Some(heading) = line.strip_prefix("## ") {
            in_history = heading.trim() == "Hardfork History";
            current = None;
            continue;
        }
        if !in_history {
            continue;
        }
        if let Some(heading) = line.strip_prefix("### ") {
            let heading = heading.trim();
            // `### [Rex6](rex6.md)` and `### MiniRex1` both name the fork.
            let label = heading
                .strip_prefix('[')
                .and_then(|rest| rest.split(']').next())
                .unwrap_or(heading);
            out.entry(label.to_string()).or_default();
            current = Some(label.to_string());
            continue;
        }
        let network = match line.trim() {
            "{% tab title=\"Testnet\" %}" => Network::Testnet,
            "{% tab title=\"Mainnet\" %}" => Network::Mainnet,
            _ => continue,
        };
        let body = lines.next().expect("a tab has a body line").trim();
        let timestamp = body.strip_prefix('`').map(|rest| {
            let digits = rest.split('`').next().expect("closing backtick");
            digits.parse::<u64>().unwrap_or_else(|e| panic!("bad timestamp {digits:?}: {e}"))
        });
        let label = current.as_ref().expect("a tab outside any fork heading");
        let entry = out.get_mut(label).expect("entry created at the heading");
        match network {
            Network::Testnet => entry.testnet = timestamp,
            Network::Mainnet => entry.mainnet = timestamp,
        }
    }
    out
}

fn expected(published: Option<u64>) -> ForkCondition {
    published.map_or(ForkCondition::Never, ForkCondition::Timestamp)
}

#[test]
fn test_canonical_schedules_match_published_activation_timestamps() {
    let Some(overview) = overview() else { return };
    let published = parse_published(&overview);

    // Every documented fork is pinned, and every pinned fork is documented.
    let documented: Vec<&str> = published.keys().map(String::as_str).collect();
    let mut pinned: Vec<&str> = FORK_PAGES.iter().map(|(label, _)| *label).collect();
    pinned.sort_unstable();
    assert_eq!(
        documented, pinned,
        "overview.md fork pages and FORK_PAGES differ — extend the pin when a fork page is added"
    );

    let mainnet = mainnet_hardforks();
    let testnet = testnet_hardforks();
    for (label, fork) in FORK_PAGES {
        let doc = &published[*label];
        assert_eq!(
            testnet.mega_fork_activation(*fork),
            expected(doc.testnet),
            "{label}: testnet schedule in block/chain.rs disagrees with overview.md"
        );
        assert_eq!(
            mainnet.mega_fork_activation(*fork),
            expected(doc.mainnet),
            "{label}: mainnet schedule in block/chain.rs disagrees with overview.md"
        );
    }
}
