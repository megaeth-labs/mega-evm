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
//! landing between two calls, a load-balanced endpoint serving divergent views,
//! or a served block header that does not hash to the hash it is served under),
//! never a definitive answer about the target. Both drivers therefore report all
//! of them as infrastructure failures.

use alloy_primitives::B256;
use alloy_rpc_types_eth::Header;
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
    /// A served block header does not hash to the hash it was served under.
    UnauthenticHeader {
        /// Height the served header claims.
        number: u64,
        /// Hash the endpoint reported for the header.
        served: B256,
        /// Hash the served consensus fields actually produce.
        computed: B256,
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
            // The hash a header is served under is a claim about the header, not
            // a property of it. Recomputing the hash is the only way to tell the
            // two apart, so the message reports both values.
            Self::UnauthenticHeader { number, served, computed } => write!(
                f,
                "the header served for block {number} hashes to {computed}, but the endpoint \
                 reported it as {served}: the served block header does not authenticate (an \
                 inconsistent backend, or a tampered capture); the block environment it \
                 describes is unverified"
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

/// Authenticate a served block header against the hash it was served under.
///
/// Every other judgment in this module compares two answers the endpoint gave.
/// This one compares an answer against itself: a block header is a consensus
/// object whose hash is a function of its own fields, so the `hash` an endpoint
/// reports beside them can be checked without asking anything further. Nothing
/// else in the replay does check it — the inclusion, linkage and membership
/// guards all consume that reported hash — so an endpoint (or a tampered offline
/// capture) can rewrite `timestamp`, `baseFeePerGas`, `gasLimit` or `parentHash`
/// while keeping the original `hash`, pass every guard, and have the target
/// replayed under a block environment it never ran in.
///
/// The hash is recomputed from the served consensus fields rather than read back
/// from the response. `Header::hash()` / `Block::hash()` return the `hash` field
/// an RPC deserialization filled in — the very value being authenticated — the
/// same trap the transaction-level authentication meets with the envelope's
/// cached hash.
pub(super) fn authenticate_block_header(header: &Header) -> Result<(), Incoherence> {
    let computed = header.inner.hash_slow();
    if computed != header.hash {
        return Err(Incoherence::UnauthenticHeader {
            number: header.inner.number,
            served: header.hash,
            computed,
        });
    }
    Ok(())
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

    /// A served header whose reported hash is the one its own fields produce.
    fn sealed_header() -> Header {
        let inner = alloy_consensus::Header {
            number: 22_945_844,
            timestamp: 1_764_000_000,
            gas_limit: 10_000_000_000,
            parent_hash: HASH_A,
            base_fee_per_gas: Some(1_000_000),
            ..Default::default()
        };
        // `Header::new` seals the consensus header, so the reported hash is the
        // hash of these fields — an authentic answer, as a real endpoint serves.
        Header::new(inner)
    }

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
                Incoherence::UnauthenticHeader {
                    number: 22_945_844,
                    served: HASH_A,
                    computed: HASH_B,
                },
                format!(
                    "the header served for block 22945844 hashes to {b}, but the endpoint \
                     reported it as {a}: the served block header does not authenticate (an \
                     inconsistent backend, or a tampered capture); the block environment it \
                     describes is unverified"
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

    /// A header served under the hash of its own fields authenticates.
    #[test]
    fn test_authenticate_block_header_accepts_a_sealed_header() {
        assert_eq!(authenticate_block_header(&sealed_header()), Ok(()));
    }

    /// Rewriting any execution-relevant header field while keeping the reported
    /// hash is rejected, and the verdict carries both hashes.
    ///
    /// The fields are exactly the ones a replay reads out of the header, which
    /// is what makes a forged header worth catching: each of them silently moves
    /// the world the target is executed in.
    #[test]
    fn test_authenticate_block_header_rejects_every_tampered_field() {
        /// One named rewrite of a served header's consensus fields.
        type Tampering = (&'static str, fn(&mut alloy_consensus::Header));

        let tamperings: [Tampering; 6] = [
            ("timestamp", |h| h.timestamp += 1),
            ("gas_limit", |h| h.gas_limit += 1),
            ("parent_hash", |h| h.parent_hash = HASH_B),
            ("base_fee_per_gas", |h| h.base_fee_per_gas = Some(2_000_000)),
            ("number", |h| h.number += 1),
            ("beneficiary", |h| h.beneficiary = alloy_primitives::Address::repeat_byte(0xcc)),
        ];
        for (field, tamper) in tamperings {
            let mut header = sealed_header();
            let served = header.hash;
            tamper(&mut header.inner);

            let Err(verdict) = authenticate_block_header(&header) else {
                panic!("a rewritten {field} must not authenticate");
            };
            let Incoherence::UnauthenticHeader { number, served: reported, computed } = verdict
            else {
                panic!("tampering with {field} must be reported as an unauthentic header");
            };
            assert_eq!(reported, served, "the verdict must carry the hash the endpoint reported");
            assert_eq!(
                number, header.inner.number,
                "the verdict must name the height the served header claims"
            );
            assert_ne!(computed, served, "a tampered {field} must change the recomputed hash");
            assert_eq!(
                computed,
                header.inner.hash_slow(),
                "the verdict must carry the hash the served fields produce"
            );
        }
    }

    /// The recomputation reads the served fields, not the reported hash.
    ///
    /// An RPC deserialization fills the `hash` field from the response, and the
    /// accessors return it verbatim; a check written against those would accept
    /// any header. Rewriting only `hash` must therefore be rejected too.
    #[test]
    fn test_authenticate_block_header_rejects_a_rewritten_hash() {
        let mut header = sealed_header();
        let computed = header.hash;
        header.hash = HASH_B;

        let verdict = authenticate_block_header(&header)
            .expect_err("a header served under another hash must not authenticate");

        assert_eq!(
            verdict,
            Incoherence::UnauthenticHeader {
                number: header.inner.number,
                served: HASH_B,
                computed
            }
        );
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
