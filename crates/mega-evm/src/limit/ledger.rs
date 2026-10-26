//! The engine's ledger of what each frame start owes and holds, kept in a debug build beside the
//! figures the revm fork carries, and checked against them.
//!
//! The revm fork carries four figures of a frame's start on the frame's gas: the shares of its
//! caller's three pools — data size, KV updates, state growth — with their nets and crossings; the
//! part of its caller's EIP-8037 reservoir the caller keeps while it runs; the start charge its
//! failure gives back to the caller; and the history allowance a value call gives it. The ledger
//! keeps the same four for every start, computed by the engine's own rules, in an entry per open
//! start.
//!
//! It is driven by the start lifecycle revm owns and by nothing else: an entry opens when revm
//! opens the start (`Host::frame_split`), settles when revm settles it (`Host::frame_settle`), and
//! the ledger is dropped when the lifecycle ends, settled or discarded
//! (`Host::frame_lifecycle_end`). The transaction's start resets it, so nothing a context's ledger
//! held carries into the next transaction. The ledger pairs nothing: revm's record says which start
//! settles, and the ledger asserts it is the one on top.
//!
//! # The rules
//!
//! - **What a start owes is parked where it is made**, on the entry of the frame that makes it, and
//!   the split that opens the start takes it ([`park`](StartLedger::park)): an instruction's start
//!   at the frame-start hook, after the hook's charge; a `keylessDeploy` call's creation where its
//!   builder takes the creation's shares; the transaction's start where its first frame's input is
//!   built ([`park_transaction`](StartLedger::park_transaction)). A start dropped before revm
//!   consumes it loses what was parked for it, and the frame's next start parks over it. The
//!   stand-in revm settles for a transaction whose runtime gas phase ran out of gas has nothing
//!   parked: it starts with unlimited pools, as the transaction's rebuilt gas holds them.
//! - **A child's share of each pool** is all but `kept` per cent of what its caller has left,
//!   rounded down, taken out of the caller's entry where it is parked; the caller keeps `kept` per
//!   cent of its reservoir, rounded down. `kept` is the engine's
//!   ([`frame_kept_percent`](crate::MegaContext)): two on the spec's configuration, none on the
//!   neutral one, which forwards the whole of each as Ethereum does. When the child's frame starts,
//!   its shares are held to the frame cap and, for a caller whose net is negative, to the share of
//!   the caller's budget, and the excess goes back to the caller
//!   ([`cap_child`](StartLedger::cap_child)).
//! - **A draw goes to the top entry**, the running frame's start: a completed write, a frame
//!   start's records on the started frame, a deposit's code on the creating frame, a creator's
//!   nonce record on the creator once its creation returned ([`draw`](StartLedger::draw)); a log's
//!   history draws the top entry's allowance first
//!   ([`draw_allowance`](StartLedger::draw_allowance)).
//! - **A precompile's draws are adopted when it answers**, fail-closed: a draw past the share
//!   empties the pool, caps the net at the share and sets the crossing, whatever the answer's
//!   status ([`adopt_provider_draws`](StartLedger::adopt_provider_draws)).
//! - **A settlement** merges the child's entry into its caller's ([`settle`](StartLedger::settle)),
//!   as the fork settles a frame's gas: on success the child's remaining pools join what the caller
//!   kept and its nets the caller's; on a revert or a halt its draws are undone, and its whole
//!   share comes back. The start charge comes back on a failure, and for a creation also when it
//!   took no address. An inspector's answer, and a result whose gas an inspector replaced, settle
//!   as unlimited pools with no net; neither an inspector's answer nor an outcome an inspector
//!   rebuilt gives the start charge back.
//!
//! # The comparison
//!
//! At every settlement the ledger compares what it computed with the fork's figures for the same
//! start: each of the caller's pools, nets and crossings once the child merged into it; the
//! reservoir the caller kept, with the reservoir revm says the caller held before it settled the
//! child; the start charge the fork gave back; and, for a frame revm built whose gas no inspector
//! replaced, the allowance the frame returns. At the lifecycle's successful end the ledger holds no
//! entry.
//!
//! A difference is a panic. The inspector shapes whose figures the owner accepted to change when
//! the ledger becomes what the engine reads differ by design: a tracker restored from a snapshot,
//! an interpreter replaced, an answer, an input or a result built with pools, an allowance or a
//! charge an inspector set, a tracker or an outcome copied from another start. A test that runs
//! one has the context record the differences instead, and asserts each
//! (`test_utils::tolerate_start_ledger_divergences`). After a recorded difference in a caller's
//! pools the ledger takes the fork's, so what it records is where the two part, not what follows.

#![cfg_attr(not(any(test, feature = "test-utils")), allow(unreachable_pub))]

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use revm::context_interface::{
    cfg::gas::{GasTracker, PoolAmounts, PoolId},
    host::{FrameSettle, SettleStatus},
};

use super::Counted;

