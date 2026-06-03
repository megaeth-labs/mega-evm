//! Backend-agnostic description of a bench scenario.
//!
//! A [`Workload`] names the accounts to seed and the transactions to run,
//! without referring to any particular revm version or EVM type. Each
//! [`Subject`](crate::common::subject::Subject) translates the same `Workload`
//! into its own stack, so a scenario is written once and compared across every
//! row of a criterion group.

use alloy_primitives::{Address, Bytes, U256};

/// A single account to seed into the database before a workload runs.
///
/// Built fluently: `Account::new(addr).balance(b).code(c).storage(slot, v)`.
/// Omitted fields default to zero balance, no code, and no storage — an
/// `Account` with all defaults still forces the account to exist, which differs
/// from leaving it out of the workload entirely.
#[derive(Clone)]
pub struct Account {
    pub address: Address,
    pub balance: U256,
    pub code: Option<Bytes>,
    pub storage: Vec<(U256, U256)>,
}

impl Account {
    pub fn new(address: Address) -> Self {
        Self { address, balance: U256::ZERO, code: None, storage: Vec::new() }
    }

    pub fn balance(mut self, balance: U256) -> Self {
        self.balance = balance;
        self
    }

    pub fn code(mut self, code: Bytes) -> Self {
        self.code = Some(code);
        self
    }

    pub fn storage(mut self, slot: U256, value: U256) -> Self {
        self.storage.push((slot, value));
        self
    }
}

/// A single `transact()` call, backend-agnostic. Mirrors the minimal set of
/// fields every adapter needs; kept deliberately small so it does not tie the
/// abstraction to one revm version's `TxEnv` (the pinned and latest stacks are
/// separate crates and each translates this into its own envelope).
#[derive(Clone)]
pub struct TxSpec {
    pub caller: Address,
    pub target: Address,
    pub gas_limit: u64,
    pub value: U256,
    pub data: Bytes,
}

impl TxSpec {
    /// A call from `caller` to `target` with a 30M gas limit, no value, no data.
    pub fn call(caller: Address, target: Address) -> Self {
        Self { caller, target, gas_limit: 30_000_000, value: U256::ZERO, data: Bytes::new() }
    }

    pub fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    pub fn value(mut self, value: U256) -> Self {
        self.value = value;
        self
    }

    pub fn data(mut self, data: Bytes) -> Self {
        self.data = data;
        self
    }
}

/// A complete bench scenario: the accounts to seed and the transactions to run.
///
/// Single-shot benches pass one tx via [`Workload::single`]; `transfer_multi`
/// passes the whole batch and a subject loops over it on one reused EVM.
#[derive(Clone)]
pub struct Workload {
    pub accounts: Vec<Account>,
    pub txs: Vec<TxSpec>,
    /// When `true` (the default) every subject asserts each tx returns a
    /// successful result. Set to `false` via [`Workload::allow_halt`] for
    /// scenarios that legitimately halt under some specs (e.g. SELFDESTRUCT
    /// disabled, or DELEGATECALL falling through to absent bytecode).
    pub assert_success: bool,
}

impl Workload {
    pub fn new(accounts: Vec<Account>, txs: Vec<TxSpec>) -> Self {
        Self { accounts, txs, assert_success: true }
    }

    /// A workload running a single transaction.
    pub fn single(accounts: Vec<Account>, tx: TxSpec) -> Self {
        Self::new(accounts, vec![tx])
    }

    /// Opt out of the per-tx `is_success()` assertion. Use for scenarios where
    /// halting is the expected outcome under at least one benchmarked spec.
    pub fn allow_halt(mut self) -> Self {
        self.assert_success = false;
        self
    }
}
