//! Differential harness of the Satin engine.
//!
//! Runs every scenario of a corpus through `MegaEvm` (the left arm) and through stock revm 43 on
//! its mainnet handler (the right arm, the oracle), and compares what they report field by
//! field: every gas figure, the outcome, the output, the logs and every touched account. Each
//! difference must be explained by an active entry of the deviation registry, and every effect
//! an active entry lists must explain at least one difference.
//!
//! See the crate README for the corpus, the registry and how the two revms coexist.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use mega_evm::test_utils::Scenario;

#[macro_use]
pub mod record;
pub mod diff;
pub mod mega;
pub mod oracle;
pub mod registry;

use diff::Difference;
use registry::{EffectRef, Registry};

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
    /// Effects of active entries that explained nothing, with their field pattern.
    pub stale: Vec<(EffectRef, String)>,
}

impl Report {
    /// Whether every difference is explained and every effect of every active entry explained
    /// one.
    pub fn is_clean(&self) -> bool {
        self.unexplained.is_empty() && self.stale.is_empty()
    }

    /// The one-line summary printed at the end of a run.
    pub fn summary(&self) -> String {
        let matched: usize = self.explained.values().map(Vec::len).sum();
        let by_entry: Vec<String> =
            self.explained.iter().map(|(id, diffs)| format!("{id} x{}", diffs.len())).collect();
        format!(
            "differential: {} scenarios, {} fields compared, {} deviations matched{}, {} unexplained, {} stale registry effects",
            self.scenarios,
            self.compared,
            matched,
            if by_entry.is_empty() { String::new() } else { format!(" ({})", by_entry.join(", ")) },
            self.unexplained.len(),
            self.stale.len(),
        )
    }

    /// Every unexplained difference and stale effect, one per line.
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
        for ((id, i), field) in &self.stale {
            let _ =
                writeln!(out, "stale: effect {i} ({field}) of active entry {id} explained nothing");
        }
        out
    }
}

/// Runs every scenario through both arms and checks the differences against `registry`.
pub fn run(scenarios: &[Scenario], registry: &Registry) -> Report {
    let comparisons = parallel_map(scenarios, |scenario| {
        diff::compare(&scenario.name, &mega::run(scenario), &oracle::run(scenario))
    });
    check(comparisons, registry)
}

/// Checks the comparisons of a run against `registry`.
pub fn check(comparisons: Vec<diff::Comparison>, registry: &Registry) -> Report {
    let mut report = Report { scenarios: comparisons.len(), ..Default::default() };
    let mut hit = BTreeSet::new();
    for comparison in comparisons {
        report.compared += comparison.compared;
        for difference in comparison.differences {
            match registry.explain(&difference) {
                Some(effect) => {
                    report.explained.entry(effect.0.clone()).or_default().push(difference);
                    hit.insert(effect);
                }
                None => {
                    let retired = registry.retired_match(&difference).map(|(id, _)| id);
                    report.unexplained.push((difference, retired));
                }
            }
        }
    }
    report.stale = registry
        .active_effects()
        .filter(|(effect, _)| !hit.contains(effect))
        .map(|(effect, spec)| (effect, spec.field.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use diff::Comparison;

    fn registry() -> Registry {
        Registry::from_json(
            r#"{"version": 1, "deviations": [
                {"id": "fee", "status": "active", "scenario": "*", "mechanism": "m", "reason": "r",
                 "effects": [
                    {"field": "tx[*].gas.gas_spent", "left": "2", "right": "1"},
                    {"field": "tx[*].output", "left": "0x", "right": "0x01"}]},
                {"id": "old", "status": "retired", "scenario": "*", "mechanism": "m", "reason": "r",
                 "retired_reason": "gone",
                 "effects": [{"field": "tx[*].logs.len", "left": "*", "right": "*"}]}
            ]}"#,
        )
        .unwrap()
    }

    fn comparison(diffs: &[(&str, &str, &str)]) -> Comparison {
        Comparison {
            scenario: "s".into(),
            differences: diffs
                .iter()
                .map(|(field, left, right)| Difference {
                    scenario: "s".into(),
                    field: (*field).into(),
                    left: (*left).into(),
                    right: (*right).into(),
                })
                .collect(),
            compared: 10,
        }
    }

    #[test]
    fn test_check_is_clean_when_every_difference_and_effect_is_matched() {
        let report = check(
            vec![
                comparison(&[("tx[0].gas.gas_spent", "2", "1")]),
                comparison(&[("tx[1].output", "0x", "0x01")]),
            ],
            &registry(),
        );
        assert!(report.is_clean(), "{}", report.failures());
        assert_eq!((report.scenarios, report.compared), (2, 20));
        assert_eq!(report.explained["fee"].len(), 2);
    }

    #[test]
    fn test_check_fails_on_an_unexplained_difference() {
        let report = check(
            vec![comparison(&[
                ("tx[0].gas.gas_spent", "2", "1"),
                ("tx[0].output", "0x", "0x01"),
                ("tx[0].gas.gas_spent", "3", "1"),
                ("tx[0].logs.len", "1", "0"),
            ])],
            &registry(),
        );
        assert!(!report.is_clean());
        assert!(report.stale.is_empty());
        let failures = report.failures();
        assert!(failures.contains("unexplained: s tx[0].gas.gas_spent: mega=3 oracle=1\n"));
        assert!(failures.contains("tx[0].logs.len: mega=1 oracle=0 (matches retired entry old)"));
    }

    #[test]
    fn test_check_fails_on_an_effect_that_explains_nothing() {
        let report = check(vec![comparison(&[("tx[0].gas.gas_spent", "2", "1")])], &registry());
        assert!(!report.is_clean());
        assert!(report.unexplained.is_empty());
        assert_eq!(report.stale, vec![(("fee".to_string(), 1), "tx[*].output".to_string())]);
    }
}
