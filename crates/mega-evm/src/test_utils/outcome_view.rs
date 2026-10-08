//! A serializable view of a transaction's outcome, for snapshot tests.
//!
//! [`MegaTransactionOutcome`] holds revm's result and state beside what `MegaETH` counts, and not
//! every part of it serializes. [`OutcomeView`] copies what a reviewer of a snapshot needs to
//! see — the result, every gas figure, the usage counted, the limit stop, the oracle reads and the
//! touched accounts — into plain fields that serialize the same way on every run: maps are
//! [`BTreeMap`]s and lists are in a fixed order.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{collections::BTreeMap, format, string::String, vec::Vec};

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::{GenericContractError, SolInterface};
use revm::{
    context::result::{ExecutionResult, ResultGas},
    state::Account,
};
use serde::Serialize;

use crate::{
    decode_mega_limit_exceeded, LimitCheck, LimitUsage, MegaGasUsage, MegaTransactionOutcome,
};

/// What one transaction produced, as a snapshot shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutcomeView {
    /// The result: its kind, reason, output and logs.
    pub result: ResultView,
    /// The gas by ledger ([`MegaGasUsage`]).
    pub gas: LedgerView,
    /// The gas figures revm's result carries ([`ResultGas`]).
    pub result_gas: ResultGasView,
    /// The data-size bytes and write records the transaction kept.
    pub usage: UsageView,
    /// The transaction-level limit that stopped the transaction, if one did.
    pub limit_stop: Option<LimitStopView>,
    /// The reads of the Oracle's storage through the oracle service, in order.
    pub oracle_reads: Vec<OracleReadView>,
    /// The accounts the transaction touched, by address.
    pub accounts: BTreeMap<Address, AccountView>,
}

impl OutcomeView {
    /// The view of `outcome`.
    pub fn new(outcome: &MegaTransactionOutcome) -> Self {
        Self {
            result: ResultView::new(&outcome.result),
            gas: LedgerView::new(&outcome.gas),
            result_gas: ResultGasView::new(outcome.result.gas()),
            usage: UsageView::new(&outcome.usage),
            limit_stop: outcome.limit_exceeded.as_ref().and_then(LimitStopView::new),
            oracle_reads: outcome
                .oracle_reads
                .iter()
                .map(|read| OracleReadView { slot: read.slot, answer: read.answer })
                .collect(),
            accounts: outcome
                .state
                .iter()
                .filter(|(_, account)| account.is_touched())
                .map(|(address, account)| (*address, AccountView::new(account)))
                .collect(),
        }
    }
}

impl From<&MegaTransactionOutcome> for OutcomeView {
    fn from(outcome: &MegaTransactionOutcome) -> Self {
        Self::new(outcome)
    }
}

impl OutcomeView {
    /// The outcome in one line ([`OutcomeSummary`]).
    pub fn summary(&self) -> OutcomeSummary {
        OutcomeSummary {
            kind: self.result.kind,
            reason: self.result.reason.clone(),
            limit_stop: self.limit_stop.clone(),
            gas: self.gas,
            usage: self.usage,
            logs: self.result.logs.len(),
            accounts: self.accounts.len(),
        }
    }
}

/// A transaction's outcome in one line, for a test that snapshots more outcomes than a reviewer
/// can read whole: how it ended — its kind, its reason and the limit that stopped it — every
/// figure of [`MegaGasUsage`], the counts of [`LimitUsage`], and how many logs and touched
/// accounts it has. The logs and the accounts themselves are the full view's ([`OutcomeView`]).
///
/// It serializes as that line, so a snapshot of many cases holds one line a case, and a change to
/// a case shows as a change to its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeSummary {
    /// How the transaction ended.
    pub kind: ResultKind,
    /// Why, as [`ResultView::reason`] has it.
    pub reason: Option<String>,
    /// The transaction-level limit that stopped the transaction, if one did.
    pub limit_stop: Option<LimitStopView>,
    /// The gas by ledger.
    pub gas: LedgerView,
    /// The data-size bytes and write records the transaction kept.
    pub usage: UsageView,
    /// How many logs the result carries.
    pub logs: usize,
    /// How many accounts the transaction touched.
    pub accounts: usize,
}

