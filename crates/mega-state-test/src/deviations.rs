//! The places Satin differs from Ethereum on purpose, and the fixtures each one fails.
//!
//! Equivalence mode runs a fixture on Satin's machinery priced as the fixture's fork prices it,
//! so a failure there is either a bug or a rule `MegaETH` keeps that no configuration can turn
//! off. Every rule of the second kind is registered here, once, with the reason Satin keeps it and
//! the failures it explains; a failure no entry explains is unattributed, and one unattributed
//! failure fails the gate.
//!
//! An entry explains a failure by where the fixture lives and how it failed, not by the test's
//! name alone, and it pins how many failures it explains on the pinned fixture releases: a new
//! failure of the same shape moves the count and fails the gate as surely as an unattributed one,
//! and so does a fixed one.

use serde::Serialize;

use crate::{runner::FailureKind, Fork};

/// A place Satin differs from Ethereum on purpose.
#[derive(Debug, Serialize)]
pub struct Deviation {
    /// A short, stable name.
    pub id: &'static str,
    /// The `MegaETH` rule the fixtures meet.
    pub rule: &'static str,
    /// Why Satin keeps the rule, and why the neutral configuration cannot express Ethereum's.
    pub reason: &'static str,
    /// Path fragments: a failure in a fixture file whose path contains one of them may be this
    /// deviation's.
    pub paths: &'static [&'static str],
    /// The ways a fixture of those paths fails on this rule.
    pub kinds: &'static [FailureKind],
    /// The forks whose fixtures meet the rule.
    pub forks: &'static [Fork],
    /// How many failures the entry explains on the pinned fixture releases, per fork.
    pub pinned: &'static [(Fork, usize)],
}

impl Deviation {
    /// Whether a failure of `kind`, in the fixture at `path`, of `fork`, is this deviation's.
    pub fn explains(&self, fork: Fork, path: &str, kind: FailureKind) -> bool {
        let path = path.replace('\\', "/");
        self.forks.contains(&fork) &&
            self.kinds.contains(&kind) &&
            self.paths.iter().any(|fragment| path.contains(fragment))
    }

    /// How many failures the entry explains on `fork`'s pinned release.
    pub fn pinned(&self, fork: Fork) -> usize {
        self.pinned.iter().find(|(f, _)| *f == fork).map_or(0, |(_, count)| *count)
    }
}

/// Every registered deviation.
pub const DEVIATIONS: &[Deviation] = &[AMSTERDAM_OPCODES_ON_OSAKA, SELFDESTRUCT_BURNS_ON_OSAKA];

/// Satin's instruction table carries four Amsterdam opcodes on its Osaka base.
pub const AMSTERDAM_OPCODES_ON_OSAKA: Deviation = Deviation {
    id: "amsterdam-opcodes-on-osaka",
    rule: "Satin runs DUPN, SWAPN and EXCHANGE (EIP-8024) and SLOTNUM (EIP-7843), which its Osaka \
           base leaves undefined.",
    reason: "Satin takes the Amsterdam schedule and the opcodes with it. The instruction table is \
             Satin's machinery, which equivalence mode keeps, so an Osaka fixture that executes \
             one of the four bytes, expecting the undefined-opcode halt Osaka gives it, runs the \
             opcode instead. With the four entries taken back out of the table, every Osaka \
             fixture passes.",
    paths: &["frontier/opcodes/test_all_opcodes.json", "stBadOpcode/undefinedOpcodeFirstByte.json"],
    kinds: &[FailureKind::StateRootMismatch],
    forks: &[Fork::Osaka],
    pinned: &[(Fork::Osaka, 2)],
};

/// A contract self-destructed in its creating transaction is removed as on Satin's Osaka base,
/// where Amsterdam keeps a balance it sent to itself (EIP-8246).
pub const SELFDESTRUCT_BURNS_ON_OSAKA: Deviation = Deviation {
    id: "selfdestruct-burns-on-osaka",
    rule: "A contract self-destructed in the transaction that created it is removed as on Satin's \
           Osaka base, a balance it sent to itself burned: EIP-8246, which keeps that balance and \
           clears the account only at the end of the transaction, is not active.",
    reason: "revm gates EIP-8246 on the Amsterdam spec id, in the journal, and Satin's spec runs \
             on Karst, an Osaka base: Satin takes Amsterdam's rules one switch at a time — EIP-8037, \
             EIP-2780, the opcodes — and EIP-8246 has no switch, so no configuration can turn it \
             on. An Amsterdam fixture in which a contract destructs to itself expects the balance \
             kept, and the EIP-7708 transfer logs a kept balance produces when it is sent on. revm \
             itself, on an Osaka base with Amsterdam's configuration, produces exactly the \
             post-state and logs Satin does for every one of these fixtures, and the fixtures' own \
             post-state and logs on the Amsterdam spec. Whether Satin takes EIP-8246 is a decision for its \
             specification, not for this gate.",
    paths: &[
        "amsterdam/eip2780_reduce_intrinsic_tx_gas/top_frame_charges/initcode_selfdestruct_keeps_top_frame_state_charge.json",
        "amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_call/call_value_to_self_destructed_same_tx_account.json",
        "amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_selfdestruct/selfdestruct_to_self_in_create_tx.json",
        "amsterdam/eip8038_state_access_gas_cost_increase/selfdestruct_gas/same_tx_created_selfdestruct_self_burn.json",
        "amsterdam/eip8246_selfdestruct_no_burn/selfdestruct_no_burn/create_transaction_initcode_selfdestruct.json",
        "cancun/eip6780_selfdestruct/selfdestruct_revert/selfdestruct_created_in_same_tx_with_revert.json",
        "cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
        "cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json",
        "frontier/create/create_suicide_during_init/create_suicide_during_transaction_create.json",
        "ported_static/stCreate2/create2_suicide/create2_suicide.json",
        "ported_static/stInitCodeTest/transaction_create_suicide_in_initcode/transaction_create_suicide_in_initcode.json",
    ],
    kinds: &[FailureKind::StateRootMismatch, FailureKind::LogsMismatch],
    forks: &[Fork::Amsterdam],
    pinned: &[(Fork::Amsterdam, 37)],
};

