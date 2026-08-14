//! Target-coherence judgments shared by the single-transaction and batch replay
//! drivers.
//!
//! Both drivers ask the same endpoint the same questions about a replay target —
//! where is it included, does the block it names have a parent, does that parent
//! link to it, does the block body actually list it — and both must reject the
//! same self-contradictory answers with the same words. The judgments live here
//! so there is one place where the classification and its wording are decided.
//!
//! What is shared is only the verdict: a pure classification of metadata, or a
//! typed [`Incoherence`] carrying its message. Control flow stays in each driver.
//! The single-transaction path fails the whole run on the first verdict; the
//! batch path turns a verdict into one target's entry and keeps going, and how it
//! orders and tallies those entries is its own business.
//!
//! Every [`Incoherence`] describes the *endpoint* contradicting itself (a reorg
//! landing between two calls, or a load-balanced endpoint serving divergent
//! views), never a definitive answer about the target. Both drivers therefore
//! report all of them as infrastructure failures.

use alloy_primitives::B256;
use core::fmt;

/// Where the endpoint placed a target, as read from its `(block_number,
/// block_hash)` pair.
///
/// Produced by [`classify_placement`], which rejects the two contradictory
/// shapes, so only these two remain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetPlacement {
    /// Mined into `number`, anchored by the inclusion hash the lookup reported.
    Mined {
        /// Height the endpoint resolved the target into.
        number: u64,
        /// Hash of the block the endpoint claims includes the target.
        inclusion_hash: B256,
    },
    /// Not mined yet: neither a block number nor an inclusion hash.
    Pending,
}

/// Why a target was expected in a block body, which is what the membership
/// failure message says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MembershipClaim {
    /// The target's own lookup named this block as its inclusion.
    ResolvedInclusion,
    /// The target was queued against this block without an inclusion claim of
    /// its own (a whole-block run takes its targets from the body itself).
    QueuedAgainstBlock,
}

/// An endpoint answer about a replay target that contradicts another answer the
/// same endpoint gave.
///
/// Carries the values that name the contradiction, and renders the message both
/// drivers report. Constructed only by the `require_*` / `classify_*` judgments
/// below, so the wording of a class of failure has exactly one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Incoherence {
    /// Mined into a block, but with no inclusion hash to anchor the replay to.
    UnanchoredView {
        /// Height the lookup reported without an accompanying hash.
        number: u64,
    },
    /// An inclusion hash paired with a null block number: the hash proves
    /// inclusion while the null number denies it.
    ContradictoryMetadata {
        /// Inclusion hash the lookup reported for an allegedly unmined target.
        inclusion_hash: B256,
    },
    /// The target was resolved into the genesis block, which has no parent to
    /// fork the pre-state from.
    GenesisPlacement,
    /// The block fetched as the parent is not the parent of the block being
    /// replayed.
    ParentLinkage {
        /// Hash of the block served at the parent height.
        parent_hash: B256,
        /// `parent_hash` of the block being replayed.
        expected_parent: B256,
    },
    /// The block fetched by number is not the block the target resolved into.
    InclusionMismatch {
        /// Height both views describe.
        number: u64,
        /// Hash of the block the endpoint served at that height.
        fetched: B256,
        /// Hash the target's own lookup reported as its inclusion.
        reported: B256,
    },
    /// The block body does not list a target the endpoint placed in it.
    AbsentFromBody {
        /// Height of the block that was expected to list the target.
        number: u64,
        /// Hash of the block whose body was inspected.
        block_hash: B256,
        /// The target that is missing from the body.
        tx_hash: B256,
        /// Why the target was expected in that body.
        claim: MembershipClaim,
    },
}

impl fmt::Display for Incoherence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // A mined transaction without an inclusion hash is an unanchored
            // view: the block number alone cannot prove which block body the
            // target belongs to, so there is nothing to anchor the replay to.
            Self::UnanchoredView { number } => write!(
                f,
                "endpoint reported a mined transaction in block {number} without an \
                 inclusion hash: unanchored view"
            ),
            // A hash proves inclusion; a null number denies it. That pair is
            // self-contradictory metadata, not a pending transaction.
            Self::ContradictoryMetadata { inclusion_hash } => write!(
                f,
                "endpoint reported inclusion hash {inclusion_hash} without a block \
                 number: contradictory metadata"
            ),
            Self::GenesisPlacement => write!(
                f,
                "endpoint resolved the target into block 0, which has no parent block \
                 to fork from: contradictory endpoint data"
            ),
            Self::ParentLinkage { parent_hash, expected_parent } => write!(
                f,
                "parent block hash {parent_hash} != block parent_hash {expected_parent}: the \
                 parent block describes a different chain than the block being replayed (reorg \
                 in progress, or a load-balanced endpoint serving divergent views); retry once \
                 the chain settles"
            ),
            Self::InclusionMismatch { number, fetched, reported } => write!(
                f,
                "block {number} has hash {fetched}, but the target transaction was \
                 resolved as included in {reported}: the endpoint served divergent views of \
                 this block (reorg in progress, or a load-balanced endpoint); retry once the \
                 chain settles"
            ),
            Self::AbsentFromBody { number, block_hash, tx_hash, claim } => {
                let expectation = match claim {
                    MembershipClaim::ResolvedInclusion => {
                        "which the endpoint resolved as included in it"
                    }
                    MembershipClaim::QueuedAgainstBlock => "which was queued against it",
                };
                write!(
                    f,
                    "block {number} ({block_hash}) does not list target transaction {tx_hash}, \
                     {expectation}: the endpoint served divergent views of this block (reorg in \
                     progress, or a load-balanced endpoint); retry once the chain settles"
                )
            }
        }
    }
}

