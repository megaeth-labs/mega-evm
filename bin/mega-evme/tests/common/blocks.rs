//! The recorded mainnet blocks under `tests/fixtures/blocks`, and running the binary on them.
//!
//! The fixtures are three `MegaETH` mainnet blocks (Rex6) in the block cache layout, recorded
//! from the chain: each block with its transactions, the chain's receipts, and the state its
//! parent left for everything the block read. 26400001 is the most varied block of its range (39
//! transactions to 19 distinct call targets), 26400007 holds a transaction that failed on chain,
//! 26400110 a plain value transfer.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use alloy_primitives::{address, Address, B256};
use mega_evme::block::{CachedBlock, PreAccount};
use serde_json::Value;

/// The recorded blocks.
pub(crate) const BLOCKS: [u64; 3] = [26_400_001, 26_400_007, 26_400_110];

/// The EIP-7997 factory, which Satin's pre-block changes deploy and the legacy engine never
/// reads: the one account a Satin replay reads that the recording does not hold.
pub(crate) const FACTORY: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");

/// The recorded block cache.
pub(crate) fn fixtures() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/blocks"))
}

/// Where block `number` lives in the cache at `dir`.
pub(crate) fn block_path(dir: &Path, number: u64) -> PathBuf {
    dir.join("blocks").join((number / 10_000).to_string()).join(format!("{number}.json.zst"))
}

/// Block `number` of the cache at `dir`.
pub(crate) fn read_block(dir: &Path, number: u64) -> CachedBlock {
    let compressed = std::fs::read(block_path(dir, number)).unwrap();
    serde_json::from_slice(&zstd::decode_all(compressed.as_slice()).unwrap()).unwrap()
}

/// Writes `block` as block `number` of the cache at `dir`.
pub(crate) fn write_block(dir: &Path, number: u64, block: &CachedBlock) {
    let json = serde_json::to_vec(block).unwrap();
    std::fs::write(block_path(dir, number), zstd::encode_all(json.as_slice(), 3).unwrap()).unwrap();
}

/// The code with `hash` from the recorded code store.
pub(crate) fn code_of(hash: B256) -> Vec<u8> {
    let hex = hash.to_string();
    std::fs::read(fixtures().join("codes").join(&hex[2..4]).join(format!("{}.bin", &hex[2..])))
        .unwrap()
}

/// A copy of the recorded block cache, so no test can write to the fixtures.
pub(crate) fn cache_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(fixtures(), dir.path());
    dir
}

/// A copy of the recorded block cache in which every block's pre-state holds the factory, absent:
/// what a Satin replay needs offline. `block_replay.rs` shows the factory's state changes no
/// transaction row.
pub(crate) fn cache_copy_with_absent_factory() -> tempfile::TempDir {
    let dir = cache_copy();
    for number in BLOCKS {
        let mut block = read_block(dir.path(), number);
        block.prestate.accounts.insert(FACTORY, PreAccount::default());
        write_block(dir.path(), number, &block);
    }
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// What a run of the binary printed.
pub(crate) struct Run {
    /// The exit code.
    pub(crate) code: i32,
    /// Standard output.
    pub(crate) stdout: String,
    /// Standard error.
    pub(crate) stderr: String,
}

impl Run {
    /// The JSON lines of standard output.
    pub(crate) fn records(&self) -> Vec<Value> {
        self.stdout.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }
}

/// Runs `mega-evme` with `args` on a thread of its own, so a test's runtime can serve it a mock.
pub(crate) fn run_evme<S: AsRef<str>>(args: &[S]) -> Run {
    let args: Vec<String> = args.iter().map(|a| a.as_ref().to_string()).collect();
    std::thread::spawn(move || {
        let output = Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(&args).output().unwrap();
        Run {
            code: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    })
    .join()
    .unwrap()
}
