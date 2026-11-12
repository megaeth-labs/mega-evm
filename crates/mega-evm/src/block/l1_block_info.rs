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
//! What the fee computation read is decided by the block, not by whether this node's op-revm
//! fetched the info for it: the fee read is made again, through op-revm's own fetch, once the
//! transaction ran and before it is committed, on the state it ran on, which a node's state cache
//! serves without a database read once op-revm made its own. A validator that replays the block
//! fetches the info at the same transaction and reads the same keys.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::collections::{BTreeMap, BTreeSet};

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

/// A database that answers from the one it wraps and, when it records, keeps what it answered
/// about the L1 block contract.
struct Recording<'a, DB> {
    db: &'a mut DB,
    read: Option<L1BlockRead>,
}

impl<'a, DB> Recording<'a, DB> {
    /// Answers from `db`, recording when `record` says so.
    fn new(db: &'a mut DB, record: bool) -> Self {
        Self { db, read: record.then(L1BlockRead::default) }
    }
}

impl<DB: Database> Database for Recording<'_, DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.db.basic(address)?;
        if let Some(read) = self.read.as_mut().filter(|_| address == L1_BLOCK_CONTRACT) {
            read.account.clone_from(&info);
        }
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let value = self.db.storage(address, index)?;
        if let Some(read) = self.read.as_mut().filter(|_| address == L1_BLOCK_CONTRACT) {
            read.slots.insert(index, value);
        }
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

/// Reads the data-availability footprint gas scalar as block execution reads it before a
/// non-deposit transaction runs: the L1 block contract's account, then the scalar's slot, decoded
/// as op-revm decodes it. With `record`, also what the read found, for the transaction's report.
///
/// # Errors
///
/// The database's, when a read fails.
pub(crate) fn read_footprint_gas_scalar<DB: Database>(
    db: &mut DB,
    record: bool,
) -> Result<(u16, Option<L1BlockRead>), DB::Error> {
    let mut db = Recording::new(db, record);
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
    let mut db = Recording::new(db, true);
    if db.basic(L1_BLOCK_CONTRACT)?.is_some() {
        L1BlockInfo::try_fetch(&mut db, number, spec)?;
    }
    Ok(db.read.unwrap_or_default())
}

/// What block execution has reported of the L1 block contract in the states it committed in the
/// block, and so what is left to report.
///
/// The footprint read is reported with the first non-deposit transaction committed, and the fee
/// read with the first committed transaction the fee computation priced, each without the keys a
/// committed state of a transaction that read the contract named already.
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
    pub(crate) const fn reports_footprint(&self) -> bool {
        !self.footprint
    }

    /// Whether the fee read is still to be reported: no transaction the fee computation priced was
    /// committed yet.
    pub(crate) const fn reports_fee(&self) -> bool {
        !self.fee
    }

    /// Adds to `state`, a transaction's returned state, what `reads` found that no committed state
    /// named yet, as read-only entries: the account as found, and each slot found of an account the
    /// state holds. An entry `state` holds already is left as it is, so the transaction's own load
    /// of the contract, or its write to it, stands; a slot to report goes under it.
    pub(crate) fn add_to<'a>(
        &self,
        state: &mut EvmState,
        reads: impl IntoIterator<Item = &'a L1BlockRead>,
    ) {
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
            let account =
                state.entry(L1_BLOCK_CONTRACT).or_insert_with(|| match read.account.clone() {
                    Some(info) => Account::from(info),
                    None => Account::new_not_existing(TransactionId::ZERO),
                });
            for (slot, value) in fresh {
                account
                    .storage
                    .entry(slot)
                    .or_insert_with(|| EvmStorageSlot::new(value, TransactionId::ZERO));
            }
        }
    }

    /// Records what the committed `state` of a non-deposit transaction reported: the footprint
    /// read, which every non-deposit transaction makes, and, when the fee computation `priced` it,
    /// the fee read; and the keys of the contract `state` names, which are in the witness from now
    /// on.
    pub(crate) fn committed(&mut self, state: &EvmState, priced: bool) {
        if self.fee || (self.footprint && !priced) {
            return;
        }
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
        let (scalar, footprint) = read_footprint_gas_scalar(&mut MemoryDatabase::default(), true)
            .expect("an absent contract holds a zero scalar");
        assert_eq!(scalar, 0);
        let mut state = EvmState::default();
        L1BlockReport::default().add_to(&mut state, footprint.iter());
        let account = &state[&L1_BLOCK_CONTRACT];
        assert!(account.is_loaded_as_not_existing() && !account.is_touched());
        assert!(account.storage.is_empty(), "no slot of an absent account");
    }

    /// The footprint read decodes the scalar as op-revm does, and records what it read only when
    /// asked: the account and the scalar's slot.
    #[test]
    fn test_the_footprint_read_records_only_when_asked() {
        let (scalar, read) = read_footprint_gas_scalar(&mut chain(scalars()), false).unwrap();
        assert_eq!((scalar, read), (400, None));
        let (scalar, read) = read_footprint_gas_scalar(&mut chain(scalars()), true).unwrap();
        let read = read.expect("recorded");
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
        let (_, footprint) = read_footprint_gas_scalar(&mut chain(scalars()), true).unwrap();
        let fee = read_fee_info(&mut chain(scalars()), U256::from(9), spec).unwrap();
        let mut report = L1BlockReport::default();
        assert!(report.reports_footprint() && report.reports_fee());

        let mut system = EvmState::default();
        report.add_to(&mut system, footprint.iter());
        report.committed(&system, false);
        assert!(!report.reports_footprint() && report.reports_fee());

        let mut priced = EvmState::default();
        report.add_to(&mut priced, footprint.iter().chain([&fee]));
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
}
