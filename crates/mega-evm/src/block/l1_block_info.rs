//! The L1 block info a block's transactions are priced against, read before the block's
//! transactions and handed to the pre-block observer.
//!
//! A non-deposit transaction is priced against the L1 block contract: op-revm reads the L1 base
//! fee, the Ecotone blob base fee and fee scalars, the L1 fee overhead when the scalars it finds
//! are empty, and the operator fee scalars, once per block, and block execution reads the DA
//! footprint gas scalar before every non-deposit transaction. Both read the block's state
//! directly, not through a transaction's journal, so the account and the slots they read land in
//! no transaction's returned state. A stateless witness built from the returned states alone
//! would miss them, and a validator's database that answers a missing slot with zero would price
//! the L1 fee, the operator fee and the footprint differently without an error.
//!
//! [`read_l1_block_info`] reads the account and the slots before the block's transactions and
//! hands them back as read-only entries, as the reads of the `SequencerRegistry`'s pending changes
//! are handed back, so the pre-block states carry them and a witness built from the states is
//! complete. It reads every slot the pricing can read, whatever the contract holds before the
//! block: op-revm makes its read at the block's first non-deposit transaction, on the state the
//! deposits before it left, so which slots it reads is not decided by the state the block starts
//! from. A deposit that empties the scalars turns the read to the overhead, and no transaction's
//! state would name it. The entry may therefore carry a slot the block does not read after all.
//! The read loads the account and its slots into the block's state cache and changes nothing: an
//! untouched entry is not committed, and the transactions' own reads find the same values, or the
//! ones the block's L1 attributes deposit wrote over them.

use alloy_primitives::U256;
use op_revm::constants::{
    ECOTONE_L1_BLOB_BASE_FEE_SLOT, ECOTONE_L1_FEE_SCALARS_SLOT, L1_BASE_FEE_SLOT,
    L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT, OPERATOR_FEE_SCALARS_SLOT,
};
use revm::{
    state::{Account, EvmState, EvmStorageSlot, TransactionId},
    Database,
};

/// The slots of the L1 block contract a block's transactions can be priced against, in ascending
/// order: the L1 base fee (1), the Ecotone fee scalars (3), the L1 fee overhead (5), the Ecotone
/// blob base fee (7) and the operator fee scalars (8), which is also the slot of the DA footprint
/// gas scalar.
///
/// The pricing reads the overhead only when the Ecotone scalars it finds are empty, and the
/// others always. The overhead is in the set all the same: the scalars the pricing finds are the
/// ones the block's deposits left, not the ones the block starts from.
pub const L1_BLOCK_INFO_SLOTS: [U256; 5] = [
    L1_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT,
    L1_OVERHEAD_SLOT,
    ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    OPERATOR_FEE_SCALARS_SLOT,
];