impl core::fmt::Display for OutcomeSummary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self { kind, reason, limit_stop, gas, usage, logs, accounts } = self;
        let kind = match kind {
            ResultKind::Success => "success",
            ResultKind::Revert => "revert",
            ResultKind::Halt => "halt",
        };
        write!(f, "{kind} {}; stop ", reason.as_deref().unwrap_or("-"))?;
        match limit_stop {
            Some(LimitStopView { kind, limit, used }) => {
                write!(f, "{kind} limit {limit} used {used}")?;
            }
            None => f.write_str("-")?,
        }
        let LedgerView {
            regular,
            state,
            history,
            history_bytes,
            reservoir_remaining,
            floor,
            gas_used,
        } = gas;
        let UsageView { data_size, write_records } = usage;
        write!(
            f,
            "; regular {regular} state {state} history {history} history_bytes {history_bytes} \
             reservoir_remaining {reservoir_remaining} floor {floor} gas_used {gas_used}; \
             data_size {data_size} write_records {write_records}; logs {logs} accounts {accounts}"
        )
    }
}

impl Serialize for OutcomeSummary {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// How a transaction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    /// It succeeded.
    Success,
    /// It reverted.
    Revert,
    /// It halted.
    Halt,
}

/// The result of a transaction, without its gas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResultView {
    /// How the transaction ended.
    pub kind: ResultKind,
    /// Why: the success reason (`Stop`, `Return`, `SelfDestruct`) or the halt reason, as their
    /// `Debug` prints them; for a revert, its output decoded as `MegaLimitExceeded` or as a
    /// Solidity `Error(string)` or `Panic(uint256)`, and `None` when it decodes as neither.
    pub reason: Option<String>,
    /// The output: the returned or deployed bytes, or the revert data. `None` for a halt.
    pub output: Option<Bytes>,
    /// The address a creation deployed to, if it deployed.
    pub created: Option<Address>,
    /// The logs, in the order they were emitted.
    pub logs: Vec<LogView>,
}

impl ResultView {
    fn new<H: core::fmt::Debug>(result: &ExecutionResult<H>) -> Self {
        let (kind, reason, output, created) = match result {
            ExecutionResult::Success { reason, output, .. } => (
                ResultKind::Success,
                Some(format!("{reason:?}")),
                Some(output.data().clone()),
                output.address().copied(),
            ),
            ExecutionResult::Revert { output, .. } => {
                (ResultKind::Revert, revert_reason(output), Some(output.clone()), None)
            }
            ExecutionResult::Halt { reason, .. } => {
                (ResultKind::Halt, Some(format!("{reason:?}")), None, None)
            }
        };
        let logs = result
            .logs()
            .iter()
            .map(|log| LogView {
                address: log.address,
                topics: log.topics().to_vec(),
                data: log.data.data.clone(),
            })
            .collect();
        Self { kind, reason, output, created, logs }
    }
}

/// The reason a revert's `output` names: a limit stop's `MegaLimitExceeded`, or a Solidity
/// `Error(string)` or `Panic(uint256)`.
fn revert_reason(output: &Bytes) -> Option<String> {
    if let Some((kind, limit)) = decode_mega_limit_exceeded(output) {
        return Some(format!("MegaLimitExceeded({kind:?}, {limit})"));
    }
    match GenericContractError::abi_decode(output) {
        Ok(error @ (GenericContractError::Revert(_) | GenericContractError::Panic(_))) => {
            Some(format!("{error}"))
        }
        _ => None,
    }
}

/// One log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogView {
    /// The account that emitted it.
    pub address: Address,
    /// Its topics, in order.
    pub topics: Vec<B256>,
    /// Its data.
    pub data: Bytes,
}