/// A figure of a settled start the ledger compares with the fork's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerFigure {
    /// What the start's caller may still draw on a pool, once the start settled into it.
    PoolRemaining(PoolId),
    /// The caller's net draw on a pool, once the start settled into it.
    PoolNet(PoolId),
    /// Whether a draw on the caller's pool was refused (`1`) or not (`0`).
    PoolCrossing(PoolId),
    /// The part of its reservoir the caller kept while the start ran.
    KeptReservoir,
    /// The history gas the start's settlement gave back to its caller.
    StartChargeRefund,
    /// What the frame returned of its history allowance.
    Allowance,
}

/// A figure of a settled start in which the ledger and the fork differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerDivergence {
    /// The identity revm gave the start.
    pub start: u32,
    /// The started frame's depth.
    pub depth: usize,
    /// The figure.
    pub figure: LedgerFigure,
    /// The ledger's value.
    pub ledger: i128,
    /// The fork's value.
    pub fork: i128,
}

/// What a start owes, decided where it is made and taken by the split that opens it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Parked {
    /// The child's share of each of its caller's pools.
    shares: PoolAmounts,
    /// The part of its reservoir the caller keeps while the child runs.
    kept_reservoir: u64,
    /// The history gas the start charged its caller for the records on the child, which the
    /// child's failure gives back.
    charge: u64,
    /// The history gas the child may spend on logs before it charges its gas.
    allowance: u64,
}

impl Parked {
    /// What the transaction's start owes: its pools, `pools`, the start `charge` of the record its
    /// frame makes, and no kept reservoir and no allowance, its frame taking the transaction's
    /// whole reservoir and making no value call.
    pub(crate) const fn transaction(pools: PoolAmounts, charge: u64) -> Self {
        Self { shares: pools, kept_reservoir: 0, charge, allowance: 0 }
    }
}

/// What a transaction's start owes when nothing was parked for it: the stand-in revm settles for a
/// transaction whose runtime gas phase ran out of gas, which carries every pool of the rebuilt
/// gas, unlimited, and owes nothing else.
const STAND_IN: Parked = Parked::transaction(PoolAmounts::UNLIMITED, 0);

/// What a start owes when an inspector consumed an input it built itself, which no start parked
/// anything for: nothing, and no pool.
const UNPARKED: Parked = Parked::transaction(PoolAmounts::ZERO, 0);

/// A frame's three pools as the engine keeps them: what it may still draw, its signed net draw,
/// and whether a draw it could not pay was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pools {
    remaining: PoolAmounts,
    net: [i64; PoolId::COUNT],
    crossings: [bool; PoolId::COUNT],
}

impl Pools {
    /// Pools that start at `shares`, undrawn.
    const fn of(shares: PoolAmounts) -> Self {
        Self { remaining: shares, net: [0; PoolId::COUNT], crossings: [false; PoolId::COUNT] }
    }

    /// The pools `gas` holds.
    fn on(gas: &GasTracker) -> Self {
        Self {
            remaining: gas.pools_remaining(),
            net: PoolId::ALL.map(|pool| gas.pool_spent(pool)),
            crossings: PoolId::ALL.map(|pool| gas.pool_crossing(pool)),
        }
    }

    /// Draws `amount` on `pool`; a draw the pool cannot pay takes nothing and sets its crossing.
    fn record(&mut self, pool: PoolId, amount: u64) -> bool {
        let index = pool.index();
        let Some(left) = self.remaining.get(pool).checked_sub(amount) else {
            self.crossings[index] = true;
            return false;
        };
        self.remaining.set(pool, left);
        self.net[index] = self.net[index].saturating_add(signed(amount));
        true
    }

    /// Gives `amount` back to `pool`, lowering its net, below zero for a draw a caller made.
    fn refill(&mut self, pool: PoolId, amount: u64) {
        let index = pool.index();
        self.remaining.set(pool, self.remaining.get(pool).saturating_add(amount));
        self.net[index] = self.net[index].saturating_sub(signed(amount));
    }

    /// Undoes every draw, for a frame that failed: each pool back at what it started with.
    fn roll_back(&mut self) {
        for pool in PoolId::ALL {
            let index = pool.index();
            let remaining = self.remaining.get(pool).saturating_add_signed(self.net[index]);
            self.remaining.set(pool, remaining);
            self.net[index] = 0;
        }
    }

    /// Takes back what a child that `succeeded`, or failed, returns: its remaining pools join what
    /// this frame kept, and its nets this frame's when it succeeded. A failed child's draws are
    /// undone first. A child's crossings stay the child's.
    fn absorb(&mut self, mut child: Self, succeeded: bool) {
        if !succeeded {
            child.roll_back();
        }
        for pool in PoolId::ALL {
            let index = pool.index();
            if succeeded {
                self.net[index] = self.net[index].saturating_add(child.net[index]);
            }
            let remaining = self.remaining.get(pool).saturating_add(child.remaining.get(pool));
            self.remaining.set(pool, remaining);
        }
    }
}

/// `amount` as a signed net, saturating.
const fn signed(amount: u64) -> i64 {
    if amount > i64::MAX as u64 {
        i64::MAX
    } else {
        amount as i64
    }
}

/// `kept` per cent of `amount`, rounded down: the part of its reservoir a caller keeps when it
/// starts a frame.
const fn kept_part(amount: u64, kept: u64) -> u64 {
    if kept >= 100 {
        return amount;
    }
    (amount as u128 * kept as u128 / 100) as u64
}

