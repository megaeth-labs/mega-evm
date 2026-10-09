//! Many outcomes in one snapshot.
//!
//! A test that snapshots more than one outcome collects them by case into a map, and the map
//! refuses a case collected twice ([`by_case`], [`InsertCase::insert_case`]): `BTreeMap`'s own
//! `insert` and `from` keep the last value under a key, so a second case with the name of a first
//! would drop that one from the snapshot without a diff or a failure.
//!
//! A test with more outcomes than a reviewer can read whole snapshots each one's summary line
//! instead ([`assert_summaries_snapshot!`](crate::assert_summaries_snapshot)). The line ends with
//! a digest of the full view, so a change to anything the line does not show still fails the
//! snapshot; to read such a change, dump the full views at the two trees and diff them. A sweep
//! of more per-case lines than a snapshot can hold is pinned as its distinct lines and a digest
//! ([`sweep_digest`]), and dumped the same way.
//!
//! The dumps are opt-in: with `MEGA_SNAPSHOT_DUMP=<dir>` set (an absolute path, since the tests
//! run in the package's directory), each test writes its full views to
//! `<dir>/<snapshot name>.json` and each sweep its lines to `<dir>/<snapshot name>.<sweep>.txt`,
//! where the snapshot name is the `.snap` file's, without the extension.

use std::{collections::BTreeMap, fmt::Debug, path::PathBuf};

use alloy_primitives::{keccak256, B256};
use mega_evm::test_utils::{OutcomeSummary, OutcomeView};
use serde::Serialize;

/// The variable naming the directory the full views and the sweeps' lines are dumped to.
pub(crate) const DUMP_ENV_VAR: &str = "MEGA_SNAPSHOT_DUMP";

/// The map of `cases`, by case.
///
/// # Panics
///
/// If two cases have the same key.
#[track_caller]
pub(crate) fn by_case<K: Ord + Debug, V>(
    cases: impl IntoIterator<Item = (K, V)>,
) -> BTreeMap<K, V> {
    let mut map = BTreeMap::new();
    for (case, value) in cases {
        map.insert_case(case, value);
    }
    map
}

/// Adds a case to a map of cases, refusing one already there.
pub(crate) trait InsertCase<K, V> {
    /// Adds `value` under `case`.
    ///
    /// # Panics
    ///
    /// If the map holds `case` already.
    fn insert_case(&mut self, case: K, value: V);
}

impl<K: Ord + Debug, V> InsertCase<K, V> for BTreeMap<K, V> {
    #[track_caller]
    fn insert_case(&mut self, case: K, value: V) {
        assert!(
            !self.contains_key(&case),
            "the case {case:?} is collected twice: one of the two would fall out of the snapshot"
        );
        self.insert(case, value);
    }
}

/* ---------- summaries ---------- */

/// Outcome views by case, and what a snapshot holds in their place: each view's summary line,
/// under the same keys.
pub(crate) trait Summarize {
    /// The summaries, shaped as the views are.
    type Summary: Serialize;

    /// The summary of every view.
    fn summarize(&self) -> Self::Summary;
}

impl Summarize for OutcomeView {
    type Summary = OutcomeSummary;

    fn summarize(&self) -> OutcomeSummary {
        self.summary()
    }
}

impl<K: Clone + Ord + Serialize, V: Summarize> Summarize for BTreeMap<K, V> {
    type Summary = BTreeMap<K, V::Summary>;

    fn summarize(&self) -> Self::Summary {
        self.iter().map(|(case, view)| (case.clone(), view.summarize())).collect()
    }
}

/// Snapshots the summary line of every outcome view in `views` — a map of views by case, or a
/// map of such maps — under the same keys, and dumps the views themselves
/// ([`dump_views`]) under the snapshot's name.
#[macro_export]
macro_rules! assert_summaries_snapshot {
    ($views:expr $(,)?) => {{
        let views = $views;
        crate::cases::dump_views(&crate::snapshot_name!(), views);
        let summaries = &crate::cases::Summarize::summarize(views);
        crate::assert_sorted_json_snapshot!(summaries);
    }};
}

