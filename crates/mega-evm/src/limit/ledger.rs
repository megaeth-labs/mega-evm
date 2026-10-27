//! The engine's ledger of what each frame start owes and holds: the frame budgets, the start
//! charge and the history allowance.
//!
//! A frame start carries three figures beside its gas that are the engine's: the shares of its
//! caller's three pools — data size, KV updates, state growth — it may draw, with its net draws;
//! the start charge its failure gives back to its caller, the history of the records its start
//! made on it; and the history allowance a value call gives it. The ledger keeps them in an entry
//! per open start, by the engine's own rules, and they decide: a draw is held to the top entry's
//! budget ([`budget`](StartLedger::budget)), a settlement gives the start charge back
//! ([`settle`](StartLedger::settle)), and a log's history draws the top entry's allowance first
//! ([`draw_allowance`](StartLedger::draw_allowance)). The part of its EIP-8037 reservoir a caller
//! keeps is the engine's rule too ([`reservoir_share`]), taken out of the caller's gas where the
//! start is made; the reservoir itself stays on the gas, which revm settles.
//!
//! It is driven by the start lifecycle revm owns and by nothing else: an entry opens when revm
//! opens the start (`Host::frame_split`), settles when revm settles it (`Host::frame_settle`), and
//! the ledger is dropped when the lifecycle ends, settled or discarded
//! (`Host::frame_lifecycle_end`). The transaction's start resets it, so nothing a context's ledger
//! held carries into the next transaction. The ledger pairs nothing: revm's record says which start
//! settles, and refuses a start or a settlement out of order before the ledger sees it; a debug
//! build asserts the settling start is the one on top.
//!
//! # The rules
//!
//! - **What a start owes is parked where it is made**, on the entry of the frame that makes it, and
//!   the split that opens the start takes it ([`park`](StartLedger::park)): an instruction's start
//!   at the frame-start hook, after the hook's charge; a `keylessDeploy` call's creation where its
//!   builder makes it; the transaction's start where its first frame's input is built
//!   ([`park_transaction`](StartLedger::park_transaction)). A start dropped before revm consumes it
//!   loses what was parked for it, and the frame's next start parks over it. The stand-in revm
//!   settles for a transaction whose runtime gas phase ran out of gas has nothing parked: it starts
//!   with unlimited pools and owes nothing. A start whose input an inspector built itself has
//!   nothing parked either, and opens with no pool and nothing owed.
//! - **A child's share of each pool** is all but `kept` per cent of what its caller has left,
//!   rounded down, taken out of the caller's entry where it is parked; the caller keeps `kept` per
//!   cent of its reservoir, rounded down ([`reservoir_share`]). `kept` is the engine's
//!   ([`frame_kept_percent`](crate::MegaContext)): two on the spec's configuration, none on the
//!   neutral one, which forwards the whole of each as Ethereum does. When the child's frame starts,
//!   its shares are held to the frame cap and, for a caller whose net is negative, to the share of
//!   the caller's budget, and the excess goes back to the caller
//!   ([`cap_child`](StartLedger::cap_child)).
//! - **A draw goes to the top entry**, the running frame's start, held first to its budget: a
//!   completed write, a frame start's records on the started frame, a deposit's code on the
//!   creating frame, a creator's nonce record on the creator once its creation returned
//!   ([`draw`](StartLedger::draw)). With no start open, as for a frame a test drives by hand, there
//!   is no frame budget.
//! - **A precompile's draws are adopted when it answers**, fail-closed: a draw past the share
//!   empties the pool and caps the net at the share, whatever the answer's status
//!   ([`adopt_provider_draws`](StartLedger::adopt_provider_draws)).
//! - **A settlement** merges the child's entry into its caller's ([`settle`](StartLedger::settle)):
//!   on success the child's remaining pools join what the caller kept and its nets the caller's; on
//!   a revert or a halt its draws are undone, and its whole share comes back. The start charge
//!   comes back on a failure, and for a creation also when it took no address. An inspector's
//!   answer, and a result whose gas an inspector replaced, settle as unlimited pools with no net;
//!   neither an inspector's answer nor an outcome an inspector rebuilt gives the start charge back.
//!   The figures an inspector puts on the gas or the input it hands over are not read.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use revm::context_interface::{
    cfg::gas::{PoolAmounts, PoolId},
    host::{FrameSettle, SettleStatus},
};