/// Classify a target from the `(block_number, block_hash)` pair its lookup
/// reported.
///
/// Every shape the endpoint can return is answered here, from the metadata
/// alone: no block has to be fetched first, so no fetch failure can mask the
/// verdict, and a contradictory row cannot fall through into the pending arm.
pub(super) fn classify_placement(
    block_number: Option<u64>,
    block_hash: Option<B256>,
) -> Result<TargetPlacement, Incoherence> {
    match (block_number, block_hash) {
        (Some(number), Some(inclusion_hash)) => {
            Ok(TargetPlacement::Mined { number, inclusion_hash })
        }
        (Some(number), None) => Err(Incoherence::UnanchoredView { number }),
        (None, Some(inclusion_hash)) => Err(Incoherence::ContradictoryMetadata { inclusion_hash }),
        (None, None) => Ok(TargetPlacement::Pending),
    }
}

/// Require that the block a target was resolved into has a parent to fork from.
///
/// An endpoint resolving a target into block 0 contradicts itself: genesis has
/// no parent, so there is no pre-state to replay against. Rejecting here also
/// keeps the caller's `number - 1` state base from underflowing.
pub(super) const fn require_forkable_block(number: u64) -> Result<(), Incoherence> {
    if number == 0 {
        return Err(Incoherence::GenesisPlacement);
    }
    Ok(())
}

/// Require that the block fetched at the parent height is the parent of the
/// block being replayed.
///
/// The two blocks are fetched by number in separate calls, so across a reorg or
/// a load-balanced endpoint `eth_getBlockByNumber(N-1)` can return a block that
/// is not the parent of block `N`. Forking from that state would silently
/// execute against the wrong pre-state, and the divergence would surface later
/// as a receipt mismatch rather than as the infrastructure failure it is.
pub(super) fn require_parent_linkage(
    parent_hash: B256,
    expected_parent: B256,
) -> Result<(), Incoherence> {
    if parent_hash != expected_parent {
        return Err(Incoherence::ParentLinkage { parent_hash, expected_parent });
    }
    Ok(())
}

/// Require that the block fetched by number is the block the target resolved
/// into.
///
/// The linkage check only proves the fetched blocks belong to one chain, not
/// that the target belongs to them. The lookup that resolved the target reported
/// its inclusion in a separate call, so a reorg or a load-balanced endpoint can
/// answer the numbered fetch from a replacement block the target is not part of;
/// replaying that block would execute the target against a body it never ran in.
pub(super) fn require_inclusion_anchor(
    number: u64,
    fetched: B256,
    reported: B256,
) -> Result<(), Incoherence> {
    if fetched != reported {
        return Err(Incoherence::InclusionMismatch { number, fetched, reported });
    }
    Ok(())
}

