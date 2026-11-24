//! Mainnet block corpus: every pinned block replays offline and reproduces both
//! its receipts and its header.
//!
//! `tests/fixtures/corpus/corpus.tar.xz` holds one RPC capture per block, as the
//! member `<N>.cache.json`. `tests/fixtures/corpus/manifest.json` pins each
//! block by number and hash, names its member and that member's SHA-256, and
//! pins the archive's own SHA-256. The manifest is the single source of truth:
//! the replay test walks its entries, never the archive, and the consistency
//! test requires the archive to hold exactly the members it lists, byte for
//! byte.
//!
//! Each block is replayed with
//! `mega-evme replay --rpc.replay-file <capture> --block N --verify-receipt --verify-block --json`,
//! which must exit `0` with one matching receipt verdict per transaction and one
//! matching block verdict naming the pinned hash. The archive is extracted once,
//! the blocks then run concurrently inside one test, and every failing block is
//! reported at the end rather than only the first.
//!
//! See the corpus README for how the blocks were chosen, the SALT bucket
//! capacities they replay with, and how to recapture one.

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, OnceLock,
    },
};

use mega_evm::{mainnet_hardforks, MegaHardforks, MAINNET_CHAIN_ID};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The corpus manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    /// Chain every block belongs to.
    chain_id: u64,
    /// Where the captures' SALT bucket capacities come from.
    salt: String,
    /// File name of the archive holding every capture, in the corpus directory.
    archive: String,
    /// SHA-256 of that archive, lowercase hex.
    sha256: String,
    /// The pinned blocks, in ascending order.
    blocks: Vec<Entry>,
}

/// One pinned block of the corpus.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    /// Block number.
    number: u64,
    /// Block hash, as the chain sealed it.
    hash: String,
    /// Spec the block executes under on mainnet.
    spec: String,
    /// Transactions the block body lists.
    tx_count: usize,
    /// Name of the block's capture in the archive.
    member: String,
    /// SHA-256 of that capture, lowercase hex.
    sha256: String,
    /// Why the block is in the corpus, for the blocks that cover a particular
    /// shape.
    note: Option<String>,
}

/// Directory the corpus lives in, relative to the fixtures directory.
const CORPUS: &str = "corpus";

fn manifest() -> Manifest {
    let path = common::fixtures_dir().join(CORPUS).join("manifest.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("malformed {}: {e}", path.display()))
}

/// The directory the archive's captures are extracted into, extracted once per
/// test binary.
fn captures(manifest: &Manifest) -> &'static Path {
    static CAPTURES: OnceLock<PathBuf> = OnceLock::new();
    CAPTURES.get_or_init(|| common::fixture_archive(&format!("{CORPUS}/{}", manifest.archive)))
}