/// The deviation that explains a failure of `kind` in the fixture at `path`, if one does.
///
/// Entries are disjoint: no failure is explained by two of them (a test checks it on the
/// registry's own paths), so the first that matches is the one.
pub fn attribute(fork: Fork, path: &str, kind: FailureKind) -> Option<&'static Deviation> {
    DEVIATIONS.iter().find(|deviation| deviation.explains(fork, path, kind))
}

/// The deviation named `id`.
pub fn by_id(id: &str) -> Option<&'static Deviation> {
    DEVIATIONS.iter().find(|deviation| deviation.id == id)
}

/// The registry as the crate's `DEVIATIONS.md` has it, one sentence to a line.
pub fn render_markdown() -> String {
    let sentences = |text: &str| text.replace(". ", ".\n");
    let mut out = String::from(
        "# Deviations\n\n\
         <!-- Rendered from `src/deviations.rs` by \
         `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`; do not edit. -->\n\n\
         The places Satin differs from Ethereum on purpose that the execution-spec gate meets.\n\
         In equivalence mode a failure one of them explains is counted against it, and the count \
         is pinned; a failure none of them explains fails the gate.\n",
    );
    for deviation in DEVIATIONS {
        out.push_str(&format!("\n## `{}`\n\n", deviation.id));
        for (fork, count) in deviation.pinned {
            out.push_str(&format!("{count} failed tests on the pinned {fork} fixtures.\n\n"));
        }
        out.push_str(&format!("**Rule.**\n{}\n\n", sentences(deviation.rule)));
        out.push_str(&format!("**Reason.**\n{}\n\n", sentences(deviation.reason)));
        let kinds: Vec<_> =
            deviation.kinds.iter().map(|kind| format!("`{}`", kind.name())).collect();
        out.push_str(&format!("**Failures.** {}, in:\n\n", kinds.join(", ")));
        for path in deviation.paths {
            out.push_str(&format!("- `{path}`\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn test_ids_are_unique_and_every_entry_is_complete() {
        let ids: BTreeSet<_> = DEVIATIONS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), DEVIATIONS.len(), "duplicate deviation id");
        for deviation in DEVIATIONS {
            assert!(!deviation.rule.is_empty() && !deviation.reason.is_empty(), "{}", deviation.id);
            assert!(!deviation.paths.is_empty() && !deviation.kinds.is_empty(), "{}", deviation.id);
            assert!(!deviation.forks.is_empty(), "{}", deviation.id);
            for (fork, count) in deviation.pinned {
                assert!(
                    deviation.forks.contains(fork),
                    "{}: pins a fork it does not cover",
                    deviation.id
                );
                assert!(*count > 0, "{}: a zero pin is a stale entry", deviation.id);
            }
            for fork in deviation.forks {
                assert!(
                    deviation.pinned(*fork) > 0,
                    "{}: covers {fork} without a pin",
                    deviation.id
                );
            }
            assert_eq!(by_id(deviation.id).map(|d| d.id), Some(deviation.id));
        }
    }

    /// No two entries explain the same failure: every entry's own paths, on each fork and kind it
    /// covers, are attributed to it and to nothing before it.
    #[test]
    fn test_entries_are_disjoint() {
        for deviation in DEVIATIONS {
            for &fork in deviation.forks {
                for &kind in deviation.kinds {
                    for path in deviation.paths {
                        let path = format!("fixtures/state_tests/{path}/x.json");
                        assert_eq!(
                            attribute(fork, &path, kind).map(|d| d.id),
                            Some(deviation.id),
                            "{path} ({fork}, {kind:?})"
                        );
                        let others: Vec<_> = DEVIATIONS
                            .iter()
                            .filter(|other| {
                                other.id != deviation.id && other.explains(fork, &path, kind)
                            })
                            .map(|other| other.id)
                            .collect();
                        assert!(others.is_empty(), "{path}: also explained by {others:?}");
                    }
                }
            }
        }
    }

    /// `DEVIATIONS.md` is the rendered registry. `UPDATE_DEVIATIONS=1` writes it instead.
    #[test]
    fn test_the_rendered_registry_is_up_to_date() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("DEVIATIONS.md");
        let rendered = render_markdown();
        if std::env::var_os("UPDATE_DEVIATIONS").is_some() {
            std::fs::write(&path, &rendered).expect("the file is writable");
            return;
        }
        let checked_in = std::fs::read_to_string(&path).expect("DEVIATIONS.md is checked in");
        assert_eq!(
            checked_in, rendered,
            "DEVIATIONS.md is out of date; regenerate it with \
             UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations"
        );
    }
}