/// The share of a pool holding `remaining` a child starts with: `100 − kept` per cent of it,
/// rounded down.
const fn pool_share(remaining: u64, kept: u64) -> u64 {
    if kept >= 100 {
        return 0;
    }
    (remaining as u128 * (100 - kept) as u128 / 100) as u64
}

/// One open start.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The identity revm gave the start.
    id: u32,
    /// The started frame's depth.
    depth: usize,
    /// The shares its frame started with, once its start held them to the frame cap: what an
    /// answer rebuilt on its untouched gas carries.
    shares: PoolAmounts,
    /// Its pools as its frame draws them.
    pools: Pools,
    /// The part of its reservoir the start's caller kept.
    kept_reservoir: u64,
    /// The start charge its failure gives back.
    charge: u64,
    /// What is left of its history allowance.
    allowance: u64,
    /// Whether revm built a frame for it, which ran or ended before it ran; an answer has none.
    built: bool,
    /// What the next start its frame makes owes.
    parked: Option<Parked>,
}

impl Entry {
    const fn open(id: u32, depth: usize, owed: Parked) -> Self {
        Self {
            id,
            depth,
            shares: owed.shares,
            pools: Pools::of(owed.shares),
            kept_reservoir: owed.kept_reservoir,
            charge: owed.charge,
            allowance: owed.allowance,
            built: false,
            parked: None,
        }
    }
}

/// A start's settlement as revm reports it ([`FrameSettle`]), in the ledger's own terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settlement {
    id: u32,
    depth: usize,
    status: SettleStatus,
    created: Option<bool>,
    inspector_answer: bool,
    gas_replaced: bool,
    outcome_rebuilt: bool,
    held_reservoir: u64,
}

impl From<&FrameSettle> for Settlement {
    fn from(settle: &FrameSettle) -> Self {
        Self {
            id: settle.id,
            depth: settle.depth,
            status: settle.status,
            created: settle.created,
            inspector_answer: settle.inspector_answer,
            gas_replaced: settle.gas_replaced,
            outcome_rebuilt: settle.outcome_rebuilt,
            held_reservoir: settle.held_reservoir,
        }
    }
}

/// The fork's figures of the result about to settle, read before revm consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Returning {
    /// The history the fork gives back to the caller for the start.
    refund: u64,
    /// What the result's gas holds of the frame's history allowance.
    allowance: u64,
}

impl Returning {
    /// `refund`, the history of the start charge the fork gives back, and `allowance`, what the
    /// result's gas holds of the frame's history allowance.
    pub(crate) const fn new(refund: u64, allowance: u64) -> Self {
        Self { refund, allowance }
    }
}

/// The engine's ledger of the open starts of the running transaction. See the module
/// documentation.
#[derive(Clone, Debug, Default)]
pub(crate) struct StartLedger {
    /// One entry per open start, the transaction's first and the innermost last.
    entries: Vec<Entry>,
    /// What the transaction's start owes, parked where its first frame's input is built.
    transaction: Option<Parked>,
    /// The fork's figures of the result settling next.
    returning: Option<Returning>,
    /// The differences found, when the ledger records them rather than panics.
    divergences: Option<Vec<LedgerDivergence>>,
}

impl StartLedger {
    /* The lifecycle */

    /// Parks what the transaction's start owes, or forgets what was parked when the first frame's
    /// input could not be built (`None`), whose stand-in owes nothing.
    pub(crate) const fn park_transaction(&mut self, owed: Option<Parked>) {
        self.transaction = owed;
    }

    /// Opens the start revm just opened, `id` at `depth` under `parent`, with what was parked for
    /// it. The transaction's start, with no parent, resets the ledger.
    pub(crate) fn split(&mut self, id: u32, parent: Option<u32>, depth: usize) {
        let owed = match parent {
            None => {
                self.entries.clear();
                self.returning = None;
                self.transaction.take().unwrap_or(STAND_IN)
            }
            Some(parent) => {
                let caller = self.entries.last_mut();
                let Some(caller) =
                    caller.filter(|caller| caller.id == parent && caller.depth + 1 == depth)
                else {
                    panic!(
                        "start {id} at depth {depth} opened under {parent}, not the ledger's top"
                    )
                };
                caller.parked.take().unwrap_or(UNPARKED)
            }
        };
        self.entries.push(Entry::open(id, depth, owed));
    }

    /// Ends the lifecycle: every start settled when it `settled`, or what is open discarded.
    pub(crate) fn end(&mut self, settled: bool) {
        if settled {
            assert!(
                self.entries.is_empty(),
                "the lifecycle settled with starts open in the ledger: {:?}",
                self.entries
            );
        }
        self.entries.clear();
        self.returning = None;
        self.transaction = None;
    }

    /* What a start owes */