/// The name insta gives the snapshot of the test this expands in, which names its dumps: the
/// test's module path, then its name without the `test_` prefix, joined by `__`.
#[macro_export]
macro_rules! snapshot_name {
    () => {{
        fn here() {}
        crate::cases::snapshot_name_of(::std::any::type_name_of_val(&here))
    }};
}

/// The snapshot name of the test whose function `here` has the path `here`.
pub(crate) fn snapshot_name_of(here: &str) -> String {
    let mut path = here.strip_suffix("::here").expect("the path of a function named `here`");
    while let Some(outer) = path.strip_suffix("::{{closure}}") {
        path = outer;
    }
    let (module, test) = path.rsplit_once("::").expect("a test inside a module");
    let test = test.strip_prefix("test_").unwrap_or(test);
    format!("{}__{test}", module.replace("::", "__"))
}

/// With [`DUMP_ENV_VAR`] set, writes `views` as pretty JSON to `<dir>/<name>.json`.
pub(crate) fn dump_views(name: &str, views: &impl Serialize) {
    let json = serde_json::to_string_pretty(views).expect("the views serialize");
    dump(&format!("{name}.json"), &(json + "\n"));
}

/// With [`DUMP_ENV_VAR`] set, writes `contents` to `<dir>/<file>`.
fn dump(file: &str, contents: &str) {
    let Some(dir) = std::env::var_os(DUMP_ENV_VAR) else { return };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("the dump directory can be made");
    std::fs::write(dir.join(file), contents).expect("the dump can be written");
}

/* ---------- sweeps ---------- */

/// A sweep of per-case lines too many to snapshot one by one: the distinct lines with how many
/// cases produced each, a keccak256 over every case's line, keyed by its case and in case order,
/// so that a change to any one case changes it, and the case count.
#[derive(Debug, Serialize)]
pub(crate) struct SweepDigest {
    distinct: BTreeMap<String, usize>,
    digest: B256,
    count: usize,
}

/// The sweep of `cases`, whose keyed lines are dumped ([`DUMP_ENV_VAR`]) to `<dir>/<name>.txt`.
pub(crate) fn sweep_digest(
    name: &str,
    cases: impl IntoIterator<Item = (String, String)>,
) -> SweepDigest {
    let mut distinct = BTreeMap::new();
    let mut keyed = String::new();
    let mut count = 0;
    for (case, line) in cases {
        *distinct.entry(line.clone()).or_insert(0) += 1;
        keyed.push_str(&case);
        keyed.push_str(": ");
        keyed.push_str(&line);
        keyed.push('\n');
        count += 1;
    }
    dump(&format!("{name}.txt"), &keyed);
    SweepDigest { distinct, digest: keccak256(keyed.as_bytes()), count }
}

/* ---------- the helpers' own tests ---------- */

/// A dump is named after the snapshot of the test it is made in, as insta names that snapshot.
#[test]
fn test_a_dump_is_named_after_its_snapshot() {
    assert_eq!(crate::snapshot_name!(), "satin__cases__a_dump_is_named_after_its_snapshot");
    let in_a_closure = || crate::snapshot_name!();
    assert_eq!(in_a_closure(), "satin__cases__a_dump_is_named_after_its_snapshot");
}

#[test]
fn test_a_map_of_cases_holds_each_case_once() {
    let mut map = by_case([("b", 2), ("a", 1)]);
    map.insert_case("c", 3);
    assert_eq!(map, BTreeMap::from([("a", 1), ("b", 2), ("c", 3)]));
}

#[test]
#[should_panic(expected = "the case \"a\" is collected twice")]
fn test_a_case_inserted_twice_is_refused() {
    let mut map = by_case([("a", 1)]);
    map.insert_case("a", 2);
}

#[test]
#[should_panic(expected = "the case \"a\" is collected twice")]
fn test_a_map_of_cases_refuses_a_repeated_case() {
    by_case([("a", 1), ("b", 2), ("a", 3)]);
}
