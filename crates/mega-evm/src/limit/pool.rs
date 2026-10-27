//! The pools of a frame start's budgets on the start ledger: data size, KV updates and state
//! growth ([`DATA_SIZE_POOL`](super::DATA_SIZE_POOL), [`KV_UPDATE_POOL`](super::KV_UPDATE_POOL),
//! [`STATE_GROWTH_POOL`](super::STATE_GROWTH_POOL)), and an amount for each.

/// One of a frame start's budgets on the start ledger, by index.
///
/// The three are named by constants of the limit module; [`new`](Self::new) refuses an index
/// past them, so such a constant does not compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolId(u8);

impl PoolId {
    /// How many budgets a frame start has.
    pub const COUNT: usize = 3;

    /// Every pool, in index order.
    pub const ALL: [Self; Self::COUNT] = [Self(0), Self(1), Self(2)];

    /// The pool at `index`.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`COUNT`](Self::COUNT); in a constant, that is a compile error.
    #[inline]
    pub const fn new(index: usize) -> Self {
        assert!(index < Self::COUNT, "a frame start has no pool at this index");
        Self(index as u8)
    }

    /// The pool's index, below [`COUNT`](Self::COUNT).
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// An amount for each pool: a frame's budgets, the shares a child starts with, the frame caps, or
/// what a write draws on each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PoolAmounts([u64; PoolId::COUNT]);

impl PoolAmounts {
    /// Nothing in any pool. The default.
    pub const ZERO: Self = Self([0; PoolId::COUNT]);

    /// Every pool unlimited: no budget.
    pub const UNLIMITED: Self = Self([u64::MAX; PoolId::COUNT]);

    /// The amount for `pool`.
    #[inline]
    pub const fn get(&self, pool: PoolId) -> u64 {
        self.0[pool.index()]
    }

    /// Sets the amount for `pool`.
    #[inline]
    pub const fn set(&mut self, pool: PoolId, amount: u64) {
        self.0[pool.index()] = amount;
    }

    /// These amounts, with `amount` for `pool`.
    #[inline]
    #[must_use]
    pub const fn with(mut self, pool: PoolId, amount: u64) -> Self {
        self.set(pool, amount);
        self
    }

    /// Whether every amount is zero.
    #[inline]
    pub const fn is_zero(&self) -> bool {
        let mut index = 0;
        while index < PoolId::COUNT {
            if self.0[index] != 0 {
                return false;
            }
            index += 1;
        }
        true
    }
}