use super::Counted;

/// What a start owes, decided where it is made and taken by the split that opens it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Parked {
    /// The child's share of each of its caller's pools.
    shares: PoolAmounts,
    /// The history gas the start charged its caller for the records on the child, which the
    /// child's failure gives back.
    charge: u64,
    /// The history gas the child may spend on logs before it charges its gas.
    allowance: u64,
}

impl Parked {
    /// What the transaction's start owes: its pools, `pools`, the start `charge` of the record its
    /// frame makes, and no allowance, its frame making no value call.
    pub(crate) const fn transaction(pools: PoolAmounts, charge: u64) -> Self {
        Self { shares: pools, charge, allowance: 0 }
    }
}

/// What a transaction's start owes when nothing was parked for it: the stand-in revm settles for a
/// transaction whose runtime gas phase ran out of gas, which carries every pool, unlimited, and
/// owes nothing else.
const STAND_IN: Parked = Parked::transaction(PoolAmounts::UNLIMITED, 0);

/// What a start owes when an inspector consumed an input it built itself, which no start parked
/// anything for: nothing, and no pool.
const UNPARKED: Parked = Parked { shares: PoolAmounts::ZERO, charge: 0, allowance: 0 };

/// What a precompile's provider drew on one pool of its answer: its signed `net` draw, and whether
/// it drew past a share it was held to, which the net alone cannot tell once that share capped it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProviderDraw {
    /// The net draw.
    pub(crate) net: i64,
    /// Whether the draw went past a share it was held to.
    pub(crate) past_a_share: bool,
}

/// A frame's three pools as the engine keeps them: what it may still draw, and its signed net draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pools {
    remaining: PoolAmounts,
    net: [i64; PoolId::COUNT],
}

impl Pools {
    /// Pools that start at `shares`, undrawn.
    const fn of(shares: PoolAmounts) -> Self {
        Self { remaining: shares, net: [0; PoolId::COUNT] }
    }

    /// Draws `amount` on `pool`; a draw the pool cannot pay takes nothing.
    fn record(&mut self, pool: PoolId, amount: u64) -> bool {
        let index = pool.index();
        let Some(left) = self.remaining.get(pool).checked_sub(amount) else { return false };
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
    /// undone first.
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

/// The share of its caller's EIP-8037 `reservoir` a frame starts with: all but the caller's
/// `kept` per cent of it, which is rounded down, so a caller that keeps two per cent keeps nothing
/// of a reservoir below 50 and forwards 98 of 99.
pub(crate) const fn reservoir_share(reservoir: u64, kept: u64) -> u64 {
    reservoir - kept_part(reservoir, kept)
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
    /// The start charge its failure gives back.
    charge: u64,
    /// What is left of its history allowance.
    allowance: u64,
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
            charge: owed.charge,
            allowance: owed.allowance,
            parked: None,
        }
    }
}

/// A start's settlement as revm reports it: the fields of [`FrameSettle`], which only revm's record
/// can build, so the ledger's unit tests can settle a start without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settlement {
    id: u32,
    depth: usize,
    status: SettleStatus,
    created: Option<bool>,
    inspector_answer: bool,
    gas_replaced: bool,
    outcome_rebuilt: bool,
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
        }
    }
}

/// A frame's pools as a settlement left them: what it may still draw on each, and its signed net
/// draw on each.
///
/// Public to the test tools, which read the ledger's settlements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(test, feature = "test-utils"))]
pub struct LedgerPools {
    /// What the frame may still draw on each pool.
    pub remaining: PoolAmounts,
    /// The frame's net draw on each pool, by the pool's index.
    pub net: [i64; PoolId::COUNT],
}

