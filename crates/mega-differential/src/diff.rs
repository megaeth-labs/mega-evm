//! Field-by-field comparison of two scenario records.

use std::{collections::BTreeSet, fmt::Display};

use crate::record::{AccountRecord, ScenarioRecord, TxRecord, ABSENT};

/// One field on which the two arms disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// Scenario name.
    pub scenario: String,
    /// Field path, e.g. `tx[0].gas.gas_spent` or `tx[1].state[0x…].storage[0x1]`.
    pub field: String,
    /// The value `MegaEvm` reported.
    pub left: String,
    /// The value the oracle reported.
    pub right: String,
}

/// The differences between two records of one scenario, and how many fields were compared.
#[derive(Debug, Default)]
pub struct Comparison {
    /// Fields on which the arms disagree.
    pub differences: Vec<Difference>,
    /// Number of fields compared.
    pub compared: usize,
}

impl Comparison {
    fn field<T: PartialEq + Display>(&mut self, scenario: &str, field: String, left: T, right: T) {
        self.compared += 1;
        if left != right {
            self.differences.push(Difference {
                scenario: scenario.to_string(),
                field,
                left: left.to_string(),
                right: right.to_string(),
            });
        }
    }
}

/// Compares the record `MegaEvm` produced (`left`) with the oracle's (`right`).
pub fn compare(scenario: &str, left: &ScenarioRecord, right: &ScenarioRecord) -> Comparison {
    let mut cmp = Comparison::default();
    cmp.field(scenario, "tx_count".into(), left.len(), right.len());
    for (i, (l, r)) in left.iter().zip(right).enumerate() {
        compare_tx(&mut cmp, scenario, &format!("tx[{i}]"), l, r);
    }
    cmp
}

fn compare_tx(cmp: &mut Comparison, scenario: &str, at: &str, l: &TxRecord, r: &TxRecord) {
    cmp.field(scenario, format!("{at}.outcome"), &l.outcome, &r.outcome);
    for name in l.gas.keys().chain(r.gas.keys()).collect::<BTreeSet<_>>() {
        cmp.field(
            scenario,
            format!("{at}.gas.{name}"),
            render(l.gas.get(name)),
            render(r.gas.get(name)),
        );
    }
    cmp.field(scenario, format!("{at}.output"), &l.output, &r.output);
    cmp.field(
        scenario,
        format!("{at}.created"),
        render(l.created.map(|address| format!("{address:#x}"))),
        render(r.created.map(|address| format!("{address:#x}"))),
    );
    cmp.field(scenario, format!("{at}.logs.len"), l.logs.len(), r.logs.len());
    for (j, (ll, rl)) in l.logs.iter().zip(&r.logs).enumerate() {
        cmp.field(scenario, format!("{at}.logs[{j}]"), ll.render(), rl.render());
    }
    for address in l.state.keys().chain(r.state.keys()).collect::<BTreeSet<_>>() {
        let path = format!("{at}.state[{address:#x}]");
        match (l.state.get(address), r.state.get(address)) {
            (Some(la), Some(ra)) => compare_account(cmp, scenario, &path, la, ra),
            (la, ra) => cmp.field(
                scenario,
                path,
                la.map_or_else(|| ABSENT.to_string(), AccountRecord::render),
                ra.map_or_else(|| ABSENT.to_string(), AccountRecord::render),
            ),
        }
    }
}

fn compare_account(
    cmp: &mut Comparison,
    scenario: &str,
    at: &str,
    l: &AccountRecord,
    r: &AccountRecord,
) {
    cmp.field(scenario, format!("{at}.created"), l.created, r.created);
    cmp.field(scenario, format!("{at}.selfdestructed"), l.selfdestructed, r.selfdestructed);
    cmp.field(scenario, format!("{at}.balance"), l.balance, r.balance);
    cmp.field(scenario, format!("{at}.nonce"), l.nonce, r.nonce);
    cmp.field(scenario, format!("{at}.code_hash"), l.code_hash, r.code_hash);
    for slot in l.storage.keys().chain(r.storage.keys()).collect::<BTreeSet<_>>() {
        cmp.field(
            scenario,
            format!("{at}.storage[{slot:#x}]"),
            render(l.storage.get(slot).map(|value| format!("{value:#x}"))),
            render(r.storage.get(slot).map(|value| format!("{value:#x}"))),
        );
    }
}

/// Renders an optional value, with [`ABSENT`] for `None`.
fn render<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| ABSENT.to_string(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::LogRecord;
    use alloy_primitives::{address, Address, Bytes, U256};
    use std::collections::BTreeMap;

    const A: Address = address!("0x0000000000000000000000000000000000000aaa");

    fn tx() -> TxRecord {
        TxRecord {
            outcome: "success".into(),
            gas: BTreeMap::from([("gas_spent".to_string(), 21_000)]),
            state: BTreeMap::from([(A, AccountRecord { nonce: 1, ..Default::default() })]),
            ..Default::default()
        }
    }

    #[test]
    fn test_compare_equal_records_reports_nothing() {
        let cmp = compare("s", &vec![tx()], &vec![tx()]);
        assert!(cmp.differences.is_empty());
        // tx_count, outcome, one gas field, output, created, logs.len, five account fields.
        assert_eq!(cmp.compared, 11);
    }

    #[test]
    fn test_compare_reports_each_differing_field() {
        let mut right = tx();
        right.gas.insert("gas_spent".into(), 21_001);
        right.gas.insert("reservoir_remaining".into(), 0);
        right.state.get_mut(&A).unwrap().storage.insert(U256::from(1), U256::from(2));
        right.logs.push(LogRecord { address: A, topics: vec![], data: Bytes::new() });

        let fields: Vec<(String, String, String)> = compare("s", &vec![tx()], &vec![right])
            .differences
            .into_iter()
            .map(|d| (d.field, d.left, d.right))
            .collect();

        let at = format!("tx[0].state[{A:#x}]");
        assert_eq!(
            fields,
            vec![
                ("tx[0].gas.gas_spent".into(), "21000".into(), "21001".into()),
                ("tx[0].gas.reservoir_remaining".into(), ABSENT.into(), "0".into()),
                ("tx[0].logs.len".into(), "0".into(), "1".into()),
                (format!("{at}.storage[0x1]"), ABSENT.into(), "0x2".into()),
            ]
        );
    }

    #[test]
    fn test_compare_reports_an_account_only_one_arm_touched() {
        let mut right = tx();
        right.state.clear();
        let differences = compare("s", &vec![tx()], &vec![right]).differences;
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].field, "tx[0].state[0x0000000000000000000000000000000000000aaa]");
        assert!(differences[0].left.contains("nonce=1"));
        assert_eq!(differences[0].right, ABSENT);
    }

    #[test]
    fn test_compare_reports_a_missing_transaction() {
        let differences = compare("s", &vec![tx(), tx()], &vec![tx()]).differences;
        assert_eq!(differences.len(), 1);
        assert_eq!((differences[0].left.as_str(), differences[0].right.as_str()), ("2", "1"));
    }
}