fn sha256_hex(path: &Path) -> String {
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The members the archive holds, in archive order.
fn archive_members(archive: &Path) -> Vec<String> {
    let output = Command::new("tar")
        .arg("-tJf")
        .arg(archive)
        .output()
        .unwrap_or_else(|e| panic!("failed to run tar on {}: {e}", archive.display()));
    assert!(output.status.success(), "failed to list {}", archive.display());
    String::from_utf8(output.stdout)
        .expect("member names are utf-8")
        .lines()
        .map(str::to_string)
        .collect()
}

/// The archive is the one the manifest pins, it holds exactly the manifest's
/// members in block order, every member is the capture the manifest pins, and
/// the manifest describes mainnet blocks in order.
#[test]
fn test_corpus_manifest_matches_the_archive() {
    let manifest = manifest();
    assert_eq!(manifest.chain_id, MAINNET_CHAIN_ID, "the corpus is a mainnet corpus");
    assert_eq!(manifest.salt, "default-minimum", "the captures carry no bucket capacities");
    assert!(!manifest.blocks.is_empty(), "the corpus pins at least one block");

    let numbers: Vec<u64> = manifest.blocks.iter().map(|entry| entry.number).collect();
    let mut ordered = numbers.clone();
    ordered.sort_unstable();
    ordered.dedup();
    assert_eq!(numbers, ordered, "blocks are listed once each, in ascending order");

    let archive = common::fixtures_dir().join(CORPUS).join(&manifest.archive);
    assert_eq!(
        sha256_hex(&archive),
        manifest.sha256,
        "{} does not match the digest the manifest pins",
        manifest.archive
    );
    let listed: Vec<String> = manifest.blocks.iter().map(|entry| entry.member.clone()).collect();
    assert_eq!(
        archive_members(&archive),
        listed,
        "the archive must hold exactly the manifest's members, in block order"
    );

    let captures = captures(&manifest);
    for entry in &manifest.blocks {
        assert_eq!(entry.member, format!("{}.cache.json", entry.number), "{entry:?}");
        assert_eq!(
            sha256_hex(&captures.join(&entry.member)),
            entry.sha256,
            "{} does not match the digest the manifest pins",
            entry.member
        );
        assert!(entry.tx_count > 0, "every block starts with its L1 attributes deposit");
        assert!(
            entry.note.as_deref().is_none_or(|note| !note.trim().is_empty()),
            "a note says something or is left out: {entry:?}"
        );
    }
}

/// Replay one block from its extracted capture and return why it failed, if it
/// did.
fn check_block(entry: &Entry, captures: &Path) -> Result<(), String> {
    let capture = captures.join(&entry.member);
    check_capture(entry, &capture)?;

    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", "--rpc.replay-file"])
        .arg(&capture)
        .args(["--block", &entry.number.to_string(), "--verify-receipt", "--verify-block"])
        .arg("--json")
        .env_remove("RUST_LOG")
        .output()
        .map_err(|e| format!("failed to run mega-evme: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(0) {
        return Err(format!("exited {:?}; stderr: {}", output.status.code(), stderr.trim()));
    }

    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).map_err(|e| format!("not NDJSON ({e}): {line}")))
        .collect::<Result<_, _>>()?;
    let block_comes_last = lines.last().is_some_and(common::is_block_line);
    let (txs, blocks) = common::split_block_lines(lines);
    if txs.len() != entry.tx_count {
        return Err(format!("{} transaction lines, expected {}", txs.len(), entry.tx_count));
    }
    for (index, line) in txs.iter().enumerate() {
        if line.get("error").is_some() ||
            line["verification"] != serde_json::json!({ "match": true }) ||
            line["tx_index"].as_u64() != Some(index as u64) ||
            line["block_number"].as_u64() != Some(entry.number)
        {
            return Err(format!("transaction {index} did not verify: {line}"));
        }
    }
    let [block] = blocks.as_slice() else {
        return Err(format!("{} block lines, expected exactly one", blocks.len()));
    };
    if !block_comes_last {
        return Err("the block line must follow every transaction line".to_string());
    }
    if block["block_hash"].as_str() != Some(entry.hash.as_str()) ||
        block["block_number"].as_u64() != Some(entry.number) ||
        block["block_verification"] != serde_json::json!({ "match": true })
    {
        return Err(format!("the block did not verify: {block}"));
    }
    Ok(())
}

/// Check what the manifest says about a block against its capture: the capture
/// is a mainnet one without bucket capacities, it serves the pinned block, and
/// the block's timestamp puts it under the spec the manifest names.
fn check_capture(entry: &Entry, capture: &Path) -> Result<(), String> {
    let raw = std::fs::read_to_string(capture).map_err(|e| format!("unreadable capture: {e}"))?;
    let envelope: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("malformed capture: {e}"))?;
    if envelope["chain_id"].as_u64() != Some(MAINNET_CHAIN_ID) {
        return Err(format!("capture chain id is {}", envelope["chain_id"]));
    }
    if envelope["external_env"]["bucket_capacities"].as_array().is_some_and(|b| !b.is_empty()) {
        return Err("capture carries bucket capacities, but the manifest says none".to_string());
    }

    let number = format!("{:#x}", entry.number);
    let header = envelope["cache"]
        .as_array()
        .ok_or("capture has no cache entries")?
        .iter()
        .filter_map(|cached| cached["value"].as_str())
        .filter_map(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .map(|response| response["result"].clone())
        .find(|result| result["number"].as_str() == Some(number.as_str()))
        .ok_or("capture does not serve the pinned block")?;
    if header["hash"].as_str() != Some(entry.hash.as_str()) {
        return Err(format!("capture serves block hash {}", header["hash"]));
    }
    let timestamp = header["timestamp"]
        .as_str()
        .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
        .ok_or("the captured header has no timestamp")?;
    let spec = mainnet_hardforks().spec_id(timestamp).to_string();
    if spec != entry.spec {
        return Err(format!("the manifest names spec {}, the mainnet schedule {spec}", entry.spec));
    }
    Ok(())
}

/// Every block of the manifest replays offline, every receipt verdict matches,
/// and every block reproduces its header.
#[test]
fn test_corpus_blocks_reproduce_their_receipts_and_headers() {
    let manifest = manifest();
    let captures = captures(&manifest);
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(4, usize::from).min(16);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(entry) = manifest.blocks.get(index) else { break };
                if let Err(reason) = check_block(entry, captures) {
                    failures.lock().expect("failure list").push((entry.number, reason));
                }
            });
        }
    });

    let mut failures = failures.into_inner().expect("failure list");
    failures.sort_unstable();
    assert!(
        failures.is_empty(),
        "{} of {} corpus block(s) failed:\n{}",
        failures.len(),
        manifest.blocks.len(),
        failures
            .iter()
            .map(|(number, reason)| format!("  block {number}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