    /// Parks on the running frame's entry what the start it is making owes: the share of each of
    /// its pools, taken out of them, and the part of `reservoir` it keeps, all by `kept`; the
    /// start's `charge`; and the child's `allowance`. `reservoir` is what the caller holds once the
    /// start's charges are made. Overwrites what an earlier start of the frame parked and no split
    /// took.
    pub(crate) fn park(&mut self, kept: u64, reservoir: u64, charge: u64, allowance: u64) {
        let Some(caller) = self.entries.last_mut() else { return };
        let mut shares = PoolAmounts::ZERO;
        for pool in PoolId::ALL {
            let remaining = caller.pools.remaining.get(pool);
            let share = pool_share(remaining, kept);
            caller.pools.remaining.set(pool, remaining - share);
            shares.set(pool, share);
        }
        let kept_reservoir = kept_part(reservoir, kept);
        caller.parked = Some(Parked { shares, kept_reservoir, charge, allowance });
    }

    /// Holds the shares of the start on top, which its caller below it is suspended on, to `caps`
    /// and, for a caller whose net is negative, to the share of its budget by `kept`, and gives the
    /// excess back to the caller. The caller's budget is what it holds with the share and its net
    /// together, so a negative net never enlarges a child's share.
    pub(crate) fn cap_child(&mut self, caps: PoolAmounts, kept: u64) {
        let [.., caller, child] = self.entries.as_mut_slice() else { return };
        for pool in PoolId::ALL {
            let share = child.shares.get(pool);
            let mut allowed = caps.get(pool);
            let net = caller.pools.net[pool.index()];
            if net < 0 {
                let had = caller.pools.remaining.get(pool).saturating_add(share);
                allowed = allowed.min(pool_share(had.saturating_add_signed(net), kept));
            }
            let Some(excess) = share.checked_sub(allowed).filter(|excess| *excess != 0) else {
                continue;
            };
            let remaining = caller.pools.remaining.get(pool).saturating_add(excess);
            caller.pools.remaining.set(pool, remaining);
            child.shares.set(pool, allowed);
            child.pools.remaining.set(pool, allowed);
        }
    }

    /// Notes that revm built a frame for the start on top.
    pub(crate) fn built(&mut self) {
        if let Some(entry) = self.entries.last_mut() {
            entry.built = true;
        }
    }

    /* Draws */

    /// Draws `counted` on the top entry's pools, which the limits admitted: what it takes back goes
    /// back to each pool first, then what it adds is drawn.
    pub(crate) fn draw(&mut self, counted: &Counted) {
        let Some(entry) = self.entries.last_mut() else { return };
        for pool in PoolId::ALL {
            let (added, taken) = (counted.added.get(pool), counted.taken.get(pool));
            if added == 0 && taken == 0 {
                continue;
            }
            entry.pools.refill(pool, taken);
            if !entry.pools.record(pool, added) {
                // Admitted by the limits, so refused only past a saturated unlimited pool, which is
                // no crossing.
                entry.pools.crossings[pool.index()] = false;
            }
        }
    }

    /// Draws `cost` of history on the top entry's allowance first, and answers the part it did not
    /// cover.
    pub(crate) fn draw_allowance(&mut self, cost: u64) -> u64 {
        let Some(entry) = self.entries.last_mut() else { return cost };
        let drawn = cost.min(entry.allowance);
        entry.allowance -= drawn;
        cost - drawn
    }

    /// Adopts `draws`, what a precompile's provider drew on each pool of its answer to the start on
    /// top, fail-closed: a draw within the share is drawn, and one past it empties the pool, caps
    /// the net at the share and sets the crossing. The answer's status is the provider's either
    /// way.
    pub(crate) fn adopt_provider_draws(&mut self, draws: [i64; PoolId::COUNT]) {
        let Some(entry) = self.entries.last_mut() else { return };
        for pool in PoolId::ALL {
            let (index, share, drawn) = (pool.index(), entry.shares.get(pool), draws[pool.index()]);
            if drawn > 0 && drawn as u64 > share {
                entry.pools.remaining.set(pool, 0);
                entry.pools.net[index] = signed(share);
                entry.pools.crossings[index] = true;
            } else {
                entry
                    .pools
                    .remaining
                    .set(pool, share.saturating_add_signed(drawn.saturating_neg()));
                entry.pools.net[index] = drawn;
            }
        }
    }

    /// Puts the start on top back at its shares, undrawn: its answer was rebuilt on the gas its
    /// caller forwarded, untouched.
    pub(crate) fn rebuild_on_shares(&mut self) {
        if let Some(entry) = self.entries.last_mut() {
            entry.pools = Pools::of(entry.shares);
        }
    }

    /* Settlement */

    /// Notes the fork's figures of the result about to settle.
    pub(crate) const fn returning(&mut self, figures: Returning) {
        self.returning = Some(figures);
    }