/// Every figure of [`MegaGasUsage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LedgerView {
    /// Regular gas spent, before the refund.
    pub regular: u64,
    /// State gas spent.
    pub state: u64,
    /// History gas spent.
    pub history: u64,
    /// The history bytes the transaction appended.
    pub history_bytes: u64,
    /// The reservoir left unspent.
    pub reservoir_remaining: u64,
    /// The EIP-7623 floor.
    pub floor: u64,
    /// The gas used the receipt reports.
    pub gas_used: u64,
}

impl LedgerView {
    const fn new(gas: &MegaGasUsage) -> Self {
        let MegaGasUsage {
            regular,
            state,
            history,
            history_bytes,
            reservoir_remaining,
            floor,
            gas_used,
        } = *gas;
        Self { regular, state, history, history_bytes, reservoir_remaining, floor, gas_used }
    }
}

/// The figures [`ResultGas`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ResultGasView {
    /// The total spent before the refund: regular, state and history gas.
    pub total_gas_spent: u64,
    /// The state gas spent.
    pub state_gas_spent: u64,
    /// The refund, before the EIP-7623 floor.
    pub refunded: u64,
    /// The EIP-7623 floor.
    pub floor_gas: u64,
    /// The reservoir left unspent.
    pub reservoir_remaining: u64,
}

impl ResultGasView {
    const fn new(gas: &ResultGas) -> Self {
        Self {
            total_gas_spent: gas.total_gas_spent(),
            state_gas_spent: gas.state_gas_spent_final(),
            refunded: gas.inner_refunded(),
            floor_gas: gas.floor_gas(),
            reservoir_remaining: gas.reservoir_remaining(),
        }
    }
}

/// The counts of [`LimitUsage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct UsageView {
    /// Data-size bytes.
    pub data_size: u64,
    /// Account and storage write records: the KV count.
    pub write_records: u64,
}

impl UsageView {
    const fn new(usage: &LimitUsage) -> Self {
        let LimitUsage { data_size, write_records } = *usage;
        Self { data_size, write_records }
    }
}

/// A transaction-level limit that stopped the transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LimitStopView {
    /// The dimension crossed, as [`LimitKind`](crate::LimitKind)'s `Debug` prints it.
    pub kind: String,
    /// The limit crossed.
    pub limit: u64,
    /// The usage that crossed it.
    pub used: u64,
}

impl LimitStopView {
    fn new(check: &LimitCheck) -> Option<Self> {
        match *check {
            LimitCheck::ExceedsLimit { kind, limit, used, .. } => {
                Some(Self { kind: format!("{kind:?}"), limit, used })
            }
            LimitCheck::WithinLimit => None,
        }
    }
}

/// One read of the Oracle's storage through the oracle service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OracleReadView {
    /// The slot read.
    pub slot: U256,
    /// The service's answer, if it had one.
    pub answer: Option<U256>,
}

/// A touched account as the transaction left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccountView {
    /// Its balance.
    pub balance: U256,
    /// Its nonce.
    pub nonce: u64,
    /// The hash of its code.
    pub code_hash: B256,
    /// Whether the transaction created it.
    pub created: bool,
    /// Whether the transaction destroyed it.
    pub selfdestructed: bool,
    /// The slots whose value changed, by slot, ascending.
    pub storage: Vec<SlotView>,
}

impl AccountView {
    fn new(account: &Account) -> Self {
        let mut storage: Vec<SlotView> = account
            .changed_storage_slots()
            .map(|(slot, value)| SlotView {
                slot: *slot,
                original: value.original_value,
                present: value.present_value,
            })
            .collect();
        storage.sort_unstable_by_key(|slot| slot.slot);
        Self {
            balance: account.info.balance,
            nonce: account.info.nonce,
            code_hash: account.info.code_hash,
            created: account.is_created(),
            selfdestructed: account.is_selfdestructed(),
            storage,
        }
    }
}