/// Require that the block body lists a target the endpoint placed in this block.
///
/// A body that does not list the target contradicts the placement it was queued
/// under. On the single-transaction path the body position also defines the set
/// of preceding transactions, so an unlisted target would silently make every
/// transaction of the block count as preceding.
pub(super) const fn require_body_membership(
    number: u64,
    block_hash: B256,
    tx_hash: B256,
    listed: bool,
    claim: MembershipClaim,
) -> Result<(), Incoherence> {
    if !listed {
        return Err(Incoherence::AbsentFromBody { number, block_hash, tx_hash, claim });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct, recognizable hashes for the message assertions.
    const HASH_A: B256 = B256::repeat_byte(0xaa);
    const HASH_B: B256 = B256::repeat_byte(0xbb);

    /// The four `(block_number, block_hash)` shapes, at the boundary values that
    /// have historically been read wrong: block 0 (falsy height), a null hash on
    /// a mined row, and a hash on a null-number row.
    #[test]
    fn test_classify_placement_covers_every_metadata_shape() {
        for (number, hash, expected) in [
            (
                Some(7_u64),
                Some(HASH_A),
                Ok(TargetPlacement::Mined { number: 7, inclusion_hash: HASH_A }),
            ),
            // Block 0 is a placement, not a metadata contradiction: the
            // forkability judgment rejects it separately.
            (
                Some(0),
                Some(HASH_A),
                Ok(TargetPlacement::Mined { number: 0, inclusion_hash: HASH_A }),
            ),
            (
                Some(u64::MAX),
                Some(HASH_A),
                Ok(TargetPlacement::Mined { number: u64::MAX, inclusion_hash: HASH_A }),
            ),
            (Some(7), None, Err(Incoherence::UnanchoredView { number: 7 })),
            (Some(0), None, Err(Incoherence::UnanchoredView { number: 0 })),
            (
                None,
                Some(HASH_A),
                Err(Incoherence::ContradictoryMetadata { inclusion_hash: HASH_A }),
            ),
            (None, None, Ok(TargetPlacement::Pending)),
        ] {
            assert_eq!(
                classify_placement(number, hash),
                expected,
                "unexpected classification for ({number:?}, {hash:?})"
            );
        }
    }

    /// Message wording is a user-facing contract of both drivers, so it is
    /// pinned literally rather than rebuilt from the same format string.
    #[test]
    fn test_incoherence_messages_are_pinned() {
        let a = HASH_A.to_string();
        let b = HASH_B.to_string();
        for (incoherence, expected) in [
            (
                Incoherence::UnanchoredView { number: 22_945_844 },
                "endpoint reported a mined transaction in block 22945844 without an inclusion \
                 hash: unanchored view"
                    .to_string(),
            ),
            (
                Incoherence::ContradictoryMetadata { inclusion_hash: HASH_A },
                format!(
                    "endpoint reported inclusion hash {a} without a block number: \
                         contradictory metadata"
                ),
            ),
            (
                Incoherence::GenesisPlacement,
                "endpoint resolved the target into block 0, which has no parent block to fork \
                 from: contradictory endpoint data"
                    .to_string(),
            ),
            (
                Incoherence::ParentLinkage { parent_hash: HASH_A, expected_parent: HASH_B },
                format!(
                    "parent block hash {a} != block parent_hash {b}: the parent block describes \
                     a different chain than the block being replayed (reorg in progress, or a \
                     load-balanced endpoint serving divergent views); retry once the chain settles"
                ),
            ),
            (
                Incoherence::InclusionMismatch { number: 12, fetched: HASH_A, reported: HASH_B },
                format!(
                    "block 12 has hash {a}, but the target transaction was resolved as included \
                     in {b}: the endpoint served divergent views of this block (reorg in \
                     progress, or a load-balanced endpoint); retry once the chain settles"
                ),
            ),
            (
                Incoherence::AbsentFromBody {
                    number: 12,
                    block_hash: HASH_A,
                    tx_hash: HASH_B,
                    claim: MembershipClaim::ResolvedInclusion,
                },
                format!(
                    "block 12 ({a}) does not list target transaction {b}, which the endpoint \
                     resolved as included in it: the endpoint served divergent views of this \
                     block (reorg in progress, or a load-balanced endpoint); retry once the \
                     chain settles"
                ),
            ),
            (
                Incoherence::AbsentFromBody {
                    number: 12,
                    block_hash: HASH_A,
                    tx_hash: HASH_B,
                    claim: MembershipClaim::QueuedAgainstBlock,
                },
                format!(
                    "block 12 ({a}) does not list target transaction {b}, which was queued \
                     against it: the endpoint served divergent views of this block (reorg in \
                     progress, or a load-balanced endpoint); retry once the chain settles"
                ),
            ),
        ] {
            assert_eq!(incoherence.to_string(), expected, "unexpected message for {incoherence:?}");
        }
    }

    /// Genesis is the only height without a parent to fork from.
    #[test]
    fn test_require_forkable_block_rejects_only_genesis() {
        assert_eq!(require_forkable_block(0), Err(Incoherence::GenesisPlacement));
        assert_eq!(require_forkable_block(1), Ok(()));
        assert_eq!(require_forkable_block(u64::MAX), Ok(()));
    }

    /// Linkage, anchoring, and membership accept agreement and reject anything
    /// else, carrying both views into the message.
    #[test]
    fn test_pairwise_judgments_accept_only_agreement() {
        assert_eq!(require_parent_linkage(HASH_A, HASH_A), Ok(()));
        assert_eq!(
            require_parent_linkage(HASH_A, HASH_B),
            Err(Incoherence::ParentLinkage { parent_hash: HASH_A, expected_parent: HASH_B }),
        );

        assert_eq!(require_inclusion_anchor(1, HASH_A, HASH_A), Ok(()));
        assert_eq!(
            require_inclusion_anchor(1, HASH_A, HASH_B),
            Err(Incoherence::InclusionMismatch { number: 1, fetched: HASH_A, reported: HASH_B }),
        );

        for claim in [MembershipClaim::ResolvedInclusion, MembershipClaim::QueuedAgainstBlock] {
            assert_eq!(require_body_membership(1, HASH_A, HASH_B, true, claim), Ok(()));
            assert_eq!(
                require_body_membership(1, HASH_A, HASH_B, false, claim),
                Err(Incoherence::AbsentFromBody {
                    number: 1,
                    block_hash: HASH_A,
                    tx_hash: HASH_B,
                    claim,
                }),
            );
        }
    }
}
