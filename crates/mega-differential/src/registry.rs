//! The registry of accepted differences.
//!
//! Every difference between the arms must be explained by an active entry, and every active
//! entry must explain at least one difference; either failure fails the run. A mechanism that
//! stops producing its difference therefore shows up as a stale entry, to be retired, instead of
//! lingering as a blanket excuse.

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

/// An accepted difference between `MegaEvm` and the oracle.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deviation {
    /// Stable identifier, used in reports.
    pub id: String,
    /// Whether the entry is in force.
    pub status: Status,
    /// Scenario names the entry covers (`*` matches any run of characters).
    pub scenario: String,
    /// Field path the entry covers (`*` matches any run of characters).
    pub field: String,
    /// The value `MegaEvm` reports (`*` matches any run of characters).
    pub left: String,
    /// The value the oracle reports (`*` matches any run of characters).
    pub right: String,
    /// The mechanism that makes the arms differ.
    pub mechanism: String,
    /// Why the difference is correct.
    pub reason: String,
    /// Why the entry was retired; required for a retired entry.
    #[serde(default)]
    pub retired_reason: Option<String>,
}

/// Status of a registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// In force: it explains differences, and it must explain at least one.
    Active,
    /// Kept for the record; it explains nothing.
    Retired,
}

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
        let mut ids = std::collections::BTreeSet::new();
        for dev in &registry.deviations {
            if !ids.insert(dev.id.as_str()) {
                return Err(format!("duplicate registry id {}", dev.id));
            }
            if dev.mechanism.trim().is_empty() || dev.reason.trim().is_empty() {
                return Err(format!("{}: an entry names its mechanism and its reason", dev.id));
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

    /// The active entry that explains `diff`, if any.
    pub fn explain(&self, diff: &Difference) -> Option<&Deviation> {
        self.deviations.iter().find(|dev| dev.status == Status::Active && dev.matches(diff))
    }

    /// A retired entry that would have explained `diff`, to point at in a failure report.
    pub fn retired_match(&self, diff: &Difference) -> Option<&Deviation> {
        self.deviations.iter().find(|dev| dev.status == Status::Retired && dev.matches(diff))
    }
}

impl Deviation {
    /// Whether this entry's patterns cover `diff`, whatever its status.
    pub fn matches(&self, diff: &Difference) -> bool {
        glob(&self.scenario, &diff.scenario) &&
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

    fn entry(status: &str, extra: &str) -> String {
        format!(
            r#"{{"id": "d", "status": "{status}", "scenario": "call_*", "field": "tx[*].gas.gas_spent",
                "left": "*", "right": "21000", "mechanism": "m", "reason": "r"{extra}}}"#
        )
    }

    fn registry(entries: &[String]) -> Result<Registry, String> {
        Registry::from_json(&format!(r#"{{"version": 1, "deviations": [{}]}}"#, entries.join(",")))
    }

    fn diff(scenario: &str, right: &str) -> Difference {
        Difference {
            scenario: scenario.into(),
            field: "tx[0].gas.gas_spent".into(),
            left: "21100".into(),
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
        let registry = registry(&[entry("active", "")]).unwrap();
        assert_eq!(registry.explain(&diff("call_1", "21000")).map(|d| d.id.as_str()), Some("d"));
        assert!(registry.explain(&diff("create_1", "21000")).is_none(), "scenario pattern");
        assert!(registry.explain(&diff("call_1", "21001")).is_none(), "right value");
    }

    #[test]
    fn test_retired_entry_explains_nothing() {
        let registry = registry(&[entry("retired", r#", "retired_reason": "gone""#)]).unwrap();
        assert!(registry.explain(&diff("call_1", "21000")).is_none());
        assert_eq!(
            registry.retired_match(&diff("call_1", "21000")).map(|d| d.id.as_str()),
            Some("d")
        );
    }

    #[test]
    fn test_registry_rejects_malformed_entries() {
        assert!(registry(&[entry("retired", "")]).unwrap_err().contains("retired_reason"));
        assert!(registry(&[entry("active", r#", "retired_reason": "x""#)]).is_err());
        assert!(registry(&[entry("active", ""), entry("active", "")])
            .unwrap_err()
            .contains("duplicate"));
        assert!(Registry::from_json(r#"{"version": 2, "deviations": []}"#).is_err());
        assert!(registry(&[entry("paused", "")]).is_err());
    }
}