/// A storage slot whose value changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SlotView {
    /// The slot.
    pub slot: U256,
    /// Its value before the transaction.
    pub original: U256,
    /// Its value after the transaction.
    pub present: U256,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LimitKind, MegaHaltReason, MegaLimitExceeded, OracleRead};
    use alloy_primitives::{address, Log, LogData};
    use alloy_sol_types::{Revert, SolError};
    use revm::{
        context::result::{HaltReason, OutOfGasError, Output, ResultAndState, SuccessReason},
        state::{EvmState, EvmStorageSlot, TransactionId},
    };

    const SENDER: Address = address!("0x00000000000000000000000000000000000000c1");
    const CONTRACT: Address = address!("0x00000000000000000000000000000000000000c2");
    /// An account the transaction loaded and did not touch.
    const LOADED: Address = address!("0x00000000000000000000000000000000000000c3");

    /// How many more touched accounts the state carries, so the order a map hands them out in
    /// would show if the view followed it.
    const FILLER: u8 = 16;

    fn filler(i: u8) -> Address {
        let mut address = Address::repeat_byte(0xe0);
        address.0[19] = i;
        address
    }

    fn slot(original: u64, present: u64) -> EvmStorageSlot {
        let mut slot = EvmStorageSlot::new(U256::from(original), TransactionId::ZERO);
        slot.present_value = U256::from(present);
        slot
    }

    /// The accounts of [`outcome`]'s state: the sender, the contract with its slots, an account
    /// only loaded, and [`FILLER`] more touched ones.
    fn accounts() -> Vec<(Address, Account)> {
        let mut sender = Account::default();
        sender.info.balance = U256::from(1_000);
        sender.info.nonce = 1;
        sender.mark_touch();

        let mut contract = Account::default();
        contract.info.code_hash = B256::repeat_byte(0xcc);
        contract.mark_touch();
        // Two changed slots, one read and left alone.
        contract.storage.insert(U256::from(10), slot(5, 0));
        contract.storage.insert(U256::from(2), slot(0, 7));
        contract.storage.insert(U256::from(3), slot(4, 4));
        for i in 0..FILLER {
            contract.storage.insert(U256::from(100 + u64::from(i)), slot(0, u64::from(i) + 1));
        }

        let mut accounts =
            Vec::from([(SENDER, sender), (CONTRACT, contract), (LOADED, Account::default())]);
        for i in 0..FILLER {
            let mut account = Account::default();
            account.info.balance = U256::from(i);
            account.mark_touch();
            accounts.push((filler(i), account));
        }
        accounts
    }

    fn result_gas() -> ResultGas {
        ResultGas::default()
            .with_total_gas_spent(90_000)
            .with_state_gas_spent(30_000)
            .with_refunded(4_000)
            .with_floor_gas(21_000)
            .with_reservoir_remaining(5_000)
    }

    fn log() -> Log {
        Log {
            address: CONTRACT,
            data: LogData::new_unchecked(
                Vec::from([B256::repeat_byte(1), B256::repeat_byte(2)]),
                Bytes::from_static(&[0xab, 0xcd]),
            ),
        }
    }

    /// An outcome with every part of the view filled in: a successful call with a log, and a
    /// limit stop beside it, which a real transaction would not report with a success.
    fn outcome_with(accounts: Vec<(Address, Account)>) -> MegaTransactionOutcome {
        let result = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: result_gas(),
            logs: Vec::from([log()]),
            output: Output::Call(Bytes::from_static(&[0x01])),
        };
        MegaTransactionOutcome {
            result_and_state: ResultAndState::new(result, EvmState::from_iter(accounts)),
            gas: MegaGasUsage {
                regular: 50_000,
                state: 30_000,
                history: 10_000,
                history_bytes: 125,
                reservoir_remaining: 5_000,
                floor: 21_000,
                gas_used: 86_000,
            },
            usage: LimitUsage { data_size: 300, write_records: 3 },
            limit_exceeded: Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: 200,
                used: 300,
                frame_local: false,
            }),
            oracle_reads: Vec::from([
                OracleRead { slot: U256::from(1), answer: Some(U256::from(11)) },
                OracleRead { slot: U256::from(2), answer: None },
            ]),
        }
    }

    fn outcome() -> MegaTransactionOutcome {
        outcome_with(accounts())
    }

    fn json(outcome: &MegaTransactionOutcome) -> String {
        serde_json::to_string_pretty(&OutcomeView::new(outcome)).unwrap()
    }

    fn result_gas_mut(outcome: &mut MegaTransactionOutcome) -> &mut ResultGas {
        match &mut outcome.result {
            ExecutionResult::Success { gas, .. } |
            ExecutionResult::Revert { gas, .. } |
            ExecutionResult::Halt { gas, .. } => gas,
        }
    }

    fn logs_mut(outcome: &mut MegaTransactionOutcome) -> &mut Vec<Log> {
        match &mut outcome.result {
            ExecutionResult::Success { logs, .. } |
            ExecutionResult::Revert { logs, .. } |
            ExecutionResult::Halt { logs, .. } => logs,
        }
    }

    fn account_mut(outcome: &mut MegaTransactionOutcome, address: Address) -> &mut Account {
        outcome.state.get_mut(&address).unwrap()
    }

    fn halt(reason: HaltReason) -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Halt {
            reason: MegaHaltReason::Base(reason),
            gas: result_gas(),
            logs: Vec::from([log()]),
        }
    }

    fn revert(output: Bytes) -> ExecutionResult<MegaHaltReason> {
        ExecutionResult::Revert { gas: result_gas(), logs: Vec::from([log()]), output }
    }

    /// The view serializes the same whatever order the state's map hands its accounts and slots
    /// out in, and every time it is built.
    #[test]
    fn test_the_view_is_deterministic() {
        let forward = outcome();
        let mut reversed_accounts = accounts();
        reversed_accounts.reverse();
        for (_, account) in &mut reversed_accounts {
            let slots: Vec<_> = account.storage.drain().collect();
            account.storage.extend(slots.into_iter().rev());
        }
        let reversed = outcome_with(reversed_accounts);
        assert_eq!(json(&forward), json(&reversed));
        assert_eq!(json(&forward), json(&forward));
        assert_eq!(OutcomeView::new(&forward), OutcomeView::from(&reversed));
    }

    /// The view holds the touched accounts alone, and of each the slots whose value changed,
    /// ascending.
    #[test]
    fn test_the_view_holds_touched_accounts_and_changed_slots() {
        let view = OutcomeView::new(&outcome());
        assert!(!view.accounts.contains_key(&LOADED), "an account only loaded is not shown");
        assert_eq!(view.accounts.len(), 2 + usize::from(FILLER));
        let slots: Vec<U256> = view.accounts[&CONTRACT].storage.iter().map(|s| s.slot).collect();
        let mut expected = Vec::from([U256::from(2), U256::from(10)]);
        expected.extend((0..FILLER).map(|i| U256::from(100 + u64::from(i))));
        assert_eq!(slots, expected, "the slot read and left alone is not shown");
        assert_eq!(
            view.accounts[&CONTRACT].storage[1],
            SlotView { slot: U256::from(10), original: U256::from(5), present: U256::ZERO }
        );
        assert_eq!(view.result.kind, ResultKind::Success);
        assert_eq!(view.result.reason.as_deref(), Some("Stop"));
        assert_eq!(
            view.limit_stop,
            Some(LimitStopView { kind: "DataSize".into(), limit: 200, used: 300 })
        );
        let mut within = outcome();
        within.limit_exceeded = Some(LimitCheck::WithinLimit);
        assert_eq!(OutcomeView::new(&within).limit_stop, None, "a check that passed is no stop");
    }

    /// A revert's reason is its output decoded as a limit stop or a Solidity error, and nothing
    /// for other bytes.
    #[test]
    fn test_a_revert_names_its_reason() {
        let reason = |output: Bytes| ResultView::new(&revert(output)).reason;
        let stop = MegaLimitExceeded { kind: LimitKind::KVUpdate.as_u8(), limit: 7 };
        assert_eq!(
            reason(stop.abi_encode().into()).as_deref(),
            Some("MegaLimitExceeded(KVUpdate, 7)")
        );
        let error = Revert { reason: "boom".into() };
        assert_eq!(reason(error.abi_encode().into()).as_deref(), Some("revert: boom"));
        assert_eq!(reason(Bytes::from_static(b"plain text")), None);
        assert_eq!(reason(Bytes::new()), None);
        let halted = ResultView::new(&halt(HaltReason::OutOfGas(OutOfGasError::Basic)));
        assert_eq!(halted.reason.as_deref(), Some("Base(OutOfGas(Basic))"));
        assert_eq!(halted.output, None);
    }

    /// Every field the view copies moves it: each variant changes one field of the outcome, and
    /// no two of them, nor any of them and the outcome, serialize alike.
    #[test]
    fn test_every_field_moves_the_view() {
        type Change = fn(&mut MegaTransactionOutcome);
        let changes: &[(&str, Change)] = &[
            ("result: revert", |o| o.result = revert(Bytes::from_static(&[0x01]))),
            ("result: revert output", |o| o.result = revert(Bytes::from_static(&[0x02]))),
            ("result: halt", |o| o.result = halt(HaltReason::OutOfGas(OutOfGasError::Basic))),
            ("result: halt reason", |o| o.result = halt(HaltReason::CallTooDeep)),
            ("result: success reason", |o| {
                if let ExecutionResult::Success { reason, .. } = &mut o.result {
                    *reason = SuccessReason::Return;
                }
            }),
            ("result: output", |o| {
                if let ExecutionResult::Success { output, .. } = &mut o.result {
                    *output = Output::Call(Bytes::from_static(&[0x02]));
                }
            }),
            ("result: created address", |o| {
                if let ExecutionResult::Success { output, .. } = &mut o.result {
                    *output = Output::Create(Bytes::from_static(&[0x01]), Some(CONTRACT));
                }
            }),
            ("log: address", |o| logs_mut(o)[0].address = SENDER),
            ("log: topic", |o| {
                let log = &mut logs_mut(o)[0];
                let mut topics = log.topics().to_vec();
                topics[1] = B256::repeat_byte(3);
                log.data = LogData::new_unchecked(topics, log.data.data.clone());
            }),
            ("log: data", |o| {
                let log = &mut logs_mut(o)[0];
                log.data =
                    LogData::new_unchecked(log.topics().to_vec(), Bytes::from_static(&[0xab]));
            }),
            ("log: another", |o| logs_mut(o).push(log())),
            ("gas: regular", |o| o.gas.regular += 1),
            ("gas: state", |o| o.gas.state += 1),
            ("gas: history", |o| o.gas.history += 1),
            ("gas: history bytes", |o| o.gas.history_bytes += 1),
            ("gas: reservoir remaining", |o| o.gas.reservoir_remaining += 1),
            ("gas: floor", |o| o.gas.floor += 1),
            ("gas: gas used", |o| o.gas.gas_used += 1),
            ("result gas: total spent", |o| {
                let gas = result_gas_mut(o);
                gas.set_total_gas_spent(gas.total_gas_spent() + 1);
            }),
            ("result gas: state spent", |o| {
                let gas = result_gas_mut(o);
                gas.set_state_gas_spent(gas.state_gas_spent_final() + 1);
            }),
            ("result gas: refunded", |o| {
                let gas = result_gas_mut(o);
                gas.set_refunded(gas.inner_refunded() + 1);
            }),
            ("result gas: floor", |o| {
                let gas = result_gas_mut(o);
                gas.set_floor_gas(gas.floor_gas() + 1);
            }),
            ("result gas: reservoir remaining", |o| {
                let gas = result_gas_mut(o);
                gas.set_reservoir_remaining(gas.reservoir_remaining() + 1);
            }),
            ("usage: data size", |o| o.usage.data_size += 1),
            ("usage: write records", |o| o.usage.write_records += 1),
            ("limit stop: none", |o| o.limit_exceeded = None),
            ("limit stop: kind", |o| {
                if let Some(LimitCheck::ExceedsLimit { kind, .. }) = &mut o.limit_exceeded {
                    *kind = LimitKind::KVUpdate;
                }
            }),
            ("limit stop: limit", |o| {
                if let Some(LimitCheck::ExceedsLimit { limit, .. }) = &mut o.limit_exceeded {
                    *limit += 1;
                }
            }),
            ("limit stop: used", |o| {
                if let Some(LimitCheck::ExceedsLimit { used, .. }) = &mut o.limit_exceeded {
                    *used += 1;
                }
            }),
            ("oracle read: slot", |o| o.oracle_reads[0].slot = U256::from(9)),
            ("oracle read: answer", |o| o.oracle_reads[0].answer = None),
            ("oracle read: order", |o| o.oracle_reads.reverse()),
            ("account: balance", |o| account_mut(o, SENDER).info.balance += U256::from(1)),
            ("account: nonce", |o| account_mut(o, SENDER).info.nonce += 1),
            ("account: code hash", |o| {
                account_mut(o, CONTRACT).info.code_hash = B256::repeat_byte(0xdd);
            }),
            ("account: created", |o| account_mut(o, CONTRACT).mark_created()),
            ("account: selfdestructed", |o| account_mut(o, CONTRACT).mark_selfdestruct()),
            ("account: touched", |o| account_mut(o, LOADED).mark_touch()),
            ("account: untouched", |o| account_mut(o, SENDER).unmark_touch()),
            ("slot: original", |o| {
                account_mut(o, CONTRACT).storage.get_mut(&U256::from(2)).unwrap().original_value =
                    U256::from(1);
            }),
            ("slot: present", |o| {
                account_mut(o, CONTRACT).storage.get_mut(&U256::from(2)).unwrap().present_value =
                    U256::from(8);
            }),
            ("slot: changed", |o| {
                account_mut(o, CONTRACT).storage.get_mut(&U256::from(3)).unwrap().present_value =
                    U256::from(5);
            }),
            ("slot: key", |o| {
                let storage = &mut account_mut(o, CONTRACT).storage;
                let value = storage.remove(&U256::from(2)).unwrap();
                storage.insert(U256::from(4), value);
            }),
        ];
        let mut seen = BTreeMap::from([(json(&outcome()), "the outcome")]);
        for (name, change) in changes {
            let mut changed = outcome();
            change(&mut changed);
            let json = json(&changed);
            if let Some(twin) = seen.insert(json, name) {
                panic!("{name} serializes like {twin}");
            }
        }
    }

    fn summary_json(outcome: &MegaTransactionOutcome) -> String {
        serde_json::to_string(&OutcomeView::new(outcome).summary()).unwrap()
    }

    /// The summary is one line of the view's figures, the same every time it is built and
    /// whatever order the state's map hands its accounts out in.
    #[test]
    fn test_the_summary_is_one_deterministic_line() {
        let forward = outcome();
        let mut reversed_accounts = accounts();
        reversed_accounts.reverse();
        let reversed = outcome_with(reversed_accounts);
        assert_eq!(summary_json(&forward), summary_json(&reversed));
        assert_eq!(summary_json(&forward), summary_json(&forward));

        let line = OutcomeView::new(&forward).summary().to_string();
        assert_eq!(
            line,
            "success Stop; stop DataSize limit 200 used 300; regular 50000 state 30000 \
             history 10000 history_bytes 125 reservoir_remaining 5000 floor 21000 gas_used 86000; \
             data_size 300 write_records 3; logs 1 accounts 18"
        );
        assert_eq!(summary_json(&forward), format!("\"{line}\""), "it serializes as the line");

        let mut stopless = outcome();
        stopless.limit_exceeded = None;
        stopless.result = halt(HaltReason::OutOfGas(OutOfGasError::Basic));
        assert!(
            OutcomeView::new(&stopless)
                .summary()
                .to_string()
                .starts_with("halt Base(OutOfGas(Basic)); stop -; regular 50000"),
            "no stop is a dash"
        );
    }

    /// Every field the summary holds moves it: each variant changes one of them, and no two of
    /// them, nor any of them and the outcome, serialize alike. What it leaves to the full view —
    /// the logs' contents, the accounts' state — does not move it.
    #[test]
    fn test_every_field_moves_the_summary() {
        type Change = fn(&mut MegaTransactionOutcome);
        let changes: &[(&str, Change)] = &[
            ("result: revert", |o| o.result = revert(Bytes::from_static(&[0x01]))),
            ("result: halt", |o| o.result = halt(HaltReason::OutOfGas(OutOfGasError::Basic))),
            ("result: halt reason", |o| o.result = halt(HaltReason::CallTooDeep)),
            ("result: success reason", |o| {
                if let ExecutionResult::Success { reason, .. } = &mut o.result {
                    *reason = SuccessReason::Return;
                }
            }),
            ("logs: another", |o| logs_mut(o).push(log())),
            ("logs: none", |o| logs_mut(o).clear()),
            ("gas: regular", |o| o.gas.regular += 1),
            ("gas: state", |o| o.gas.state += 1),
            ("gas: history", |o| o.gas.history += 1),
            ("gas: history bytes", |o| o.gas.history_bytes += 1),
            ("gas: reservoir remaining", |o| o.gas.reservoir_remaining += 1),
            ("gas: floor", |o| o.gas.floor += 1),
            ("gas: gas used", |o| o.gas.gas_used += 1),
            ("usage: data size", |o| o.usage.data_size += 1),
            ("usage: write records", |o| o.usage.write_records += 1),
            ("limit stop: none", |o| o.limit_exceeded = None),
            ("limit stop: kind", |o| {
                if let Some(LimitCheck::ExceedsLimit { kind, .. }) = &mut o.limit_exceeded {
                    *kind = LimitKind::KVUpdate;
                }
            }),
            ("limit stop: limit", |o| {
                if let Some(LimitCheck::ExceedsLimit { limit, .. }) = &mut o.limit_exceeded {
                    *limit += 1;
                }
            }),
            ("limit stop: used", |o| {
                if let Some(LimitCheck::ExceedsLimit { used, .. }) = &mut o.limit_exceeded {
                    *used += 1;
                }
            }),
            ("accounts: one more touched", |o| account_mut(o, LOADED).mark_touch()),
            ("accounts: one fewer touched", |o| account_mut(o, SENDER).unmark_touch()),
        ];
        let mut seen = BTreeMap::from([(summary_json(&outcome()), "the outcome")]);
        for (name, change) in changes {
            let mut changed = outcome();
            change(&mut changed);
            if let Some(twin) = seen.insert(summary_json(&changed), name) {
                panic!("{name} summarizes like {twin}");
            }
        }

        let left_to_the_view: &[(&str, Change)] = &[
            ("log: data", |o| {
                let log = &mut logs_mut(o)[0];
                log.data =
                    LogData::new_unchecked(log.topics().to_vec(), Bytes::from_static(&[0xab]));
            }),
            ("account: balance", |o| account_mut(o, SENDER).info.balance += U256::from(1)),
            ("oracle read: answer", |o| o.oracle_reads[0].answer = None),
        ];
        for (name, change) in left_to_the_view {
            let mut changed = outcome();
            change(&mut changed);
            assert_eq!(summary_json(&changed), summary_json(&outcome()), "{name}");
        }
    }
}