#[cfg(any(test, feature = "test-utils"))]
impl From<Pools> for LedgerPools {
    fn from(pools: Pools) -> Self {
        Self { remaining: pools.remaining, net: pools.net }
    }
}

/// A start's settlement as the ledger made it.
///
/// Public to the test tools, which read the ledger's settlements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(test, feature = "test-utils"))]
pub struct LedgerSettlement {
    /// The identity revm gave the start.
    pub start: u32,
    /// The started frame's depth.
    pub depth: usize,
    /// The started frame's pools as they settled into its caller: an inspector's answer and a
    /// result whose gas an inspector replaced settle as unlimited pools.
    pub frame: LedgerPools,
    /// The caller's pools once the start settled into them; for the transaction's start, the
    /// transaction's, which held none of its frame's pools while the frame ran.
    pub caller: LedgerPools,
    /// The history gas the settlement gave back to the caller: the start charge, or nothing.
    pub refund: u64,
}

/// The engine's ledger of the open starts of the running transaction. See the module
/// documentation.
#[derive(Clone, Debug, Default)]
pub(crate) struct StartLedger {
    /// One entry per open start, the transaction's first and the innermost last.
    entries: Vec<Entry>,
    /// What the transaction's start owes, parked where its first frame's input is built.
    transaction: Option<Parked>,
    /// The settlements made, when a test has them recorded.
    #[cfg(any(test, feature = "test-utils"))]
    settlements: Option<Vec<LedgerSettlement>>,
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
    ///
    /// revm opens a child only under the start of the frame on top of its stack, which is the
    /// ledger's top entry, one depth less; a debug build asserts it. A hook called by hand out of
    /// that order opens the start on whatever is on top.
    pub(crate) fn split(&mut self, id: u32, parent: Option<u32>, depth: usize) {
        let owed = match parent {
            None => {
                self.entries.clear();
                self.transaction.take().unwrap_or(STAND_IN)
            }
            Some(parent) => {
                debug_assert!(
                    self.entries
                        .last()
                        .is_some_and(|caller| caller.id == parent && caller.depth + 1 == depth),
                    "start {id} at depth {depth} opened under {parent}, not the ledger's top"
                );
                self.entries.last_mut().and_then(|caller| caller.parked.take()).unwrap_or(UNPARKED)
            }
        };
        self.entries.push(Entry::open(id, depth, owed));
    }

    /// Ends the lifecycle: every start settled when it `settled`, or what is open discarded.
    pub(crate) fn end(&mut self, settled: bool) {
        debug_assert!(
            !settled || self.entries.is_empty(),
            "the lifecycle settled with starts open in the ledger: {:?}",
            self.entries
        );
        self.entries.clear();
        self.transaction = None;
    }

    /* What a start owes */

