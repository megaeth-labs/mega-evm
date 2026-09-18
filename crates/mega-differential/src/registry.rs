//! The registry of accepted differences.
//!
//! An entry names one mechanism and lists every effect it has on the compared fields, each as a
//! field and the two values. Every difference between the arms must be explained by an effect
//! of an active entry, and every effect of an active entry must explain at least one difference;
//! either failure fails the run. A mechanism that stops producing an effect therefore shows up as
//! a stale effect, to be removed or its entry retired, instead of lingering as a blanket excuse.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::diff::Difference;

/// Version of the registry format this crate reads.
pub const REGISTRY_VERSION: u64 = 1;

/// The registry file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// Format version; must be [`REGISTRY_VERSION`].
    pub version: u64,
    /// The entries.
    pub deviations: Vec<Deviation>,
}

/// An accepted difference between `MegaEvm` and the oracle: one mechanism and its effects.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deviation {
    /// Stable identifier, used in reports.
    pub id: String,
    /// Whether the entry is in force.
    pub status: Status,
    /// Scenario names the entry covers (`*` matches any run of characters).
    pub scenario: String,
    /// The mechanism that makes the arms differ.
    pub mechanism: String,
    /// Why the difference is correct.
    pub reason: String,
    /// What the mechanism changes, field by field.
    pub effects: Vec<Effect>,
    /// Why the entry was retired; required for a retired entry.
    #[serde(default)]
    pub retired_reason: Option<String>,
}

/// One field a mechanism changes, and the value on each arm.
///
/// Each part is a pattern in which `*` matches any run of characters; every other character
/// stands for itself.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    /// Field path.
    pub field: String,
    /// The value `MegaEvm` reports.
    pub left: String,
    /// The value the oracle reports.
    pub right: String,
}

/// Status of a registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// In force: its effects explain differences, and each must explain at least one.
    Active,
    /// Kept for the record; it explains nothing.
    Retired,
}

/// An effect of an entry, by entry id and position in its `effects`.
pub type EffectRef = (String, usize);

impl Registry {
    /// Parses a registry and checks its entries are well formed.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let registry: Self = serde_json::from_str(json).map_err(|err| err.to_string())?;
        if registry.version != REGISTRY_VERSION {
            return Err(format!(
                "registry version {} (this harness reads {REGISTRY_VERSION})",
                registry.version
            ));
        }
        let mut ids = BTreeSet::new();
        for dev in &registry.deviations {
            if !ids.insert(dev.id.as_str()) {
                return Err(format!("duplicate registry id {}", dev.id));
            }
            if dev.mechanism.trim().is_empty() || dev.reason.trim().is_empty() {
                return Err(format!("{}: an entry names its mechanism and its reason", dev.id));
            }
            if dev.effects.is_empty() {
                return Err(format!("{}: an entry lists at least one effect", dev.id));
            }
            if (dev.status == Status::Retired) != dev.retired_reason.is_some() {
                return Err(format!(
                    "{}: a retired entry, and only one, has retired_reason",
                    dev.id
                ));
            }
        }
        Ok(registry)
    }

    /// The effect of an active entry that explains `diff`, if any.
    pub fn explain(&self, diff: &Difference) -> Option<EffectRef> {
        self.find(Status::Active, diff)
    }

    /// The effect of a retired entry that would have explained `diff`, to point at in a failure
    /// report.
    pub fn retired_match(&self, diff: &Difference) -> Option<EffectRef> {
        self.find(Status::Retired, diff)
    }

    /// Every effect of every active entry.
    pub fn active_effects(&self) -> impl Iterator<Item = (EffectRef, &Effect)> {
        self.deviations.iter().filter(|dev| dev.status == Status::Active).flat_map(|dev| {
            dev.effects.iter().enumerate().map(|(i, effect)| ((dev.id.clone(), i), effect))
        })
    }

    fn find(&self, status: Status, diff: &Difference) -> Option<EffectRef> {
        self.deviations
            .iter()
            .filter(|dev| dev.status == status && glob(&dev.scenario, &diff.scenario))
            .find_map(|dev| {
                let i = dev.effects.iter().position(|effect| effect.matches(diff))?;
                Some((dev.id.clone(), i))
            })
    }
}

