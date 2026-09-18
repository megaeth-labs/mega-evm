//! Differential harness of the Satin engine.
//!
//! Runs every scenario of a corpus through `MegaEvm` (the left arm) and through stock revm 43 on
//! its mainnet handler (the right arm, the oracle), and compares what they report field by
//! field: every gas figure, the outcome, the output, the logs and every touched account. Each
//! difference must be explained by an active entry of the deviation registry, and every active
//! entry must explain at least one difference.
//!
//! See the crate README for the corpus, the registry and how the two revms coexist.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use mega_evm::test_utils::Scenario;

pub mod diff;
pub mod mega;
pub mod oracle;
pub mod record;
pub mod registry;

use diff::Difference;
use registry::Registry;

/// Directory of the corpus, one subdirectory per origin.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

/// Path of the deviation registry.
pub fn registry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("deviations.json")
}

/// Loads every `*.json` scenario under `dir` (recursively), sorted by path.
///
/// Every scenario must be valid, its name must be its file stem, and names must be unique.
pub fn load_corpus(dir: &Path) -> Result<Vec<Scenario>, String> {
    let mut paths = Vec::new();
    collect_json(dir, &mut paths)?;
    paths.sort();
    let mut names = BTreeMap::new();
    let mut scenarios = Vec::with_capacity(paths.len());
    for path in paths {
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let scenario: Scenario =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        scenario.validate()?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(scenario.name.as_str()) {
            return Err(format!("{}: the name must be the file stem", path.display()));
        }
        if let Some(other) = names.insert(scenario.name.clone(), path.clone()) {
            return Err(format!("{}: name already used by {}", path.display(), other.display()));
        }
        scenarios.push(scenario);
    }
    Ok(scenarios)
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|err| format!("{}: {err}", dir.display()))?.path();
        if path.is_dir() {
            collect_json(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
    Ok(())
}

/// The outcome of a differential run.
#[derive(Debug, Default)]
pub struct Report {
    /// Number of scenarios run.
    pub scenarios: usize,
    /// Number of fields compared.
    pub compared: usize,
    /// Differences an active registry entry explains, by entry id.
    pub explained: BTreeMap<String, Vec<Difference>>,
    /// Differences no active entry explains, with the retired entry that would have, if any.
    pub unexplained: Vec<(Difference, Option<String>)>,
    /// Active entries that explained nothing.
    pub stale: Vec<String>,
}

impl Report {
    /// Whether every difference is explained and every active entry explained one.
    pub fn is_clean(&self) -> bool {
        self.unexplained.is_empty() && self.stale.is_empty()
    }

    /// The one-line summary printed at the end of a run.
    pub fn summary(&self) -> String {
        let matched: usize = self.explained.values().map(Vec::len).sum();
        let by_entry: Vec<String> =
            self.explained.iter().map(|(id, diffs)| format!("{id} x{}", diffs.len())).collect();
        format!(
            "differential: {} scenarios, {} fields compared, {} deviations matched{}, {} unexplained, {} stale registry entries",
            self.scenarios,
            self.compared,
            matched,
            if by_entry.is_empty() { String::new() } else { format!(" ({})", by_entry.join(", ")) },
            self.unexplained.len(),
            self.stale.len(),
        )
    }

    /// Every unexplained difference and stale entry, one per line.
    pub fn failures(&self) -> String {
        let mut out = String::new();
        for (diff, retired) in &self.unexplained {
            let _ = write!(
                out,
                "unexplained: {} {}: mega={} oracle={}",
                diff.scenario, diff.field, diff.left, diff.right
            );
            if let Some(id) = retired {
                let _ = write!(out, " (matches retired entry {id})");
            }
            out.push('\n');
        }
        for id in &self.stale {
            let _ = writeln!(out, "stale: active registry entry {id} explained no difference");
        }
        out
    }
}

/// Runs every scenario through both arms and checks the differences against `registry`.
pub fn run(scenarios: &[Scenario], registry: &Registry) -> Report {
    let comparisons: Vec<diff::Comparison> = parallel_map(scenarios, |scenario| {
        diff::compare(&scenario.name, &mega::run(scenario), &oracle::run(scenario))
    });
    let mut report = Report { scenarios: scenarios.len(), ..Default::default() };
    for comparison in comparisons {
        report.compared += comparison.compared;
        for difference in comparison.differences {
            match registry.explain(&difference) {
                Some(dev) => report.explained.entry(dev.id.clone()).or_default().push(difference),
                None => {
                    let retired = registry.retired_match(&difference).map(|dev| dev.id.clone());
                    report.unexplained.push((difference, retired));
                }
            }
        }
    }
    report.stale = registry
        .deviations
        .iter()
        .filter(|dev| dev.status == registry::Status::Active)
        .filter(|dev| !report.explained.contains_key(&dev.id))
        .map(|dev| dev.id.clone())
        .collect();
    report
}

/// Maps `f` over `items` on all available cores, keeping the order.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|chunk| scope.spawn(|| chunk.iter().map(&f).collect::<Vec<R>>()))
            .collect();
        handles.into_iter().flat_map(|handle| handle.join().expect("scenario panicked")).collect()
    })
}

/// The `u64` fields of `value` as it serializes to a JSON object, by serialized name.
///
/// Both arms read `ResultGas` through this, so a field the fork adds shows up on the left arm
/// as a difference until the oracle reports the same figure or the registry explains it.
fn serialized_u64_fields(value: &impl serde::Serialize) -> BTreeMap<String, u64> {
    let serde_json::Value::Object(fields) = serde_json::to_value(value).expect("serializable")
    else {
        panic!("expected a JSON object");
    };
    fields
        .into_iter()
        .map(|(name, value)| {
            let value = value.as_u64().unwrap_or_else(|| panic!("{name} is not a u64: {value}"));
            (name, value)
        })
        .collect()
}
