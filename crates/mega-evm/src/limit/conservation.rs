//! The transaction-level gas conservation law, as one set of terms.
//!
//! Three places state the same law: the settlement that derives what a transaction destroyed, the
//! re-settlement that follows a rewritten envelope, and the terminal check that the tracker lanes
//! account for the whole receipt. They used to restate it three times, each with its own
//! rearrangement and its own hand-written assertion message. This module holds it once.

use core::fmt;

/// The terms of the transaction-level gas conservation law.
///
/// # The law
///
/// Every unit of EVM gas a transaction burns is one of three things: compute work the trackers
/// enforced, `MegaETH` storage gas, or a budget something threw away without executing anything
/// for it. Two producers sit outside that partition and have to be corrected for — the
/// `CALL_STIPEND` revm mints into a value-transferring call's child frame without debiting the
/// caller, and whatever an inspector wrote into the execution from outside it. What is left is an
/// identity:
///
/// ```text
/// spent = C + S + D − K − I
/// ```
///
/// | term | meaning                                                                      |
/// | ---- | ---------------------------------------------------------------------------- |
/// | `spent` | the envelope the transaction burnt, read where it is final                |
/// | `C`  | [`enforced_compute_gas`](Self::enforced_compute_gas) — the work performed        |
/// | `S`  | [`non_compute_gas`](Self::non_compute_gas) — `MegaETH` storage gas              |
/// | `D`  | the destroyed remainder — budget thrown away without work                       |
/// | `K`  | [`minted_call_stipend`](Self::minted_call_stipend) — gas minted, never debited  |
/// | `I`  | [`inspector_conjured_gas`](Self::inspector_conjured_gas) — gas from outside the EVM |
///
/// `D` is deliberately not a field: it is the one term nothing measures directly, and each caller
/// supplies the reading it holds. [`destroyed_for`](Self::destroyed_for) solves the law for it,
/// [`envelope_for`](Self::envelope_for) solves the law for `spent` given one, and neither is a
/// second law — they are the same identity rearranged.
///
/// # Sign conventions
///
/// `S` and `I` are signed. `S` because the sandbox boundary can return gas to the lane, and `I`
/// because an inspector destroys gas as readily as it conjures it: positive is gas that exists in
/// the execution but that nothing debited from the transaction's envelope, negative is gas the
/// envelope funded that no frame ever received.
///
/// # Before REX7
///
/// Every term but `C` is structurally zero: no lane records non-compute gas, no site mints a
/// stipend into the law, nothing is destroyed, and the outcome's enforced total is its reported
/// total. The law holds there too, but trivially, which is why the assertions that read it are
/// gated to REX7+.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConservationTerms {
    /// `C` — the compute gas the transaction performed, off the lane every compute-gas limit is
    /// evaluated against.
    pub enforced_compute_gas: u64,

    /// `S` — the EVM gas the transaction spent that is neither compute work nor a destroyed
    /// remainder: the `MegaETH` storage-gas surcharges, the `MegaETH` share of intrinsic gas, the
    /// code-deposit charge, and the sandbox boundary's residue.
    pub non_compute_gas: i128,

    /// `K` — the `CALL_STIPEND` total this transaction's value-transferring calls minted into
    /// their child frames.
    ///
    /// revm mints it without debiting the caller, so recorded work exceeds the envelope by one
    /// stipend per such call and the law needs it added back.
    pub minted_call_stipend: u64,

    /// `I` — the net gas an inspector conjured, across every lane the measurement shim books.
    ///
    /// Zero for every transaction that ran without an inspector and for every observation-only
    /// inspector, which is why the law reads the same as it always did on those paths.
    pub inspector_conjured_gas: i128,

    /// What the sites that destroyed a budget booked as they destroyed it — *not* a term of the
    /// law, and never read by [`destroyed_for`](Self::destroyed_for).
    ///
    /// The derivation and this total are two independent measurements of the same quantity. They
    /// agree, and [`unbooked_for`](Self::unbooked_for) is the gap a caller checks or settles;
    /// deriving one from the other would collapse the cross-check into a tautology.
    pub booked_destroyed_compute_gas: u64,
}

impl ConservationTerms {
    /// Solves the law for `D`: `D = spent + K + I − S − C`.
    ///
    /// Signed on purpose. A negative result means the recorded lanes together claim more gas than
    /// the transaction spent, which is a defect to report rather than a value to clamp — clamping
    /// inside the law would hide the half of the mismatch space where the bookings over-count.
    #[inline]
    pub const fn destroyed_for(&self, tx_gas_spent: u64) -> i128 {
        (tx_gas_spent as i128) + (self.minted_call_stipend as i128) + self.inspector_conjured_gas -
            self.non_compute_gas -
            (self.enforced_compute_gas as i128)
    }

    /// Solves the law for `spent`: `spent = C + S + D − K − I`.
    ///
    /// The reading to pass for `D` is the transaction's *reported* destroyed total — what the
    /// receipt's compute total carries — because this direction is what checks that the lanes
    /// account for the envelope that receipt reports.
    #[inline]
    pub const fn envelope_for(&self, destroyed_compute_gas: u64) -> i128 {
        (self.enforced_compute_gas as i128) + self.non_compute_gas + (destroyed_compute_gas as i128) -
            (self.minted_call_stipend as i128) -
            self.inspector_conjured_gas
    }

