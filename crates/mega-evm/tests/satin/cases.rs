//! Many outcomes in one snapshot.
//!
//! A test that snapshots more than one outcome collects them by case into a map, and the map
//! refuses a case collected twice ([`by_case`], [`InsertCase::insert_case`]): `BTreeMap`'s own
//! `insert` and `from` keep the last value under a key, so a second case with the name of a first
//! would drop that one from the snapshot without a diff or a failure.

use std::{collections::BTreeMap, fmt::Debug};

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
