//! The L1 block contract's account and slots a block's transactions read outside their journal,
//! and their report with the first transaction that read them.
//!
//! Two reads of the base layer read the L1 block contract on the block's state rather than through
//! a transaction's journal. The fee computation prices a non-deposit transaction that pays an L1
//! fee: op-revm reads the contract's account, its L1 base fee (slot 1), its Ecotone blob base fee
//! (7) and fee scalars (3), the L1 fee overhead (5) only when those scalars are empty, and its
//! operator fee scalars (8), once per block, at the first transaction it prices, on the state the
//! transactions before it left. The footprint read holds every non-deposit transaction, a Mega
//! System Transaction included, which pays no L1 fee, to the data-availability footprint: block
//! execution reads the contract's account and the footprint gas scalar in slot 8 before each one
//! runs. Neither read lands in a transaction's returned state, so a stateless witness built from
//! the states the block commits would miss them, and a validator re-executing the block from such
//! a witness would fail it on a read the witness does not hold.
//!
//! Block execution therefore reports them with the first transaction that read them. Once each per
//! block, the account and every slot either read reached are added to that transaction's returned
//! state as read-only entries — the account neither touched nor created, each slot unchanged —
//! which the block commits with the rest of the state, so a witness built from the committed
//! states holds them; their commit changes nothing. A read-only entry never replaces an entry the
//! transaction's own execution made, so its write to the contract stands. A contract the state
//! does not hold is reported as not existing, with no slot: the slots of an absent account are
//! zero without a read. Nothing is read before the block's transactions, so a block of deposits
//! alone reads nothing of the contract, and a slot neither read reaches — the overhead beside set
//! scalars — is not read at all.
//!
//! Which transaction is the first is the block's order, which the commits make: a builder may
//! execute several candidates before it commits any, or commit an outcome another executor of the
//! block executed. So every outcome carries what it read ([`L1BlockReads`]) beside its returned
//! state, whatever its executor had reported, and the executor that commits it decides, from what
//! it has itself committed ([`L1BlockReport`]), what the committed state reports. The returned
//! state carries the report its executor would make if it committed the outcome next; a commit
//! takes that out again and adds what its own report lacks, so the committed states report each
//! key once, with the first committed transaction that read it.
//!
//! What the fee computation read is decided by the block, not by whether this node's op-revm
//! fetched the info for it: op-revm fetches it once per block, at the first transaction it prices,
//! and prices every later one from what it fetched, and the fee read is made again, through
//! op-revm's own fetch, once that transaction ran and before it is committed, on the state it ran
//! on, which a node's state cache serves without a database read once op-revm made its own, and
//! carried by every transaction the executor prices in the block. A validator that replays the
//! block fetches the info at its first such transaction and reads the same keys.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};

use alloy_primitives::{Address, B256, U256};
use op_revm::{
    constants::{
        ECOTONE_L1_BLOB_BASE_FEE_SLOT, ECOTONE_L1_FEE_SCALARS_SLOT, L1_BASE_FEE_SLOT,
        L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT, OPERATOR_FEE_SCALARS_SLOT,
    },
    L1BlockInfo, OpSpecId,
};
use revm::{
    bytecode::Bytecode,
    state::{Account, AccountInfo, EvmState, EvmStorageSlot, TransactionId},
    Database,
};

/// The slots of the L1 block contract the fee computation and the footprint read can reach, in
/// ascending order: the L1 base fee (1), the Ecotone fee scalars (3), the L1 fee overhead (5), the
/// Ecotone blob base fee (7) and the operator fee scalars (8), which is also the slot of the DA
/// footprint gas scalar.
///
/// The fee computation reads the overhead only when the Ecotone scalars it finds are empty, and
/// the others always; the footprint read reads the operator fee scalars' slot alone.
pub const L1_BLOCK_INFO_SLOTS: [U256; 5] = [
    L1_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT,
    L1_OVERHEAD_SLOT,
    ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    OPERATOR_FEE_SCALARS_SLOT,
];

/// What one read of the L1 block contract found: the account as the block's state held it, and
/// each slot the read reached, with the value it found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct L1BlockRead {
    /// The account, `None` for a state that holds no contract at the address.
    account: Option<AccountInfo>,
    /// The slots read, by key. A slot of an account the state does not hold is never reported.
    slots: BTreeMap<U256, U256>,
}