    /// Parks on the running frame's entry what the start it is making owes: the share of each of
    /// its pools, by `kept`, taken out of them; the start's `charge`; and the child's `allowance`.
    /// Overwrites what an earlier start of the frame parked and no split took.
    pub(crate) fn park(&mut self, kept: u64, charge: u64, allowance: u64) {
        let Some(caller) = self.entries.last_mut() else { return };
        let mut shares = PoolAmounts::ZERO;
        for pool in PoolId::ALL {
            let remaining = caller.pools.remaining.get(pool);
            let share = pool_share(remaining, kept);
            caller.pools.remaining.set(pool, remaining - share);
            shares.set(pool, share);
        }
        caller.parked = Some(Parked { shares, charge, allowance });
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

    /// The start charge of the start on top: the history its caller paid for the records the start
    /// makes on its frame. Nothing with no start open.
    pub(crate) fn charge(&self) -> u64 {
        self.entries.last().map_or(0, |entry| entry.charge)
    }

    /* Draws */

    /// The budget of the running frame on `pool`: what it may still draw, and its signed net draw.
    /// With no start open there is no frame budget: the pool is unlimited, undrawn.
    pub(crate) fn budget(&self, pool: PoolId) -> (u64, i64) {
        self.entries.last().map_or((u64::MAX, 0), |entry| {
            (entry.pools.remaining.get(pool), entry.pools.net[pool.index()])
        })
    }

    /// Draws `counted` on the top entry's pools, which the limits admitted: what it takes back goes
    /// back to each pool first, then what it adds is drawn. A pool it leaves alone is not visited.
    ///
    /// The limits admit a draw past the pool only for an exempt transaction, which no budget holds:
    /// past a saturated unlimited pool, or on a start an inspector built itself, which opens with
    /// none. Such a draw takes nothing.
    pub(crate) fn draw(&mut self, counted: &Counted) {
        let Some(entry) = self.entries.last_mut() else { return };
        for pool in PoolId::ALL {
            let (added, taken) = (counted.added.get(pool), counted.taken.get(pool));
            if added == 0 && taken == 0 {
                continue;
            }
            entry.pools.refill(pool, taken);
            entry.pools.record(pool, added);
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
    /// top, fail-closed: a draw within the share is drawn, and one past it empties the pool and
    /// caps the net at the share. The answer's status is the provider's either way.
    ///
    /// The adoption replaces the entry's pools with the share less the provider's draw, so it runs
    /// before anything else is drawn on the answer: the start's own records, which the frame start
    /// counts on the answer once it was adopted, would otherwise be overwritten.
    pub(crate) fn adopt_provider_draws(&mut self, draws: [ProviderDraw; PoolId::COUNT]) {
        let Some(entry) = self.entries.last_mut() else { return };
        for pool in PoolId::ALL {
            let (index, share) = (pool.index(), entry.shares.get(pool));
            let ProviderDraw { net: drawn, past_a_share } = draws[index];
            if past_a_share || (drawn > 0 && drawn as u64 > share) {
                entry.pools.remaining.set(pool, 0);
                entry.pools.net[index] = signed(share);
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

    /// Settles the start on top, which revm just settled into its caller's gas — the transaction's
    /// gas, for the transaction's start — into its caller's entry, and answers the history gas the
    /// caller gets back for it: the start charge, when the start did not keep its records.
    ///
    /// revm settles only the start on top of its record, the ledger's top entry; a debug build
    /// asserts it. A hook called by hand out of that order settles whatever is on top, and with
    /// nothing open settles nothing.
    pub(crate) fn settle(&mut self, settlement: &Settlement) -> u64 {
        debug_assert!(
            self.entries
                .last()
                .is_some_and(|child| (child.id, child.depth) == (settlement.id, settlement.depth)),
            "the start on top of the ledger is not the one revm settled: {settlement:?}"
        );
        let Some(child) = self.entries.pop() else { return 0 };
        let inspected = settlement.inspector_answer || settlement.gas_replaced;
        let pools = if inspected { Pools::of(PoolAmounts::UNLIMITED) } else { child.pools };
        let succeeded = settlement.status == SettleStatus::Ok;
        let failed = !succeeded || settlement.created == Some(false);
        let refunds = failed && !settlement.inspector_answer && !settlement.outcome_rebuilt;
        let refund = if refunds { child.charge } else { 0 };

        // The transaction's gas holds none of its pools while its frame runs, which took them
        // whole.
        let mut transaction = Pools::of(PoolAmounts::ZERO);
        let caller = match self.entries.last_mut() {
            Some(caller) => &mut caller.pools,
            None => &mut transaction,
        };
        caller.absorb(pools, succeeded);
        #[cfg(any(test, feature = "test-utils"))]
        if let Some(settlements) = &mut self.settlements {
            settlements.push(LedgerSettlement {
                start: child.id,
                depth: child.depth,
                frame: pools.into(),
                caller: (*caller).into(),
                refund,
            });
        }
        refund
    }

    /* The settlements a test reads */

    /// Records every settlement from now on.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn record_settlements(&mut self) {
        self.settlements.get_or_insert_with(Vec::new);
    }

    /// The settlements recorded since they were last taken, in the order they were made; `None`
    /// unless they are recorded.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn take_settlements(&mut self) -> Option<Vec<LedgerSettlement>> {
        self.settlements.as_mut().map(core::mem::take)
    }

    /// Each pool of the start on top: what it may still draw and its net.
    #[cfg(test)]
    pub(crate) fn top_pools(&self) -> [(u64, i64); PoolId::COUNT] {
        let pools = self.entries.last().expect("a start is open").pools;
        PoolId::ALL.map(|pool| (pools.remaining.get(pool), pools.net[pool.index()]))
    }

    /// The shares the start on top started with and the pools of the start below it.
    #[cfg(test)]
    pub(crate) fn top_shares_and_caller_pools(&self) -> (PoolAmounts, PoolAmounts) {
        let [.., caller, child] = self.entries.as_slice() else { panic!("two starts are open") };
        (child.shares, caller.pools.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit::{DATA_SIZE_POOL, KV_UPDATE_POOL, STATE_GROWTH_POOL};

    /// The part of its pools and reservoir the spec's configuration has a caller keep.
    const KEPT: u64 = crate::evm::FRAME_KEPT_PERCENT;

    /// A settlement of start `id` at `depth` with `status`, which no inspector touched, of a call.
    fn settled(id: u32, depth: usize, status: SettleStatus) -> Settlement {
        Settlement {
            id,
            depth,
            status,
            created: None,
            inspector_answer: false,
            gas_replaced: false,
            outcome_rebuilt: false,
        }
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

    /// `amount × numerator / 100`, rounded down, computed apart from the ledger's arithmetic.
    fn per_cent(amount: u64, numerator: u64) -> u64 {
        u64::try_from(u128::from(amount) * u128::from(numerator) / 100).unwrap()
    }

    /// A child's share of each pool is the floor of 98 per cent of what its caller has left, the
    /// caller keeping the rest; a child's share of the reservoir is what the caller has less the
    /// floor of its 2 per cent, so a caller keeps nothing of a reservoir below 50 and forwards 98
    /// of
    /// 99. On the neutral configuration the caller keeps nothing of either.
    #[test]
    fn test_a_start_takes_its_shares_and_leaves_the_caller_its_part() {
        for (kept, forwarded) in [(KEPT, 98), (0, 100)] {
            for reservoir in [0, 1, 49, 50, 99, 100, 149, 12_345, u64::MAX] {
                let share = reservoir_share(reservoir, kept);
                assert_eq!(share, reservoir - per_cent(reservoir, 100 - forwarded), "{reservoir}");
            }
            for pools in [POOLS, PoolAmounts::UNLIMITED, PoolAmounts::ZERO] {
                let case = format!("kept {kept}, pools {pools:?}");
                let mut ledger = opened(pools);
                ledger.park(kept, 7, 9);
                let parked = ledger.entries[0].parked.expect("parked");
                for pool in PoolId::ALL {
                    let share = per_cent(pools.get(pool), forwarded);
                    assert_eq!(parked.shares.get(pool), share, "{case}: {pool:?}");
                    let left = top(&ledger).remaining.get(pool);
                    assert_eq!(left, pools.get(pool) - share, "{case}: {pool:?}");
                }
                assert_eq!((parked.charge, parked.allowance), (7, 9), "{case}");
            }
        }
        assert_eq!(reservoir_share(99, KEPT), 98);
        assert_eq!(reservoir_share(49, KEPT), 49);
        assert_eq!(per_cent(99, 98), 97, "a pool's share of 99 is rounded down");
    }

    /// What a start owes is the frame's own scratch: the split that opens it takes it, a start
    /// dropped before revm consumed it loses its shares — the caller keeps only its part — and the
    /// frame's next start parks over it.
    #[test]
    fn test_a_dropped_start_loses_its_shares_and_the_next_start_parks_over_it() {
        let mut ledger = opened(POOLS);
        ledger.park(KEPT, 1, 0);
        let after_first = top(&ledger).remaining;
        ledger.park(KEPT, 2, 0);
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
        assert_eq!(ledger.charge(), 2, "the start on top's");
    }

    /// A child's draws settle into its caller: a success hands its remaining pools and its nets to
    /// the caller, a revert and a halt give its whole share back and leave the caller's nets alone;
    /// a child's take-back of a caller's draw nets out on success.
    #[test]
    fn test_a_settlement_merges_a_success_and_gives_a_failures_share_back() {
        for status in [SettleStatus::Ok, SettleStatus::Revert, SettleStatus::Halt] {
            let case = format!("{status:?}");
            let mut ledger = opened(POOLS);
            // The caller draws, then starts a child, which draws and takes back.
            let caller_draw = Counted::records(3, 0, false).with_growth(2, 0);
            ledger.draw(&caller_draw);
            ledger.park(KEPT, 0, 0);
            let kept = top(&ledger);
            ledger.split(2, Some(1), 1);
            let shares = ledger.entries[1].shares;
            let child_draw = Counted::records(5, 2, true).with_growth(1, 2);
            ledger.draw(&child_draw);
            let child_net = PoolId::ALL.map(|pool| {
                let (added, taken) = (child_draw.added.get(pool), child_draw.taken.get(pool));
                i64::try_from(added).unwrap() - i64::try_from(taken).unwrap()
            });
            assert_eq!(top(&ledger).net, child_net, "{case}: the child's draws");
            assert_eq!(ledger.settle(&settled(2, 1, status)), 0, "{case}: no charge");
            let caller = top(&ledger);
            for pool in PoolId::ALL {
                let index = pool.index();
                let (kept, share, net) =
                    (kept.remaining.get(pool), shares.get(pool), child_net[index]);
                let caller_net = i64::try_from(caller_draw.added.get(pool)).unwrap();
                let (remaining, net) = if status == SettleStatus::Ok {
                    (kept + share.saturating_add_signed(-net), caller_net + net)
                } else {
                    (kept + share, caller_net)
                };
                assert_eq!(
                    (caller.remaining.get(pool), caller.net[index]),
                    (remaining, net),
                    "{case}: {pool:?}"
                );
            }
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
            ledger.park(KEPT, 123, 0);
            ledger.split(2, Some(1), 1);
            let settlement =
                Settlement { created, inspector_answer, outcome_rebuilt, ..settled(2, 1, status) };
            assert_eq!(ledger.settle(&settlement), if refunded { 123 } else { 0 }, "{case}");
        }
        // The transaction's start gives its own charge back the same way.
        let mut ledger = StartLedger::default();
        ledger.park_transaction(Some(Parked::transaction(POOLS, 45)));
        ledger.split(1, None, 0);
        assert_eq!(ledger.settle(&settled(1, 0, SettleStatus::Halt)), 45);
    }

    /// An inspector's answer and a result whose tracker an inspector replaced settle as unlimited
    /// pools with no net, whatever the frame drew and whatever the status: the caller is left with
    /// unlimited pools. An outcome an inspector rebuilt around the frame's own tracker settles on
    /// the frame's pools.
    #[test]
    fn test_an_inspectors_answer_and_a_replaced_tracker_settle_as_unlimited_pools() {
        for (inspector_answer, gas_replaced, outcome_rebuilt) in
            [(true, false, false), (false, true, false), (false, false, true)]
        {
            for status in [SettleStatus::Ok, SettleStatus::Revert] {
                let case =
                    format!("{inspector_answer}, {gas_replaced}, {outcome_rebuilt}, {status:?}");
                let mut ledger = opened(POOLS);
                ledger.record_settlements();
                ledger.park(KEPT, 0, 0);
                let kept = top(&ledger);
                ledger.split(2, Some(1), 1);
                let shares = ledger.entries[1].shares;
                ledger.draw(&Counted::records(1, 0, false));
                let settlement = Settlement {
                    inspector_answer,
                    gas_replaced,
                    outcome_rebuilt,
                    ..settled(2, 1, status)
                };
                ledger.settle(&settlement);
                let settled = ledger.take_settlements().expect("recorded")[0];
                let caller = top(&ledger);
                assert_eq!(settled.caller, caller.into(), "{case}: the record");
                if outcome_rebuilt {
                    let back = if status == SettleStatus::Ok { 40 } else { 0 };
                    let data =
                        kept.remaining.get(DATA_SIZE_POOL) + shares.get(DATA_SIZE_POOL) - back;
                    assert_eq!(caller.remaining.get(DATA_SIZE_POOL), data, "{case}: the frame's");
                } else {
                    assert_eq!(settled.frame.remaining, PoolAmounts::UNLIMITED, "{case}");
                    assert_eq!(settled.frame.net, [0; PoolId::COUNT], "{case}");
                    assert_eq!(caller.remaining, PoolAmounts::UNLIMITED, "{case}: unlimited");
                    assert_eq!(caller.net, [0; PoolId::COUNT], "{case}: no net");
                }
            }
        }
    }

    /// A precompile's draws are adopted fail-closed: within the share they are drawn, and past it
    /// the pool is emptied and the net capped at the share — share 10, draw 11 — whatever the
    /// answer's status. A draw marked past a share is adopted as one past the entry's, whatever its
    /// net: an unlimited share included, which no net can pass.
    #[test]
    fn test_a_precompiles_draws_past_its_share_are_adopted_fail_closed() {
        let none = ProviderDraw::default();
        let adopted = |share: u64, net: i64, past_a_share: bool| {
            let mut ledger = opened(PoolAmounts::UNLIMITED);
            ledger.park(0, 0, 0);
            ledger.split(2, Some(1), 1);
            ledger.entries[1].shares = PoolAmounts::UNLIMITED.with(DATA_SIZE_POOL, share);
            ledger.adopt_provider_draws([ProviderDraw { net, past_a_share }, none, none]);
            ledger.top_pools()[DATA_SIZE_POOL.index()]
        };
        for (drawn, expected) in
            [(0, (10, 0)), (4, (6, 4)), (10, (0, 10)), (11, (0, 10)), (-3, (13, -3))]
        {
            assert_eq!(adopted(10, drawn, false), expected, "share 10, drawn {drawn}");
        }
        assert_eq!(adopted(10, 10, true), (0, 10), "a draw a share of 10 capped at it");
        let unlimited = u64::MAX;
        assert_eq!(adopted(unlimited, 7, false), (unlimited - 7, 7), "an unlimited share");
        assert_eq!(adopted(unlimited, i64::MAX, true), (0, i64::MAX), "marked past a share");
    }

    /// A log's history draws the allowance first and pays the rest; with no start open it pays all.
    #[test]
    fn test_the_allowance_is_drawn_first() {
        let mut ledger = opened(PoolAmounts::UNLIMITED);
        ledger.park(KEPT, 0, 100);
        ledger.split(2, Some(1), 1);
        assert_eq!(ledger.draw_allowance(30), 0, "within it");
        assert_eq!(ledger.draw_allowance(80), 10, "past it");
        assert_eq!(ledger.draw_allowance(5), 5, "spent");
        assert_eq!(StartLedger::default().draw_allowance(5), 5, "no start open");
    }

    /// A start an inspector built itself, which nothing parked for, opens with no budget; a draw
    /// the limits admitted on it anyway — an exempt transaction's, which no budget holds — takes
    /// nothing and is no broken invariant.
    #[test]
    fn test_a_draw_on_a_start_nothing_was_parked_for_takes_nothing() {
        let mut ledger = opened(PoolAmounts::UNLIMITED);
        ledger.split(2, Some(1), 1);
        assert_eq!(ledger.budget(DATA_SIZE_POOL), (0, 0), "no budget");
        ledger.draw(&Counted::records(1, 0, false).with_growth(1, 0));
        assert_eq!(ledger.top_pools(), [(0, 0); PoolId::COUNT], "nothing taken");
        assert_eq!(ledger.settle(&settled(2, 1, SettleStatus::Ok)), 0);
        assert_eq!(
            top(&ledger).remaining,
            PoolAmounts::UNLIMITED,
            "the caller's pools as they were"
        );
    }

    /// The running frame's budget is its entry's pools; with no start open there is none, and a
    /// draw draws nothing.
    #[test]
    fn test_the_budget_is_the_top_entrys() {
        let mut ledger = opened(POOLS);
        ledger.draw(&Counted::records(1, 0, false));
        assert_eq!(ledger.budget(DATA_SIZE_POOL), (10_000 - 40, 40));
        assert_eq!(ledger.budget(KV_UPDATE_POOL), (249, 1));
        assert_eq!(ledger.budget(STATE_GROWTH_POOL), (49, 0));
        let mut none = StartLedger::default();
        none.draw(&Counted::records(1, 0, false));
        assert_eq!(none.budget(DATA_SIZE_POOL), (u64::MAX, 0), "no frame budget");
    }

    /// The transaction's start resets the ledger and takes what its first frame's input parked or
    /// the stand-in's unlimited pools; a settlement with nothing open settles nothing. The
    /// lifecycle's end drops what is open.
    #[test]
    fn test_the_transactions_start_resets_the_ledger() {
        let mut ledger = opened(POOLS);
        ledger.park(KEPT, 0, 0);
        ledger.split(2, Some(1), 1);
        ledger.end(false);
        assert!(ledger.entries.is_empty(), "discarded");

        ledger.split(1, None, 0);
        assert_eq!(top(&ledger).remaining, PoolAmounts::UNLIMITED, "the stand-in's");
        ledger.park(KEPT, 0, 0);
        ledger.split(1, None, 0);
        assert_eq!(ledger.entries.len(), 1, "a transaction's start resets");
        assert_eq!(ledger.settle(&settled(1, 0, SettleStatus::Ok)), 0);
        ledger.end(true);
    }

    /// A settled lifecycle with a start open in the ledger is a broken pairing, which a debug build
    /// asserts.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "the lifecycle settled with starts open in the ledger")]
    fn test_a_settled_end_with_a_start_open_panics() {
        opened(POOLS).end(true);
    }

    /// A start that opens under another start than the ledger's top is a broken pairing, which a
    /// debug build asserts.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "not the ledger's top")]
    fn test_a_start_under_another_than_the_top_panics() {
        let mut ledger = opened(POOLS);
        ledger.split(3, Some(2), 1);
    }

    /// A settlement of another start than the ledger's top is a broken pairing, which a debug build
    /// asserts.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "is not the one revm settled")]
    fn test_a_settlement_of_another_start_than_the_top_panics() {
        let mut ledger = opened(POOLS);
        ledger.settle(&settled(2, 1, SettleStatus::Ok));
    }

    /// A release build takes a broken pairing, which only a hook called by hand makes, without a
    /// panic: a start opens on whatever is on top, a settlement settles it, one with nothing open
    /// settles nothing, and a settled end with a start open drops it.
    #[test]
    #[cfg(not(debug_assertions))]
    fn test_a_broken_pairing_does_not_panic_a_release_build() {
        let mut ledger = opened(POOLS);
        ledger.split(3, Some(2), 5);
        assert_eq!(ledger.settle(&settled(9, 9, SettleStatus::Revert)), 0);
        ledger.end(true);
        assert!(ledger.entries.is_empty());
        assert_eq!(ledger.settle(&settled(1, 0, SettleStatus::Ok)), 0, "nothing open");
        ledger.split(4, Some(1), 1);
        assert_eq!(ledger.entries.len(), 1, "a child with nothing under it opens unparked");
    }
}