    /// Settles the start on top, which revm just settled into `gas`, its caller's — the
    /// transaction's gas, for the transaction's start — and compares the caller's pools, what the
    /// caller kept of its reservoir, the start charge given back and the allowance the frame
    /// returned with the fork's.
    pub(crate) fn settle(&mut self, settlement: &Settlement, gas: &GasTracker) {
        let Some(child) = self.entries.pop() else {
            panic!("start {} settled with no start open in the ledger", settlement.id)
        };
        assert_eq!(
            (child.id, child.depth),
            (settlement.id, settlement.depth),
            "the start on top of the ledger is not the one revm settled"
        );
        let Some(fork) = self.returning.take() else {
            panic!("start {} settled without the fork's figures of its result", settlement.id)
        };
        let inspected = settlement.inspector_answer || settlement.gas_replaced;
        let pools = if inspected { Pools::of(PoolAmounts::UNLIMITED) } else { child.pools };
        let succeeded = settlement.status == SettleStatus::Ok;
        let failed = !succeeded || settlement.created == Some(false);
        let refunds = failed && !settlement.inspector_answer && !settlement.outcome_rebuilt;
        let refund = if refunds { child.charge } else { 0 };
        let returned = child.built && !inspected;

        // The transaction's gas holds none of its pools while its frame runs, which took them
        // whole.
        let mut transaction = Pools::of(PoolAmounts::ZERO);
        let caller = match self.entries.last_mut() {
            Some(caller) => &mut caller.pools,
            None => &mut transaction,
        };
        caller.absorb(pools, succeeded);
        let ledger = *caller;
        let on_gas = Pools::on(gas);

        let mut differences = Vec::new();
        for pool in PoolId::ALL {
            let index = pool.index();
            differences.extend([
                (
                    LedgerFigure::PoolRemaining(pool),
                    i128::from(ledger.remaining.get(pool)),
                    i128::from(on_gas.remaining.get(pool)),
                ),
                (LedgerFigure::PoolNet(pool), ledger.net[index].into(), on_gas.net[index].into()),
                (
                    LedgerFigure::PoolCrossing(pool),
                    ledger.crossings[index].into(),
                    on_gas.crossings[index].into(),
                ),
            ]);
        }
        differences.extend([
            (
                LedgerFigure::KeptReservoir,
                child.kept_reservoir.into(),
                settlement.held_reservoir.into(),
            ),
            (LedgerFigure::StartChargeRefund, refund.into(), fork.refund.into()),
        ]);
        if returned {
            differences.push((
                LedgerFigure::Allowance,
                child.allowance.into(),
                fork.allowance.into(),
            ));
        }
        differences.retain(|(_, ledger, fork)| ledger != fork);
        if differences.is_empty() {
            return;
        }
        let pools_differ = differences.iter().any(|(figure, ..)| {
            matches!(
                figure,
                LedgerFigure::PoolRemaining(_) |
                    LedgerFigure::PoolNet(_) |
                    LedgerFigure::PoolCrossing(_)
            )
        });
        let Some(recorded) = &mut self.divergences else {
            panic!(
                "the start ledger parts from the fork at start {} (depth {}), settled {:?}: \
                 (figure, ledger, fork) {differences:?}",
                child.id, child.depth, settlement
            )
        };
        recorded.extend(differences.into_iter().map(|(figure, ledger, fork)| LedgerDivergence {
            start: child.id,
            depth: child.depth,
            figure,
            ledger,
            fork,
        }));
        if pools_differ {
            if let Some(caller) = self.entries.last_mut() {
                caller.pools = on_gas;
            }
        }
    }

    /// The shares the start on top started with and the pools of the start below it.
    #[cfg(test)]
    pub(crate) fn top_shares_and_caller_pools(&self) -> (PoolAmounts, PoolAmounts) {
        let [.., caller, child] = self.entries.as_slice() else { panic!("two starts are open") };
        (child.shares, caller.pools.remaining)
    }

    /* The differences an inspector's shape makes */