impl Effect {
    /// Whether this effect's patterns cover the field and values of `diff`.
    pub fn matches(&self, diff: &Difference) -> bool {
        glob(&self.field, &diff.field) &&
            glob(&self.left, &diff.left) &&
            glob(&self.right, &diff.right)
    }
}

/// Matches `text` against `pattern`, where `*` stands for any run of characters (including
/// none) and every other character stands for itself.
pub fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else { return false };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else { return rest.is_empty() };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAS_EFFECT: &str = r#"{"field": "tx[*].gas.gas_spent", "left": "*", "right": "21000"}"#;
    const OUTPUT_EFFECT: &str = r#"{"field": "tx[*].output", "left": "0x", "right": "0x01"}"#;

    fn entry(id: &str, status: &str, effects: &[&str], extra: &str) -> String {
        format!(
            r#"{{"id": "{id}", "status": "{status}", "scenario": "call_*", "mechanism": "m",
                "reason": "r", "effects": [{}]{extra}}}"#,
            effects.join(",")
        )
    }

    fn registry(entries: &[String]) -> Result<Registry, String> {
        Registry::from_json(&format!(r#"{{"version": 1, "deviations": [{}]}}"#, entries.join(",")))
    }

    fn diff(scenario: &str, field: &str, left: &str, right: &str) -> Difference {
        Difference {
            scenario: scenario.into(),
            field: field.into(),
            left: left.into(),
            right: right.into(),
        }
    }

    #[test]
    fn test_glob() {
        assert!(glob("*", ""));
        assert!(glob("abc", "abc"));
        assert!(!glob("abc", "abcd"));
        assert!(glob("a*c", "abbbc"));
        assert!(glob("a*c", "ac"));
        assert!(!glob("a*c", "ab"));
        assert!(glob("*b*", "abc"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(!glob("a*b*c", "aXcYb"));
        // The suffix may not reuse characters the prefix consumed.
        assert!(!glob("ab*ba", "aba"));
    }

    #[test]
    fn test_active_entry_explains_matching_differences_only() {
        let registry = registry(&[entry("d", "active", &[GAS_EFFECT, OUTPUT_EFFECT], "")]).unwrap();
        let gas = |scenario, right| diff(scenario, "tx[0].gas.gas_spent", "21100", right);

        assert_eq!(registry.explain(&gas("call_1", "21000")), Some(("d".into(), 0)));
        assert_eq!(
            registry.explain(&diff("call_1", "tx[1].output", "0x", "0x01")),
            Some(("d".into(), 1))
        );
        assert!(registry.explain(&gas("create_1", "21000")).is_none(), "scenario pattern");
        assert!(registry.explain(&gas("call_1", "21001")).is_none(), "right value");
        assert!(registry.explain(&diff("call_1", "tx[0].output", "0x", "0x02")).is_none());
    }

    #[test]
    fn test_retired_entry_explains_nothing() {
        let registry =
            registry(&[entry("d", "retired", &[GAS_EFFECT], r#", "retired_reason": "gone""#)])
                .unwrap();
        let gas = diff("call_1", "tx[0].gas.gas_spent", "21100", "21000");
        assert!(registry.explain(&gas).is_none());
        assert_eq!(registry.retired_match(&gas), Some(("d".into(), 0)));
        assert_eq!(registry.active_effects().count(), 0);
    }

    #[test]
    fn test_registry_rejects_malformed_entries() {
        let reject = |entries: &[String]| registry(entries).unwrap_err();
        assert!(reject(&[entry("d", "retired", &[GAS_EFFECT], "")]).contains("retired_reason"));
        assert!(reject(&[entry("d", "active", &[GAS_EFFECT], r#", "retired_reason": "x""#)])
            .contains("retired_reason"));
        assert!(reject(&[entry("d", "active", &[], "")]).contains("effect"));
        assert!(reject(&[
            entry("d", "active", &[GAS_EFFECT], ""),
            entry("d", "active", &[GAS_EFFECT], "")
        ])
        .contains("duplicate"));
        assert!(Registry::from_json(r#"{"version": 2, "deviations": []}"#).is_err());
        assert!(registry(&[entry("d", "paused", &[GAS_EFFECT], "")]).is_err());
    }
}