/// Reads the L1 block contract's account and every slot of [`L1_BLOCK_INFO_SLOTS`] from `db` and
/// answers them as a read-only state: the account as loaded, neither touched nor created, with
/// each slot as an unchanged entry; or, for a chain that does not hold the contract, the account
/// recorded as not existing and no slot read, since the slots of an absent account are zero
/// without a read.
///
/// Every slot is read whatever the others hold, the overhead beside set scalars included.
///
/// Nothing is committed. Block execution hands the state to the pre-block observer and commits
/// it, which changes nothing.
///
/// # Errors
///
/// The database's, when a read fails.
pub fn read_l1_block_info<DB: Database>(db: &mut DB) -> Result<EvmState, DB::Error> {
    let Some(info) = db.basic(L1_BLOCK_CONTRACT)? else {
        let absent = Account::new_not_existing(TransactionId::ZERO);
        return Ok(EvmState::from_iter([(L1_BLOCK_CONTRACT, absent)]));
    };
    let mut account = Account::from(info);
    for slot in L1_BLOCK_INFO_SLOTS {
        let value = db.storage(L1_BLOCK_CONTRACT, slot)?;
        account.storage.insert(slot, EvmStorageSlot::new(value, TransactionId::ZERO));
    }
    Ok(EvmState::from_iter([(L1_BLOCK_CONTRACT, account)]))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use op_revm::constants::{
        BASE_FEE_SCALAR_OFFSET, BLOB_BASE_FEE_SCALAR_OFFSET, DA_FOOTPRINT_GAS_SCALAR_SLOT,
    };

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

    /// The value the chain holds in every slot of the set: a distinct non-zero value, with
    /// `scalars` at the scalars slot.
    fn values(scalars: U256) -> [(U256, U256); 5] {
        let mut values = L1_BLOCK_INFO_SLOTS.map(|slot| (slot, slot + U256::from(1_000)));
        values[1] = (ECOTONE_L1_FEE_SCALARS_SLOT, scalars);
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

    /// The set is the five slots the pricing can read: it names the footprint slot and the
    /// overhead, holds no slot twice and is ascending.
    #[test]
    fn test_the_slots_are_distinct_ascending_and_hold_the_footprint_slot_and_the_overhead() {
        assert_eq!(L1_BLOCK_INFO_SLOTS.map(|slot| slot.to::<u64>()), [1, 3, 5, 7, 8]);
        assert!(L1_BLOCK_INFO_SLOTS.contains(&DA_FOOTPRINT_GAS_SCALAR_SLOT));
        assert!(L1_BLOCK_INFO_SLOTS.contains(&L1_OVERHEAD_SLOT));
        assert!(L1_BLOCK_INFO_SLOTS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// A held contract is answered as loaded and unchanged, with every slot of the set as an
    /// unchanged entry holding the chain's value — the overhead among them — whether the scalars
    /// are set or empty.
    #[test]
    fn test_a_held_contract_is_read_as_unchanged_entries_whatever_the_scalars_hold() {
        for scalars in [scalars(), U256::ZERO] {
            let mut db = chain(scalars);
            let state = read_l1_block_info(&mut db).expect("the reads succeed");
            assert_eq!(state.len(), 1, "the L1 block contract alone");
            let account = &state[&L1_BLOCK_CONTRACT];
            assert!(!account.is_touched(), "read, not written");
            assert!(!account.is_created());
            assert!(!account.is_loaded_as_not_existing());
            assert_eq!(account.info.code_hash, alloy_primitives::keccak256([0x00]));
            assert_eq!(account.storage.len(), L1_BLOCK_INFO_SLOTS.len(), "the set, no more");
            for (slot, value) in values(scalars) {
                let entry = account.storage.get(&slot).expect("in the set");
                assert_eq!(entry.original_value, value, "{slot}");
                assert!(!entry.is_changed(), "{slot} is unchanged");
            }
            let overhead = &account.storage[&L1_OVERHEAD_SLOT];
            assert_eq!(overhead.original_value, U256::from(1_005), "scalars {scalars}");
        }
    }

    /// A chain without the contract answers it as not existing and reads no slot: a slot read
    /// would fail on this database.
    #[test]
    fn test_an_absent_contract_is_recorded_absent_and_no_slot_is_read() {
        for slot in L1_BLOCK_INFO_SLOTS {
            let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
            db.fail_on_storage = Some((L1_BLOCK_CONTRACT, slot));
            let state = read_l1_block_info(&mut db).expect("no slot is read");
            let account = &state[&L1_BLOCK_CONTRACT];
            assert!(account.is_loaded_as_not_existing());
            assert!(!account.is_touched());
            assert!(account.storage.is_empty());
        }
    }

    /// A read the database cannot serve is the database's error: the account, or any slot of the
    /// set — the overhead too, though the scalars are set.
    #[test]
    fn test_a_database_error_is_returned() {
        let mut db = ErrorInjectingDatabase::new(chain(scalars()));
        db.fail_on_account = Some(L1_BLOCK_CONTRACT);
        assert!(read_l1_block_info(&mut db).is_err());

        for slot in L1_BLOCK_INFO_SLOTS {
            let mut db = ErrorInjectingDatabase::new(chain(scalars()));
            db.fail_on_storage = Some((L1_BLOCK_CONTRACT, slot));
            assert!(read_l1_block_info(&mut db).is_err(), "slot {slot} of the set");
        }
    }
}