    /// Records every difference from now on rather than panicking on it.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn tolerate_divergences(&mut self) {
        self.divergences.get_or_insert_with(Vec::new);
    }

    /// The differences recorded since the last were taken; `None` unless they are recorded.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn take_divergences(&mut self) -> Option<Vec<LedgerDivergence>> {
        self.divergences.as_mut().map(core::mem::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        limit::{DATA_SIZE_POOL, KV_UPDATE_POOL, STATE_GROWTH_POOL},
        satin_gas_params,
    };
    use revm::{
        context_interface::cfg::GasParams, handler::handle_reservoir_remaining_gas,
        interpreter::InstructionResult, primitives::hardfork::SpecId,
    };

    /// The part of its pools and reservoir the spec's configuration has a caller keep.
    const KEPT: u64 = crate::evm::FRAME_KEPT_PERCENT;

    /// A settlement of start `id` at `depth` with `status`, which no inspector touched, of a call.
    fn settled(id: u32, depth: usize, status: SettleStatus, held_reservoir: u64) -> Settlement {
        Settlement {
            id,
            depth,
            status,
            created: None,
            inspector_answer: false,
            gas_replaced: false,
            outcome_rebuilt: false,
            held_reservoir,
        }
    }

    /// The status a settlement reports of `result`, as revm's record reports it.
    fn status_of(result: InstructionResult) -> SettleStatus {
        if result.is_ok() {
            SettleStatus::Ok
        } else if result.is_revert() {
            SettleStatus::Revert
        } else {
            SettleStatus::Halt
        }
    }

    /// A tracker with `pools`, regular gas and `reservoir`.
    fn tracker(pools: PoolAmounts, reservoir: u64) -> GasTracker {
        let mut gas = GasTracker::new(10_000_000, 10_000_000, reservoir);
        gas.set_pools_remaining(pools);
        gas
    }

    /// A ledger whose transaction's start, 1, has `pools`, open.
    fn opened(pools: PoolAmounts) -> StartLedger {
        let mut ledger = StartLedger::default();
        ledger.park_transaction(Some(Parked::transaction(pools, 0)));
        ledger.split(1, None, 0);
        ledger
    }

    /// The pools of the open start on top.
    fn top(ledger: &StartLedger) -> Pools {
        ledger.entries.last().expect("a start is open").pools
    }

    const POOLS: PoolAmounts = PoolAmounts::ZERO
        .with(DATA_SIZE_POOL, 10_000)
        .with(KV_UPDATE_POOL, 250)
        .with(STATE_GROWTH_POOL, 49);

    /// A child's share of each pool is what the fork forwards on the spec's schedule, the floor of
    /// 98 per cent, and the caller keeps the rest; the caller keeps the part of its reservoir the
    /// fork leaves it, the floor of 2 per cent, so nothing below 50. On the neutral configuration
    /// the caller keeps nothing of either, as the fork forwards on a schedule that keeps nothing.
    #[test]
    fn test_a_start_takes_the_forks_shares_and_leaves_the_caller_its_part() {
        let amsterdam = GasParams::new_spec(SpecId::AMSTERDAM);
        for (kept, params) in [(KEPT, satin_gas_params()), (0, amsterdam)] {
            for reservoir in [0, 1, 49, 50, 99, 100, 12_345, u64::MAX] {
                for pools in [POOLS, PoolAmounts::UNLIMITED, PoolAmounts::ZERO] {
                    let case = format!("kept {kept}, reservoir {reservoir}, pools {pools:?}");
                    let mut ledger = opened(pools);
                    ledger.park(kept, reservoir, 7, 9);
                    let mut caller = tracker(pools, reservoir);
                    let shares = caller.forward_to_child(&params);
                    assert_eq!(top(&ledger).remaining, caller.pools_remaining(), "{case}");
                    let parked = ledger.entries[0].parked.expect("parked");
                    assert_eq!(parked.shares, shares.pools, "{case}");
                    assert_eq!(parked.kept_reservoir, caller.reservoir(), "{case}");
                    assert_eq!((parked.charge, parked.allowance), (7, 9), "{case}");
                }
            }
        }
    }

    /// What a start owes is the frame's own scratch: the split that opens it takes it, a start
    /// dropped before revm consumed it loses its shares — the caller keeps only its part — and the
    /// frame's next start parks over it.
    #[test]
    fn test_a_dropped_start_loses_its_shares_and_the_next_start_parks_over_it() {
        let mut ledger = opened(POOLS);
        ledger.park(KEPT, 0, 1, 0);
        let after_first = top(&ledger).remaining;
        ledger.park(KEPT, 0, 2, 0);
        let parked = ledger.entries[0].parked.expect("parked");
        assert_eq!(parked.charge, 2, "the second start's");
        for pool in PoolId::ALL {
            let (first, second) = (after_first.get(pool), top(&ledger).remaining.get(pool));
            assert_eq!(parked.shares.get(pool), pool_share(first, KEPT), "{pool:?}");
            assert_eq!(second, first - pool_share(first, KEPT), "{pool:?}: the first share lost");
        }
        ledger.split(2, Some(1), 1);
        assert_eq!(ledger.entries[0].parked, None, "taken");
        assert_eq!(ledger.entries[1].shares, parked.shares);
        assert_eq!(ledger.entries[1].charge, 2);
    }

    /// A child's draws settle into its caller as the fork settles them: a success hands its
    /// remaining pools and its nets to the caller, a revert and a halt give its whole share back
    /// and leave the caller's nets alone; a child's take-back of a caller's draw nets out on
    /// success. The comparison finds nothing to part on.
    #[test]
    fn test_a_settlement_merges_as_the_fork_does() {
        let results =
            [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas];
        for result in results {
            let case = format!("{result:?}");
            let mut ledger = opened(POOLS);
            let mut caller = tracker(POOLS, 0);
            // The caller draws, then starts a child, which draws and takes back.
            let caller_draw = Counted::records(3, 0, false).with_growth(2, 0);
            ledger.draw(&caller_draw);
            for pool in PoolId::ALL {
                assert!(caller.record_pool(pool, caller_draw.added.get(pool)));
            }
            ledger.park(KEPT, 0, 0, 0);
            ledger.split(2, Some(1), 1);
            let mut child = tracker(caller.forward_to_child(&satin_gas_params()).pools, 0);
            let child_draw = Counted::records(5, 2, true).with_growth(1, 2);
            ledger.draw(&child_draw);
            for pool in PoolId::ALL {
                child.refill_pool(pool, child_draw.taken.get(pool));
                assert!(child.record_pool(pool, child_draw.added.get(pool)));
            }
            assert_eq!(top(&ledger), Pools::on(&child), "{case}: the child's draws");
            handle_reservoir_remaining_gas(result, &mut caller, &mut child);
            ledger.returning(Returning::new(0, 0));
            ledger.settle(&settled(2, 1, status_of(result), 0), &caller);
            assert_eq!(top(&ledger), Pools::on(&caller), "{case}: the caller's");
        }
    }

    /// The start charge comes back on a call's failure, and on a creation's too when it took no
    /// address; never on a success, an inspector's answer or an outcome an inspector rebuilt.
    #[test]
    fn test_the_start_charge_comes_back_on_failure() {
        // (status, created, inspector answer, outcome rebuilt, refunded)
        let cases = [
            (SettleStatus::Ok, None, false, false, false),
            (SettleStatus::Revert, None, false, false, true),
            (SettleStatus::Halt, None, false, false, true),
            (SettleStatus::Ok, Some(true), false, false, false),
            (SettleStatus::Ok, Some(false), false, false, true),
            (SettleStatus::Revert, Some(false), false, false, true),
            (SettleStatus::Revert, None, true, false, false),
            (SettleStatus::Revert, None, false, true, false),
            (SettleStatus::Ok, Some(false), false, true, false),
        ];
        for (status, created, inspector_answer, outcome_rebuilt, refunded) in cases {
            let case = format!("{status:?}, {created:?}, {inspector_answer}, {outcome_rebuilt}");
            let mut ledger = opened(PoolAmounts::UNLIMITED);
            ledger.tolerate_divergences();
            ledger.park(KEPT, 0, 123, 0);
            ledger.split(2, Some(1), 1);
            ledger.returning(Returning::new(0, 0));
            let settlement = Settlement {
                created,
                inspector_answer,
                outcome_rebuilt,
                ..settled(2, 1, status, 0)
            };
            ledger.settle(&settlement, &tracker(PoolAmounts::UNLIMITED, 0));
            let refund = ledger
                .take_divergences()
                .unwrap_or_default()
                .into_iter()
                .find(|divergence| divergence.figure == LedgerFigure::StartChargeRefund)
                .map(|divergence| divergence.ledger);
            assert_eq!(refund, refunded.then_some(123), "{case}");
        }
    }

    /// An inspector's answer and a result whose tracker an inspector replaced settle as unlimited
    /// pools with no net, as the fork settles one built with `Gas::new`; an outcome an inspector
    /// rebuilt around the frame's own tracker settles on the frame's pools.
    #[test]
    fn test_an_inspectors_answer_and_a_replaced_tracker_settle_as_unlimited_pools() {
        for (inspector_answer, gas_replaced, outcome_rebuilt) in
            [(true, false, false), (false, true, false), (false, false, true)]
        {
            let case = format!("{inspector_answer}, {gas_replaced}, {outcome_rebuilt}");
            let mut ledger = opened(POOLS);
            let mut caller = tracker(POOLS, 0);
            ledger.park(KEPT, 0, 0, 0);
            ledger.split(2, Some(1), 1);
            let mut child = tracker(caller.forward_to_child(&satin_gas_params()).pools, 0);
            ledger.draw(&Counted::records(1, 0, false));
            for pool in [DATA_SIZE_POOL, KV_UPDATE_POOL] {
                assert!(child.record_pool(pool, ledger.entries[1].pools.net[pool.index()] as u64));
            }
            let mut returned = if outcome_rebuilt { child } else { GasTracker::new(0, 0, 0) };
            handle_reservoir_remaining_gas(InstructionResult::Stop, &mut caller, &mut returned);
            ledger.returning(Returning::new(0, 0));
            let settlement = Settlement {
                inspector_answer,
                gas_replaced,
                outcome_rebuilt,
                ..settled(2, 1, SettleStatus::Ok, 0)
            };
            ledger.settle(&settlement, &caller);
            assert_eq!(top(&ledger), Pools::on(&caller), "{case}");
        }
    }

    /// A precompile's draws are adopted fail-closed, as the fork adopts them: within the share they
    /// are drawn, and past it the pool is emptied, the net capped at the share and the crossing
    /// set — share 10, draw 11 — whatever the answer's status.
    #[test]
    fn test_a_precompiles_draws_past_its_share_are_adopted_fail_closed() {
        for drawn in [0_i64, 4, 10, 11, -3] {
            let pools = PoolAmounts::ZERO.with(DATA_SIZE_POOL, 10);
            let mut ledger = opened(PoolAmounts::UNLIMITED);
            ledger.park(0, 0, 0, 0);
            ledger.split(2, Some(1), 1);
            ledger.entries[1].shares = pools;
            ledger.adopt_provider_draws([drawn, 0, 0]);
            let mut answer = GasTracker::new(0, 0, 0);
            answer.set_pool_spent(DATA_SIZE_POOL, drawn);
            answer.adopt_pool_shares(pools);
            assert_eq!(top(&ledger), Pools::on(&answer), "drawn {drawn}");
        }
        let mut ledger = opened(PoolAmounts::UNLIMITED);
        ledger.park(0, 0, 0, 0);
        ledger.split(2, Some(1), 1);
        ledger.entries[1].shares = PoolAmounts::ZERO.with(DATA_SIZE_POOL, 10);
        ledger.adopt_provider_draws([11, 0, 0]);
        let pools = top(&ledger);
        assert_eq!(
            (pools.remaining.get(DATA_SIZE_POOL), pools.net[0], pools.crossings[0]),
            (0, 10, true),
            "share 10, draw 11"
        );
    }

    /// A log's history draws the allowance first and pays the rest, and what the frame returns of
    /// it is compared for a frame revm built; an answer's allowance is never drawn or compared.
    #[test]
    fn test_the_allowance_is_drawn_first_and_compared_for_a_built_frame() {
        for built in [false, true] {
            let mut ledger = opened(PoolAmounts::UNLIMITED);
            ledger.tolerate_divergences();
            ledger.park(KEPT, 0, 0, 100);
            ledger.split(2, Some(1), 1);
            if built {
                ledger.built();
            }
            assert_eq!(ledger.draw_allowance(30), 0, "within it");
            assert_eq!(ledger.draw_allowance(80), 10, "past it");
            assert_eq!(ledger.draw_allowance(5), 5, "spent");
            ledger.returning(Returning::new(0, 1));
            ledger.settle(&settled(2, 1, SettleStatus::Ok, 0), &tracker(PoolAmounts::UNLIMITED, 0));
            let divergences = ledger.take_divergences().unwrap_or_default();
            let expected = built.then_some(LedgerDivergence {
                start: 2,
                depth: 1,
                figure: LedgerFigure::Allowance,
                ledger: 0,
                fork: 1,
            });
            assert_eq!(divergences.first().copied(), expected, "built {built}");
        }
    }

    /// The transaction's start resets the ledger, takes what its first frame's input parked or the
    /// stand-in's unlimited pools, and settles into the transaction's gas, which holds none of its
    /// pools while it runs. The lifecycle's end drops what is open.
    #[test]
    fn test_the_transactions_start_resets_the_ledger() {
        let mut ledger = opened(POOLS);
        ledger.park(KEPT, 0, 0, 0);
        ledger.split(2, Some(1), 1);
        ledger.end(false);
        assert!(ledger.entries.is_empty(), "discarded");

        ledger.split(1, None, 0);
        assert_eq!(top(&ledger).remaining, PoolAmounts::UNLIMITED, "the stand-in's");
        ledger.park(KEPT, 0, 0, 0);
        ledger.split(1, None, 0);
        assert_eq!(ledger.entries.len(), 1, "a transaction's start resets");
        ledger.returning(Returning::new(0, 0));
        let mut transaction = tracker(PoolAmounts::UNLIMITED, 0);
        let mut frame = tracker(transaction.forward_all_to_child().pools, 0);
        handle_reservoir_remaining_gas(InstructionResult::Stop, &mut transaction, &mut frame);
        ledger.settle(&settled(1, 0, SettleStatus::Ok, 0), &transaction);
        ledger.end(true);
    }

    /// A settled lifecycle with a start open in the ledger is a broken pairing.
    #[test]
    #[should_panic(expected = "the lifecycle settled with starts open in the ledger")]
    fn test_a_settled_end_with_a_start_open_panics() {
        opened(POOLS).end(true);
    }

    /// A start that opens under another start than the ledger's top is a broken pairing.
    #[test]
    #[should_panic(expected = "not the ledger's top")]
    fn test_a_start_under_another_than_the_top_panics() {
        let mut ledger = opened(POOLS);
        ledger.split(3, Some(2), 1);
    }

    /// A difference panics, naming the start and every figure that parts.
    #[test]
    #[should_panic(expected = "the start ledger parts from the fork at start 2 (depth 1)")]
    fn test_a_difference_panics() {
        let mut ledger = opened(POOLS);
        ledger.park(KEPT, 1_000, 0, 0);
        ledger.split(2, Some(1), 1);
        ledger.returning(Returning::new(0, 0));
        ledger.settle(&settled(2, 1, SettleStatus::Ok, 0), &tracker(POOLS, 0));
    }

    /// A ledger that tolerates differences records each, in the order of the figures, and takes the
    /// fork's pools for the caller, so the next settlement compares from where the two parted.
    #[test]
    fn test_a_tolerated_difference_is_recorded_and_the_caller_takes_the_forks_pools() {
        let mut ledger = opened(POOLS);
        ledger.tolerate_divergences();
        ledger.park(KEPT, 1_000, 0, 0);
        ledger.split(2, Some(1), 1);
        ledger.returning(Returning::new(5, 0));
        let fork = tracker(POOLS.with(DATA_SIZE_POOL, 1), 0);
        ledger.settle(&settled(2, 1, SettleStatus::Ok, 0), &fork);
        let at =
            |figure, ledger, fork| LedgerDivergence { start: 2, depth: 1, figure, ledger, fork };
        assert_eq!(
            ledger.take_divergences(),
            Some(std::vec![
                at(LedgerFigure::PoolRemaining(DATA_SIZE_POOL), 10_000, 1),
                at(LedgerFigure::KeptReservoir, 20, 0),
                at(LedgerFigure::StartChargeRefund, 0, 5),
            ])
        );
        assert_eq!(top(&ledger), Pools::on(&fork), "the fork's pools taken");
        assert_eq!(ledger.take_divergences(), Some(std::vec![]), "taken once");
    }
}