/// A database that answers from the one it wraps and keeps what it answered about the L1 block
/// contract.
struct Recording<'a, DB> {
    db: &'a mut DB,
    read: L1BlockRead,
}

impl<'a, DB> Recording<'a, DB> {
    /// Answers from `db`.
    const fn new(db: &'a mut DB) -> Self {
        Self { db, read: L1BlockRead { account: None, slots: BTreeMap::new() } }
    }
}

impl<DB: Database> Database for Recording<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.db.basic(address)?;
        if address == L1_BLOCK_CONTRACT {
            self.read.account.clone_from(&info);
        }
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let value = self.db.storage(address, index)?;
        if address == L1_BLOCK_CONTRACT {
            self.read.slots.insert(index, value);
        }
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

/// Reads the data-availability footprint gas scalar as block execution reads it before a
/// non-deposit transaction runs: the L1 block contract's account, then the scalar's slot, decoded
/// as op-revm decodes it; and what the read found, for the transaction's report.
///
/// # Errors
///
/// The database's, when a read fails.
pub(crate) fn read_footprint_gas_scalar<DB: Database>(
    db: &mut DB,
) -> Result<(u16, L1BlockRead), DB::Error> {
    let mut db = Recording::new(db);
    // Load the account first: a database that has never seen it cannot serve its storage.
    db.basic(L1_BLOCK_CONTRACT)?;
    let scalar = L1BlockInfo::fetch_da_footprint_gas_scalar(&mut db)?;
    Ok((scalar, db.read))
}

/// Reads what the fee computation reads to price a non-deposit transaction of block `number`
/// under `spec`: the L1 block contract's account, and, when the state holds it, every slot
/// op-revm's fetch reads, through that fetch itself ([`L1BlockInfo::try_fetch`]), so the two
/// cannot part. A state that holds no contract reads no slot.
///
/// # Errors
///
/// The database's, when a read fails.
pub(crate) fn read_fee_info<DB: Database>(
    db: &mut DB,
    number: U256,
    spec: OpSpecId,
) -> Result<L1BlockRead, DB::Error> {
    let mut db = Recording::new(db);
    if db.basic(L1_BLOCK_CONTRACT)?.is_some() {
        L1BlockInfo::try_fetch(&mut db, number, spec)?;
    }
    Ok(db.read)
}

/// What a transaction read of the L1 block contract outside its journal, carried with its outcome
/// until a block executor commits it, and what the report of the executor that executed it added
/// to its returned state.
///
/// A deposit reads nothing; every other transaction the footprint, made before it ran; and a
/// transaction the fee computation priced the fee read too. Each read is carried whole, whatever
/// its executor had reported, so the executor that commits the outcome can report what it has not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct L1BlockReads {
    /// The footprint read: `Some` for every non-deposit transaction.
    footprint: Option<L1BlockRead>,
    /// The fee read: `Some` for a transaction the fee computation priced. One read serves every
    /// transaction the fee computation prices in the block, so its outcomes share it.
    fee: Option<Arc<L1BlockRead>>,
    /// The entries the report of the executor that executed the transaction added to its returned
    /// state.
    added: Added,
}

impl L1BlockReads {
    /// The reads of a non-deposit transaction: the `footprint` read, and the `fee` read when the
    /// fee computation priced it.
    pub(crate) const fn new(footprint: L1BlockRead, fee: Option<Arc<L1BlockRead>>) -> Self {
        Self { footprint: Some(footprint), fee, added: Added::NONE }
    }

    /// Whether these are the reads of a transaction that is a deposit when `is_deposit`, and that
    /// the fee computation priced when `priced`: none for a deposit, the footprint read for every
    /// other transaction, and the fee read for one the fee computation priced.
    pub(crate) const fn are_of(&self, is_deposit: bool, priced: bool) -> bool {
        self.footprint.is_some() != is_deposit && self.fee.is_some() == (!is_deposit && priced)
    }
}

/// The entries a report added to a transaction's returned state: the L1 block contract's account,
/// when the state named none, and the slots it added.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Added {
    /// Whether the report added the account, and so every slot under it.
    account: bool,
    /// The slots the report added.
    slots: Vec<U256>,
}

impl Added {
    /// Nothing added.
    const NONE: Self = Self { account: false, slots: Vec::new() };