    /// The gap between what the law derives for `D` and what the per-site bookings hold.
    ///
    /// Zero whenever the two measurements agree. A caller either asserts that (the cross-check at
    /// settlement) or books the difference (the rewritten-envelope re-settlement, where the
    /// receipt's envelope grows past every site that could have booked it).
    #[inline]
    pub const fn unbooked_for(&self, tx_gas_spent: u64) -> i128 {
        self.destroyed_for(tx_gas_spent) - (self.booked_destroyed_compute_gas as i128)
    }
}

impl fmt::Display for ConservationTerms {
    /// The whole term set, in the order the law states it, for assertion messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "enforced compute {}, non-compute {}, minted stipend {}, inspector conjured {}, \
             booked destroyed {}",
            self.enforced_compute_gas,
            self.non_compute_gas,
            self.minted_call_stipend,
            self.inspector_conjured_gas,
            self.booked_destroyed_compute_gas,
        )
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "std"))]
    use alloc as std;

    use super::*;

    fn terms() -> ConservationTerms {
        ConservationTerms {
            enforced_compute_gas: 21_000,
            non_compute_gas: 5_000,
            minted_call_stipend: 2_300,
            inspector_conjured_gas: -400,
            booked_destroyed_compute_gas: 0,
        }
    }

    /// The two directions are one identity, so solving for either term and substituting it back
    /// must return the reading it started from — for any term set, including one whose signed
    /// lanes point in opposite directions.
    #[test]
    fn test_the_two_directions_are_the_same_law() {
        let terms = terms();
        // Above the point where this term set's derivation turns non-negative, so the round-trip
        // stays inside the domain a real transaction produces.
        for spent in [24_100_u64, 100_000, 1_000_000] {
            let destroyed = terms.destroyed_for(spent);
            assert!(destroyed >= 0, "fixture check: {destroyed} must be a real remainder");
            assert_eq!(
                terms.envelope_for(destroyed as u64),
                i128::from(spent),
                "solving for the destroyed remainder and substituting it back must close",
            );
        }
        for destroyed in [0_u64, 1, 5_000] {
            let spent = terms.envelope_for(destroyed);
            assert!(spent >= 0, "fixture check: {spent} must be a real envelope");
            assert_eq!(
                terms.destroyed_for(spent as u64),
                i128::from(destroyed),
                "and so must solving for the envelope and substituting that back",
            );
        }
    }

    /// Each term enters the law with the sign the doc claims, and the two directions carry
    /// opposite signs for the same term.
    #[test]
    fn test_each_term_moves_the_law_in_its_documented_direction() {
        let base = terms();
        let spent = 100_000;
        let destroyed = base.destroyed_for(spent);

        let more_compute =
            ConservationTerms { enforced_compute_gas: base.enforced_compute_gas + 1, ..base };
        assert_eq!(more_compute.destroyed_for(spent), destroyed - 1, "C reduces D");
        assert_eq!(
            more_compute.envelope_for(0),
            base.envelope_for(0) + 1,
            "and raises the envelope",
        );

        let more_storage = ConservationTerms { non_compute_gas: base.non_compute_gas + 1, ..base };
        assert_eq!(more_storage.destroyed_for(spent), destroyed - 1, "S reduces D");

        let more_stipend =
            ConservationTerms { minted_call_stipend: base.minted_call_stipend + 1, ..base };
        assert_eq!(more_stipend.destroyed_for(spent), destroyed + 1, "K raises D");

        let more_conjured =
            ConservationTerms { inspector_conjured_gas: base.inspector_conjured_gas + 1, ..base };
        assert_eq!(more_conjured.destroyed_for(spent), destroyed + 1, "I raises D");
        assert_eq!(
            more_conjured.envelope_for(0),
            base.envelope_for(0) - 1,
            "and lowers the envelope",
        );
    }

    /// The booked total is the cross-check operand, not a term: it moves the gap and nothing else.
    #[test]
    fn test_the_booked_total_is_not_a_term_of_the_law() {
        let base = terms();
        let booked = ConservationTerms { booked_destroyed_compute_gas: 777, ..base };

        assert_eq!(
            booked.destroyed_for(100_000),
            base.destroyed_for(100_000),
            "the derivation must not read the bookings it is checked against",
        );
        assert_eq!(booked.envelope_for(0), base.envelope_for(0));
        assert_eq!(booked.unbooked_for(100_000), base.unbooked_for(100_000) - 777);
    }

    /// An all-zero term set is the pre-REX7 shape: the law degenerates to `spent == 0`.
    #[test]
    fn test_the_default_term_set_is_the_trivial_law() {
        let terms = ConservationTerms::default();
        assert_eq!(terms.destroyed_for(0), 0);
        assert_eq!(terms.envelope_for(0), 0);
        assert_eq!(terms.unbooked_for(0), 0);
    }

    /// The term set is rendered into every assertion message the law raises, and a message that
    /// names no term is a failing invariant with nothing to debug it by. Pins the full text: the
    /// order the law states the terms in, and the signed lanes' signs.
    #[test]
    fn test_display_renders_every_term_in_the_order_the_law_states_them() {
        assert_eq!(
            std::format!("{}", terms()),
            "enforced compute 21000, non-compute 5000, minted stipend 2300, \
             inspector conjured -400, booked destroyed 0",
        );
    }
}
