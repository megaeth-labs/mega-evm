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
//! a served block header that does not hash to the hash it is served under, or
//! that answers a numbered fetch from another height, or a served block body or
//! set of receipts the header does not commit to), never a definitive answer
//! about the target. Both drivers therefore report all of them as infrastructure
//! failures.

use alloy_primitives::{logs_bloom, Bloom, Bytes, Log, B256};
use alloy_rpc_types_eth::Header;
use core::fmt;
use op_alloy_consensus::OpReceiptEnvelope;

use super::header::{receipts_root, transactions_root};

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
    /// A block fetched by number is a block at another height.
    HeightMismatch {
        /// Height the fetch asked for.
        requested: u64,
        /// Height the served header claims.
        served: u64,
    },
    /// A served receipt's logs bloom is not the bloom of the logs it carries.
    UnauthenticReceiptBloom {
        /// Transaction the receipt was served for.
        tx_hash: B256,
        /// How many logs the receipt carries.
        logs: usize,
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
    /// The transactions served for a block body do not rebuild the transactions
    /// root its header commits to.
    UncommittedBody {
        /// Height of the block whose body was served.
        number: u64,
        /// Hash of that block.
        block_hash: B256,
        /// Transactions root the served body rebuilds to.
        served: B256,
        /// Transactions root the block's header commits to.
        committed: B256,
    },
    /// The receipts served for every transaction of a block do not rebuild the
    /// receipts root its header commits to.
    UncommittedReceipts {
        /// Height of the block whose receipts were fetched.
        number: u64,
        /// Hash of that block.
        block_hash: B256,
        /// Receipts root the served receipts rebuild to.
        served: B256,
        /// Receipts root the block's header commits to.
        committed: B256,
    },
    /// A served receipt's `gasUsed` is not its share of the block's cumulative
    /// gas: the rise in cumulative gas from the receipt before it (from zero for
    /// the first one).
    InconsistentReceiptGas {
        /// Height of the block whose receipts were fetched.
        number: u64,
        /// Hash of that block.
        block_hash: B256,
        /// Transaction the receipt was served for.
        tx_hash: B256,
        /// The `gasUsed` the receipt reports.
        served: u64,
        /// Cumulative gas of the block before this transaction.
        cumulative_before: u64,
        /// Cumulative gas of the block up to and including this transaction.
        cumulative_after: u64,
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
            // The height a header claims is a consensus field, so an
            // authenticated header cannot be at the height it was fetched under
            // by accident: either the endpoint answered a different question, or
            // the answer was moved onto another request.
            Self::HeightMismatch { requested, served } => write!(
                f,
                "the endpoint answered the fetch of block {requested} with block {served}: a \
                 numbered fetch was served a header from another height (an inconsistent backend, \
                 or a tampered capture); the block environment it describes is not the one the run \
                 asked for"
            ),
            // The bloom is a function of the logs beside it, so a receipt can be
            // checked against itself without asking anything further.
            Self::UnauthenticReceiptBloom { tx_hash, logs } => write!(
                f,
                "the on-chain receipt served for transaction {tx_hash} carries a logs bloom that \
                 is not the bloom of its own {logs} log(s): the endpoint served an inconsistent \
                 receipt (a corrupted backend, or a tampered capture); the transaction is \
                 unverified"
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
            // The header hash does not cover the body listing, so an authentic
            // header can sit beside a listing the endpoint changed; the
            // transactions root is what ties the two together.
            Self::UncommittedBody { number, block_hash, served, committed } => write!(
                f,
                "the transactions served for block {number} ({block_hash}) rebuild transactions \
                 root {served}, but its header commits to {committed}: the endpoint served a \
                 block body the header does not commit to (an inconsistent backend, or a \
                 tampered capture); the block is unverified"
            ),
            // Each receipt was checked against the question it answers (its
            // transaction and inclusion); the root is what ties the set to the
            // block, so a set that does not rebuild it is not the block's.
            Self::UncommittedReceipts { number, block_hash, served, committed } => write!(
                f,
                "the on-chain receipts served for block {number} ({block_hash}) rebuild receipts \
                 root {served}, but its header commits to {committed}: the endpoint served \
                 receipts the block does not commit to (an inconsistent backend, or a tampered \
                 capture); every receipt of the block is unverified"
            ),
            // The receipts root covers each receipt's cumulative gas but not the
            // RPC `gasUsed` beside it, so the two are checked against each other.
            Self::InconsistentReceiptGas {
                number,
                block_hash,
                tx_hash,
                served,
                cumulative_before,
                cumulative_after,
            } => write!(
                f,
                "the on-chain receipt served for transaction {tx_hash} of block {number} \
                 ({block_hash}) reports gasUsed {served}, but the block's committed cumulative \
                 gas goes from {cumulative_before} to {cumulative_after} at it: the endpoint \
                 served a gasUsed that contradicts the receipts the header commits to (an \
                 inconsistent backend, or a tampered capture); every receipt of the block is \
                 unverified"
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

/// Authenticate a served block header: it must hash to the hash it was served
/// under, and it must be the height that was asked for.
///
/// Every other judgment in this module compares two answers the endpoint gave.
/// This one compares an answer against itself and against the question: a block
/// header is a consensus object whose hash and height are functions of its own
/// fields, so both can be checked without asking anything further. Nothing else
/// in the replay does check them — the inclusion, linkage and membership guards
/// all consume the reported hash, and every block is fetched by number without
/// ever reading the height back — so two forgeries would otherwise pass:
///
/// - Rewriting `timestamp`, `baseFeePerGas`, `gasLimit` or `parentHash` while keeping the original
///   `hash` moves the world the target executes in past every later guard.
/// - Answering `eth_getBlockByNumber(N)` with a self-consistent block `M` replays the body and
///   environment of `M` while the run reports it as `N`. Offsetting the parent fetch by the same
///   distance keeps the linkage guard satisfied, so nothing downstream notices.
///
/// The two checks are asked in that order: the height a header claims is one of
/// the fields the hash covers, so it is worth reading only once the header is
/// proven to be the one the endpoint vouches for.
///
/// The hash is recomputed from the served consensus fields rather than read back
/// from the response. `Header::hash()` / `Block::hash()` return the `hash` field
/// an RPC deserialization filled in — the very value being authenticated — the
/// same trap the transaction-level authentication meets with the envelope's
/// cached hash.
pub(super) fn authenticate_block_header(
    header: &Header,
    requested_number: u64,
) -> Result<(), Incoherence> {
    let computed = header.inner.hash_slow();
    if computed != header.hash {
        return Err(Incoherence::UnauthenticHeader {
            number: header.inner.number,
            served: header.hash,
            computed,
        });
    }
    if header.inner.number != requested_number {
        return Err(Incoherence::HeightMismatch {
            requested: requested_number,
            served: header.inner.number,
        });
    }
    Ok(())
}

/// Require that a served receipt's logs bloom is the bloom of its own logs.
///
/// The bloom is a consensus field of the receipt, but it is derived from the
/// logs: a receipt whose bloom says something its logs do not is not a receipt
/// any execution produced. Checking it at admission is what lets a comparison
/// of the logs stand for a comparison of the bloom too.
pub(super) fn require_receipt_bloom(
    tx_hash: B256,
    served: Bloom,
    logs: &[Log],
) -> Result<(), Incoherence> {
    if logs_bloom(logs) != served {
        return Err(Incoherence::UnauthenticReceiptBloom { tx_hash, logs: logs.len() });
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

/// Require that the body served for a block is the one its header commits to.
///
/// `transactions` are the EIP-2718 encodings of every transaction the body
/// lists, in body order, each already authenticated against its body-listed
/// hash. Those checks prove each transaction is the one its hash names; only the
/// header's `transactionsRoot` ties the listing itself to the block, since the
/// header hash does not cover it. A listing with a transaction left out, added,
/// or reordered under an authentic header is caught here, and it is the endpoint
/// contradicting itself rather than a replay that diverged: the root depends on
/// what was served, never on what executed.
pub(super) fn require_committed_body(
    number: u64,
    block_hash: B256,
    committed: B256,
    transactions: &[Bytes],
) -> Result<(), Incoherence> {
    let served = transactions_root(transactions);
    if served != committed {
        return Err(Incoherence::UncommittedBody { number, block_hash, served, committed });
    }
    Ok(())
}

/// Require that the receipts served for a whole block body are the ones its
/// header commits to.
///
/// `receipts` are the consensus receipts the endpoint served for every
/// transaction of the body, in body order, each already checked to describe its
/// transaction and this block. Those checks answer each receipt's own question;
/// only the header's `receiptsRoot` ties the set to the block, so a receipt
/// rewritten under a valid identity passes them and is caught here. A caller
/// holding receipts for only part of the body cannot rebuild the root and does
/// not ask.
pub(super) fn require_committed_receipts(
    number: u64,
    block_hash: B256,
    committed: B256,
    receipts: &[OpReceiptEnvelope],
) -> Result<(), Incoherence> {
    let served = receipts_root(receipts);
    if served != committed {
        return Err(Incoherence::UncommittedReceipts { number, block_hash, served, committed });
    }
    Ok(())
}

/// Require that every served receipt's `gasUsed` is its share of the block's
/// cumulative gas.
///
/// `receipts` are `(transaction, served gasUsed, cumulative gas)` for every
/// transaction of the body, in body order, whose receipts already rebuilt the
/// header's receipts root ([`require_committed_receipts`]). That root covers
/// the cumulative gas but not the RPC `gasUsed` field beside it, so a receipt
/// whose `gasUsed` alone was rewritten still authenticates; comparing the replay
/// against it would report a gas divergence the chain never had. On `MegaETH` a
/// receipt's `gasUsed` is exactly the rise in cumulative gas over the receipt
/// before it (over zero for the first), so any other value is the endpoint
/// contradicting the receipts it served.
pub(super) fn require_receipt_gas(
    number: u64,
    block_hash: B256,
    receipts: &[(B256, u64, u64)],
) -> Result<(), Incoherence> {
    let mut cumulative_before = 0;
    for &(tx_hash, served, cumulative_after) in receipts {
        if cumulative_after.checked_sub(cumulative_before) != Some(served) {
            return Err(Incoherence::InconsistentReceiptGas {
                number,
                block_hash,
                tx_hash,
                served,
                cumulative_before,
                cumulative_after,
            });
        }
        cumulative_before = cumulative_after;
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
    use alloy_primitives::Address;

    /// Distinct, recognizable hashes for the message assertions.
    const HASH_A: B256 = B256::repeat_byte(0xaa);
    const HASH_B: B256 = B256::repeat_byte(0xbb);
    const HASH_C: B256 = B256::repeat_byte(0xcc);

    /// Height [`sealed_header`] claims, and therefore the height a fetch has to
    /// have asked for to accept it.
    const SEALED_NUMBER: u64 = 22_945_844;

    /// A served header whose reported hash is the one its own fields produce.
    fn sealed_header() -> Header {
        let inner = alloy_consensus::Header {
            number: SEALED_NUMBER,
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
        let c = HASH_C.to_string();
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
                Incoherence::HeightMismatch { requested: 22_945_844, served: 22_945_843 },
                "the endpoint answered the fetch of block 22945844 with block 22945843: a \
                 numbered fetch was served a header from another height (an inconsistent \
                 backend, or a tampered capture); the block environment it describes is not the \
                 one the run asked for"
                    .to_string(),
            ),
            (
                Incoherence::UnauthenticReceiptBloom { tx_hash: HASH_A, logs: 2 },
                format!(
                    "the on-chain receipt served for transaction {a} carries a logs bloom that is \
                     not the bloom of its own 2 log(s): the endpoint served an inconsistent \
                     receipt (a corrupted backend, or a tampered capture); the transaction is \
                     unverified"
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
                Incoherence::UncommittedBody {
                    number: 12,
                    block_hash: HASH_A,
                    served: HASH_B,
                    committed: HASH_C,
                },
                format!(
                    "the transactions served for block 12 ({a}) rebuild transactions root {b}, \
                     but its header commits to {c}: the endpoint served a block body the header \
                     does not commit to (an inconsistent backend, or a tampered capture); the \
                     block is unverified"
                ),
            ),
            (
                Incoherence::UncommittedReceipts {
                    number: 12,
                    block_hash: HASH_A,
                    served: HASH_B,
                    committed: HASH_C,
                },
                format!(
                    "the on-chain receipts served for block 12 ({a}) rebuild receipts root {b}, \
                     but its header commits to {c}: the endpoint served receipts the block does \
                     not commit to (an inconsistent backend, or a tampered capture); every \
                     receipt of the block is unverified"
                ),
            ),
            (
                Incoherence::InconsistentReceiptGas {
                    number: 12,
                    block_hash: HASH_A,
                    tx_hash: HASH_B,
                    served: 21_001,
                    cumulative_before: 40_000,
                    cumulative_after: 61_000,
                },
                format!(
                    "the on-chain receipt served for transaction {b} of block 12 ({a}) reports \
                     gasUsed 21001, but the block's committed cumulative gas goes from 40000 to \
                     61000 at it: the endpoint served a gasUsed that contradicts the receipts the \
                     header commits to (an inconsistent backend, or a tampered capture); every \
                     receipt of the block is unverified"
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

    /// A header served under the hash of its own fields, at the height it was
    /// fetched under, authenticates.
    #[test]
    fn test_authenticate_block_header_accepts_a_sealed_header() {
        assert_eq!(authenticate_block_header(&sealed_header(), SEALED_NUMBER), Ok(()));
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

            // Asked for under the height the untampered header claims, so a
            // rewritten `number` is judged by the hash rather than by the
            // height: the served fields no longer produce the served hash, and
            // that is the more specific answer of the two.
            let Err(verdict) = authenticate_block_header(&header, SEALED_NUMBER) else {
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

        let verdict = authenticate_block_header(&header, SEALED_NUMBER)
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

    /// A header that authenticates is still rejected when it is not the height
    /// the fetch asked for, and the verdict names both heights.
    ///
    /// This is the forgery the hash check cannot see: the header is a real,
    /// self-consistent block — it is simply the answer to another question.
    #[test]
    fn test_authenticate_block_header_rejects_another_height() {
        for requested in [SEALED_NUMBER - 1, SEALED_NUMBER + 1, 0, u64::MAX] {
            assert_eq!(
                authenticate_block_header(&sealed_header(), requested),
                Err(Incoherence::HeightMismatch { requested, served: SEALED_NUMBER }),
                "a sealed header fetched as block {requested} must be rejected",
            );
        }
    }

    /// Genesis is the only height without a parent to fork from.
    /// A receipt is admitted exactly when its bloom is the bloom of its logs.
    #[test]
    fn test_require_receipt_bloom_accepts_only_the_bloom_of_the_logs() {
        let log = Log::new_unchecked(Address::repeat_byte(0xaa), vec![HASH_B], Default::default());
        let logs = [log];

        assert_eq!(require_receipt_bloom(HASH_A, logs_bloom(&logs), &logs), Ok(()));
        assert_eq!(require_receipt_bloom(HASH_A, Bloom::ZERO, &[]), Ok(()));
        for (served, logs) in [(Bloom::ZERO, &logs[..]), (Bloom::repeat_byte(0xff), &[][..])] {
            assert_eq!(
                require_receipt_bloom(HASH_A, served, logs),
                Err(Incoherence::UnauthenticReceiptBloom { tx_hash: HASH_A, logs: logs.len() }),
            );
        }
    }

    #[test]
    fn test_require_forkable_block_rejects_only_genesis() {
        assert_eq!(require_forkable_block(0), Err(Incoherence::GenesisPlacement));
        assert_eq!(require_forkable_block(1), Ok(()));
        assert_eq!(require_forkable_block(u64::MAX), Ok(()));
    }

    /// The served body authenticates exactly when it rebuilds the root the
    /// header commits to: a transaction left out, or two swapped, breaks it, and
    /// the verdict carries both roots.
    #[test]
    fn test_require_committed_body_accepts_only_the_committed_body() {
        let body = [Bytes::from_static(&[0x7e, 0x01]), Bytes::from_static(&[0x02, 0xc0])];
        let committed = transactions_root(&body);

        assert_eq!(require_committed_body(7, HASH_A, committed, &body), Ok(()));
        for served in [vec![body[0].clone()], vec![body[1].clone(), body[0].clone()], vec![]] {
            assert_eq!(
                require_committed_body(7, HASH_A, committed, &served),
                Err(Incoherence::UncommittedBody {
                    number: 7,
                    block_hash: HASH_A,
                    served: transactions_root(&served),
                    committed,
                }),
                "{served:?}"
            );
        }
    }

    /// A receipt with the given status and cumulative gas, as a block commits to
    /// it.
    fn committed_receipt(status: bool, cumulative_gas_used: u64) -> OpReceiptEnvelope {
        let receipt = alloy_consensus::Receipt {
            status: alloy_consensus::Eip658Value::Eip658(status),
            cumulative_gas_used,
            logs: vec![],
        };
        OpReceiptEnvelope::Eip1559(receipt.with_bloom())
    }

    /// The served receipts authenticate exactly when they rebuild the root the
    /// header commits to; a single rewritten field of a single receipt breaks
    /// it, and the verdict carries both roots.
    #[test]
    fn test_require_committed_receipts_accepts_only_the_committed_set() {
        let receipts = [committed_receipt(true, 21_000), committed_receipt(true, 42_000)];
        let committed = receipts_root(&receipts);

        assert_eq!(require_committed_receipts(7, HASH_A, committed, &receipts), Ok(()));

        let tampered = [committed_receipt(true, 21_000), committed_receipt(false, 42_000)];
        assert_eq!(
            require_committed_receipts(7, HASH_A, committed, &tampered),
            Err(Incoherence::UncommittedReceipts {
                number: 7,
                block_hash: HASH_A,
                served: receipts_root(&tampered),
                committed,
            }),
        );

        let reordered = [receipts[1].clone(), receipts[0].clone()];
        assert!(
            require_committed_receipts(7, HASH_A, committed, &reordered).is_err(),
            "the root commits to the body order"
        );
    }

    /// Each served `gasUsed` must be its receipt's rise in cumulative gas, the
    /// first one's over zero; a forged value anywhere, or cumulative gas that
    /// falls, is named with the receipt it is found at.
    #[test]
    fn test_require_receipt_gas_accepts_only_the_cumulative_deltas() {
        let honest = [(HASH_A, 40_000, 40_000), (HASH_B, 21_000, 61_000), (HASH_C, 0, 61_000)];
        assert_eq!(require_receipt_gas(7, HASH_A, &honest), Ok(()));
        assert_eq!(require_receipt_gas(7, HASH_A, &[]), Ok(()));

        let first_forged = [(HASH_A, 39_999, 40_000), (HASH_B, 21_000, 61_000)];
        assert_eq!(
            require_receipt_gas(7, HASH_C, &first_forged),
            Err(Incoherence::InconsistentReceiptGas {
                number: 7,
                block_hash: HASH_C,
                tx_hash: HASH_A,
                served: 39_999,
                cumulative_before: 0,
                cumulative_after: 40_000,
            }),
        );
        let later_forged = [(HASH_A, 40_000, 40_000), (HASH_B, 1, 61_000)];
        assert!(matches!(
            require_receipt_gas(7, HASH_C, &later_forged),
            Err(Incoherence::InconsistentReceiptGas { tx_hash, served: 1, .. }) if tx_hash == HASH_B
        ));
        let falling = [(HASH_A, 40_000, 40_000), (HASH_B, 0, 39_000)];
        assert!(require_receipt_gas(7, HASH_C, &falling).is_err(), "cumulative gas cannot fall");
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