    /// Takes these entries out of `state`, which leaves what the transaction's own execution put
    /// there.
    fn take_out(&self, state: &mut EvmState) {
        if self.account {
            state.remove(&L1_BLOCK_CONTRACT);
        } else if let Some(account) =
            state.get_mut(&L1_BLOCK_CONTRACT).filter(|_| !self.slots.is_empty())
        {
            for slot in &self.slots {
                account.storage.remove(slot);
            }
        }
    }
}

/// What block execution has reported of the L1 block contract in the states it committed in the
/// block, and so what is left to report.
///
/// The footprint read is reported with the first non-deposit transaction committed, and the fee
/// read with the first committed transaction the fee computation priced, each without the keys a
/// committed state of a transaction that read the contract named already. It is the committing
/// executor's: an outcome executed elsewhere, or before another commit, is reported by what the
/// executor that commits it has committed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct L1BlockReport {
    /// Whether a committed transaction reported the footprint read.
    footprint: bool,
    /// Whether a committed transaction reported the fee read.
    fee: bool,
    /// Whether a committed state of a transaction that read the contract named its account.
    account: bool,
    /// The slots of [`L1_BLOCK_INFO_SLOTS`] such a state named.
    slots: BTreeSet<U256>,
}

impl L1BlockReport {
    /// Whether the footprint read is still to be reported: no transaction that made it was
    /// committed yet.
    const fn reports_footprint(&self) -> bool {
        !self.footprint
    }

    /// Whether the fee read is still to be reported: no transaction the fee computation priced was
    /// committed yet.
    const fn reports_fee(&self) -> bool {
        !self.fee
    }

    /// Adds to `state`, the returned state of the transaction that made `reads`, what this report
    /// would add if the transaction were committed next, and keeps on `reads` what it added, which
    /// a commit takes out again ([`commit`](Self::commit)).
    pub(crate) fn report(&self, state: &mut EvmState, reads: &mut L1BlockReads) {
        reads.added = self.add(state, reads);
    }

    /// Settles `state`, the returned state of the non-deposit transaction that made `reads`, which
    /// this report's executor commits, and records what it reports: it takes out what the report
    /// of the executor that executed it added, adds what this report lacks, and marks the reads
    /// `state` now reports ([`committed`](Self::committed)), the fee read when the fee computation
    /// `priced` the transaction.
    ///
    /// Committed where it executed and before anything else was committed, the outcome is
    /// committed as it was returned: the two reports are one.
    pub(crate) fn commit(&mut self, state: &mut EvmState, reads: &L1BlockReads, priced: bool) {
        reads.added.take_out(state);
        self.add(state, reads);
        self.committed(state, priced);
    }

    /// Adds to `state` what `reads` found that this report lacks — the footprint read while no
    /// committed transaction reported it, the fee read while no committed transaction the fee
    /// computation priced did — and answers what it added.
    fn add(&self, state: &mut EvmState, reads: &L1BlockReads) -> Added {
        let footprint = reads.footprint.as_ref().filter(|_| self.reports_footprint());
        let fee = reads.fee.as_deref().filter(|_| self.reports_fee());
        self.add_to(state, footprint.into_iter().chain(fee))
    }

    /// Adds to `state`, a transaction's returned state, what `reads` found that no committed state
    /// named yet, as read-only entries: the account as found, and each slot found of an account the
    /// state holds. An entry `state` holds already is left as it is, so the transaction's own load
    /// of the contract, or its write to it, stands; a slot to report goes under it. Answers what it
    /// added.
    fn add_to<'a>(
        &self,
        state: &mut EvmState,
        reads: impl IntoIterator<Item = &'a L1BlockRead>,
    ) -> Added {
        let mut added = Added::NONE;
        for read in reads {
            let fresh: BTreeMap<U256, U256> = match read.account {
                Some(_) => read
                    .slots
                    .iter()
                    .filter(|(slot, _)| !self.slots.contains(*slot))
                    .map(|(slot, value)| (*slot, *value))
                    .collect(),
                None => BTreeMap::new(),
            };
            if self.account && fresh.is_empty() {
                continue;
            }
            let account = state.entry(L1_BLOCK_CONTRACT).or_insert_with(|| {
                added.account = true;
                match read.account.clone() {
                    Some(info) => Account::from(info),
                    None => Account::new_not_existing(TransactionId::ZERO),
                }
            });
            for (slot, value) in fresh {
                let mut inserted = false;
                account.storage.entry(slot).or_insert_with(|| {
                    inserted = true;
                    EvmStorageSlot::new(value, TransactionId::ZERO)
                });
                if inserted {
                    added.slots.push(slot);
                }
            }
        }
        added
    }

    /// Records what the committed `state` of a non-deposit transaction reported: the footprint
    /// read, which every non-deposit transaction makes, and, when the fee computation `priced` it,
    /// the fee read; and the keys of the contract `state` names, its own entries among them, which
    /// are in the witness from now on, so no later report names them again.
    fn committed(&mut self, state: &EvmState, priced: bool) {
        self.footprint = true;
        self.fee |= priced;
        if let Some(account) = state.get(&L1_BLOCK_CONTRACT) {
            self.account = true;
            let named = account.storage.keys().filter(|slot| L1_BLOCK_INFO_SLOTS.contains(slot));
            self.slots.extend(named);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use op_revm::constants::{
        BASE_FEE_SCALAR_OFFSET, BLOB_BASE_FEE_SCALAR_OFFSET, DA_FOOTPRINT_GAS_SCALAR_OFFSET,
        DA_FOOTPRINT_GAS_SCALAR_SLOT,
    };
    use revm::state::AccountStatus;

    use super::*;
    use crate::test_utils::{ErrorInjectingDatabase, MemoryDatabase};

    /// The Ecotone fee scalars word with a base fee scalar of 7 and a blob base fee scalar of 9,
    /// at the offsets the pricing reads them from: scalars that are set.
    fn scalars() -> U256 {
        let mut word = [0_u8; 32];
        word[BASE_FEE_SCALAR_OFFSET + 3] = 7;
        word[BLOB_BASE_FEE_SCALAR_OFFSET + 3] = 9;
        U256::from_be_bytes(word)
    }

    /// The operator fee scalars word holding a footprint gas scalar of 400.
    fn footprint_word() -> U256 {
        let mut word = [0_u8; 32];
        word[DA_FOOTPRINT_GAS_SCALAR_OFFSET..DA_FOOTPRINT_GAS_SCALAR_OFFSET + 2]
            .copy_from_slice(&400_u16.to_be_bytes());
        U256::from_be_bytes(word)
    }

    /// The value the chain holds in every slot of the set: a distinct non-zero value, with
    /// `scalars` at the scalars slot and [`footprint_word`] at the operator fee scalars slot.
    fn values(scalars: U256) -> BTreeMap<U256, U256> {
        let mut values: BTreeMap<_, _> =
            L1_BLOCK_INFO_SLOTS.map(|slot| (slot, slot + U256::from(1_000))).into();
        values.insert(ECOTONE_L1_FEE_SCALARS_SLOT, scalars);
        values.insert(OPERATOR_FEE_SCALARS_SLOT, footprint_word());
        values
    }

    /// A chain holding the L1 block contract with [`values`].
    fn chain(scalars: U256) -> MemoryDatabase {
        let mut db = MemoryDatabase::default();
        db.set_account_code(L1_BLOCK_CONTRACT, Bytes::from(vec![0x00]));
        for (slot, value) in values(scalars) {
            db.set_account_storage(L1_BLOCK_CONTRACT, slot, value);
        }
        db
    }

    /// The set is the five slots either read can reach: it names the footprint slot and the
    /// overhead, holds no slot twice and is ascending.
    #[test]
    fn test_the_slots_are_distinct_ascending_and_hold_the_footprint_slot_and_the_overhead() {
        assert_eq!(L1_BLOCK_INFO_SLOTS.map(|slot| slot.to::<u64>()), [1, 3, 5, 7, 8]);
        assert!(L1_BLOCK_INFO_SLOTS.contains(&DA_FOOTPRINT_GAS_SCALAR_SLOT));
        assert!(L1_BLOCK_INFO_SLOTS.contains(&L1_OVERHEAD_SLOT));
        assert!(L1_BLOCK_INFO_SLOTS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// The fee read is op-revm's fetch on Satin's base: the account and slots 1, 3, 7 and 8 with
    /// the chain's values, and the overhead only when the scalars are empty.
    #[test]
    fn test_the_fee_read_reads_the_overhead_only_beside_empty_scalars() {
        let spec = crate::MegaSpecId::SATIN.into_op_spec();
        for (scalars, overhead) in [(scalars(), false), (U256::ZERO, true)] {
            let read = read_fee_info(&mut chain(scalars), U256::from(9), spec).expect("served");
            let account = read.account.expect("the chain holds the contract");
            assert_eq!(account.code_hash, alloy_primitives::keccak256([0x00]));
            let mut expected = values(scalars);
            if !overhead {
                expected.remove(&L1_OVERHEAD_SLOT);
            }
            assert_eq!(read.slots, expected, "scalars {scalars}");
        }
    }

    /// A chain without the contract is read as not existing, and no slot is read: a slot read
    /// would fail on this database. Reported, it is an account recorded as not existing, with no
    /// slot, the footprint read's included.
    #[test]
    fn test_an_absent_contract_is_read_as_absent_and_no_slot_is_read() {
        let spec = crate::MegaSpecId::SATIN.into_op_spec();
        for slot in L1_BLOCK_INFO_SLOTS {
            let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
            db.fail_on_storage = Some((L1_BLOCK_CONTRACT, slot));
            let read = read_fee_info(&mut db, U256::from(9), spec).expect("no slot is read");
            assert_eq!(read, L1BlockRead::default(), "absent, with no slot");
        }
        let (scalar, footprint) = read_footprint_gas_scalar(&mut MemoryDatabase::default())
            .expect("an absent contract holds a zero scalar");
        assert_eq!(scalar, 0);
        let mut state = EvmState::default();
        L1BlockReport::default().add_to(&mut state, [&footprint]);
        let account = &state[&L1_BLOCK_CONTRACT];
        assert!(account.is_loaded_as_not_existing() && !account.is_touched());
        assert!(account.storage.is_empty(), "no slot of an absent account");
    }

    /// The footprint read decodes the scalar as op-revm does, and records what it read: the
    /// account and the scalar's slot.
    #[test]
    fn test_the_footprint_read_records_the_account_and_the_scalars_slot() {
        let (scalar, read) = read_footprint_gas_scalar(&mut chain(scalars())).unwrap();
        assert_eq!(scalar, 400);
        assert!(read.account.is_some());
        assert_eq!(read.slots, BTreeMap::from([(OPERATOR_FEE_SCALARS_SLOT, footprint_word())]));
    }

    /// A report adds read-only entries — the account neither touched nor created, each slot
    /// unchanged at the value read — and leaves what the transaction's own state holds: an account
    /// it touched stays touched, and a slot it wrote keeps its write.
    #[test]
    fn test_a_report_never_replaces_the_transactions_own_entries() {
        let spec = crate::MegaSpecId::SATIN.into_op_spec();
        let read = read_fee_info(&mut chain(scalars()), U256::from(9), spec).unwrap();

        let mut state = EvmState::default();
        L1BlockReport::default().add_to(&mut state, [&read]);
        let account = &state[&L1_BLOCK_CONTRACT];
        assert!(!account.is_touched() && !account.is_created());
        assert_eq!(account.storage.len(), 4);
        for (slot, value) in &read.slots {
            assert_eq!(account.storage[slot].present_value, *value);
            assert!(!account.storage[slot].is_changed());
        }

        let mut own = Account::from(read.account.clone().unwrap());
        own.status = AccountStatus::Touched;
        let mut written = EvmStorageSlot::new(scalars(), TransactionId::ZERO);
        written.present_value = U256::ZERO;
        own.storage.insert(ECOTONE_L1_FEE_SCALARS_SLOT, written.clone());
        let mut state = EvmState::from_iter([(L1_BLOCK_CONTRACT, own)]);
        L1BlockReport::default().add_to(&mut state, [&read]);
        let account = &state[&L1_BLOCK_CONTRACT];
        assert!(account.is_touched(), "the transaction's own entry");
        assert_eq!(account.storage[&ECOTONE_L1_FEE_SCALARS_SLOT], written, "its write stands");
        assert_eq!(account.storage.len(), 4, "the other three slots are added");
    }

    /// Once a committed state reported a read, nothing of it is reported again: after a Mega
    /// System Transaction's footprint read, the fee read adds the slots it read beside the scalar's
    /// alone, under the account it must name to hold them; after a transaction the fee
    /// computation priced, nothing is left to report.
    #[test]
    fn test_what_a_committed_state_reported_is_not_reported_again() {
        let spec = crate::MegaSpecId::SATIN.into_op_spec();
        let (_, footprint) = read_footprint_gas_scalar(&mut chain(scalars())).unwrap();
        let fee = read_fee_info(&mut chain(scalars()), U256::from(9), spec).unwrap();
        let mut report = L1BlockReport::default();
        assert!(report.reports_footprint() && report.reports_fee());

        let mut system = EvmState::default();
        report.add_to(&mut system, [&footprint]);
        report.committed(&system, false);
        assert!(!report.reports_footprint() && report.reports_fee());

        let mut priced = EvmState::default();
        report.add_to(&mut priced, [&footprint, &fee]);
        let slots: BTreeSet<_> = priced[&L1_BLOCK_CONTRACT].storage.keys().copied().collect();
        assert_eq!(
            slots,
            [L1_BASE_FEE_SLOT, ECOTONE_L1_FEE_SCALARS_SLOT, ECOTONE_L1_BLOB_BASE_FEE_SLOT].into()
        );
        report.committed(&priced, true);
        assert!(!report.reports_footprint() && !report.reports_fee());

        let mut later = EvmState::default();
        report.add_to(&mut later, [&fee]);
        assert!(later.is_empty(), "nothing is left to report");
    }

    /// The reads of a priced transaction on [`chain`]: its footprint read and its fee read.
    fn priced_reads() -> L1BlockReads {
        let spec = crate::MegaSpecId::SATIN.into_op_spec();
        let (_, footprint) = read_footprint_gas_scalar(&mut chain(scalars())).unwrap();
        let fee = read_fee_info(&mut chain(scalars()), U256::from(9), spec).unwrap();
        L1BlockReads::new(footprint, Some(Arc::new(fee)))
    }

    /// A report after a priced transaction's commit: nothing is left to report.
    fn reported() -> L1BlockReport {
        let mut report = L1BlockReport::default();
        let mut state = EvmState::default();
        report.commit(&mut state, &priced_reads(), true);
        assert!(!report.reports_footprint() && !report.reports_fee());
        report
    }

    /// A transaction's own entry of the contract: the account touched, and the scalars' slot
    /// written from the chain's value to zero.
    fn own_entry() -> EvmState {
        let mut own = Account::from(chain(scalars()).basic(L1_BLOCK_CONTRACT).unwrap().unwrap());
        own.status = AccountStatus::Touched;
        let mut written = EvmStorageSlot::new(scalars(), TransactionId::ZERO);
        written.present_value = U256::ZERO;
        own.storage.insert(ECOTONE_L1_FEE_SCALARS_SLOT, written);
        EvmState::from_iter([(L1_BLOCK_CONTRACT, own)])
    }

    /// A commit by the executor that executed the transaction, with nothing committed in between,
    /// leaves the returned state as it was: both reports are one.
    #[test]
    fn test_a_commit_on_the_report_that_executed_changes_nothing() {
        for (report, own) in [
            (L1BlockReport::default(), EvmState::default()),
            (L1BlockReport::default(), own_entry()),
            (reported(), EvmState::default()),
            (reported(), own_entry()),
        ] {
            let mut reads = priced_reads();
            let mut state = own.clone();
            report.report(&mut state, &mut reads);
            let returned = state.clone();
            report.clone().commit(&mut state, &reads, true);
            assert_eq!(state, returned);
        }
    }

    /// An outcome executed on a report that had reported nothing, committed on one that reported
    /// everything, loses every entry its executor's report added and keeps its own; one executed on
    /// a report that had reported everything, committed on one that reported nothing, gets every
    /// read added under its own entry, which it replaces in nothing.
    #[test]
    fn test_a_commit_takes_out_what_the_executing_report_added_and_adds_what_its_own_lacks() {
        for own in [EvmState::default(), own_entry()] {
            let mut reads = priced_reads();
            let mut state = own.clone();
            L1BlockReport::default().report(&mut state, &mut reads);
            assert_eq!(state[&L1_BLOCK_CONTRACT].storage.len(), 4, "reported where executed");
            reported().commit(&mut state, &reads, true);
            assert_eq!(state, own, "nothing reported where committed, the own entry kept");

            let mut reads = priced_reads();
            let mut state = own.clone();
            reported().report(&mut state, &mut reads);
            assert_eq!(state, own, "nothing reported where executed");
            let mut report = L1BlockReport::default();
            report.commit(&mut state, &reads, true);
            let mut expected = own.clone();
            L1BlockReport::default().add_to(
                &mut expected,
                [reads.footprint.as_ref().unwrap(), reads.fee.as_deref().unwrap()],
            );
            assert_eq!(state, expected, "reported where committed");
            if !own.is_empty() {
                let account = &state[&L1_BLOCK_CONTRACT];
                assert!(account.is_touched(), "the own account stands");
                assert_eq!(
                    account.storage[&ECOTONE_L1_FEE_SCALARS_SLOT],
                    own[&L1_BLOCK_CONTRACT].storage[&ECOTONE_L1_FEE_SCALARS_SLOT],
                    "the own write stands"
                );
            }
            assert!(!report.reports_footprint() && !report.reports_fee(), "and marked");
        }
    }

    /// A deposit reads nothing, every other transaction the footprint, and one the fee
    /// computation priced the fee too.
    #[test]
    fn test_the_reads_are_of_the_transaction_that_made_them() {
        let none = L1BlockReads::default();
        let footprint = L1BlockReads::new(L1BlockRead::default(), None);
        let priced = priced_reads();
        assert!(none.are_of(true, false) && none.are_of(true, true));
        assert!(!none.are_of(false, false) && !none.are_of(false, true));
        assert!(footprint.are_of(false, false) && !footprint.are_of(false, true));
        assert!(priced.are_of(false, true) && !priced.are_of(false, false));
        assert!(!footprint.are_of(true, false) && !priced.are_of(true, true));
    }

    /// The recording database answers every read as the database it wraps — the code by hash and
    /// the block hashes, which no read of the contract asks for, included — and records what it
    /// answered about the contract alone.
    #[test]
    fn test_the_recording_answers_as_the_database_it_wraps() {
        let mut db = chain(scalars());
        let mut wrapped = chain(scalars());
        let mut recording = Recording::new(&mut db);
        let code_hash = alloy_primitives::keccak256([0x00]);
        let code = wrapped.code_by_hash(code_hash).unwrap();
        assert!(!code.is_empty(), "the chain holds the contract's code");
        assert_eq!(recording.code_by_hash(code_hash).unwrap(), code);
        let hash = wrapped.block_hash(7).unwrap();
        assert_ne!(hash, B256::ZERO);
        assert_eq!(recording.block_hash(7).unwrap(), hash);
        let other = Address::with_last_byte(9);
        assert_eq!(recording.basic(other).unwrap(), wrapped.basic(other).unwrap());
        assert_eq!(recording.storage(other, U256::from(1)).unwrap(), U256::ZERO);
        assert_eq!(recording.read, L1BlockRead::default(), "nothing of the contract was read");

        let mut db = ErrorInjectingDatabase::new(chain(scalars()));
        db.fail_on_code_by_hash = Some(code_hash);
        db.fail_on_block_hash = Some(7);
        let mut recording = Recording::new(&mut db);
        assert!(recording.code_by_hash(code_hash).is_err(), "a failed read stays one");
        assert!(recording.block_hash(7).is_err(), "a failed read stays one");
    }

    /// A committed state's own entries of the contract are in the witness from its commit on, so
    /// no later report names them again, whichever read was still to be reported: a Mega System
    /// Transaction whose own execution read the L1 base fee leaves the fee read that slot to skip.
    #[test]
    fn test_a_committed_states_own_entries_are_not_reported_again() {
        let reads = priced_reads();
        let mut report = L1BlockReport::default();
        let mut system = EvmState::default();
        report.add_to(&mut system, [reads.footprint.as_ref().unwrap()]);
        report.committed(&system, false);
        let mut own = system.clone();
        own.get_mut(&L1_BLOCK_CONTRACT)
            .unwrap()
            .storage
            .insert(L1_BASE_FEE_SLOT, EvmStorageSlot::new(U256::from(1_001), TransactionId::ZERO));
        report.committed(&own, false);
        assert!(!report.reports_footprint() && report.reports_fee());

        let mut priced = EvmState::default();
        report.add_to(&mut priced, [reads.fee.as_deref().unwrap()]);
        let slots: BTreeSet<_> = priced[&L1_BLOCK_CONTRACT].storage.keys().copied().collect();
        assert_eq!(slots, [ECOTONE_L1_FEE_SCALARS_SLOT, ECOTONE_L1_BLOB_BASE_FEE_SLOT].into());
    }
}
