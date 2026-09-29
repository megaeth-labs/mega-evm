//! The L1 block info a block's transactions are priced against, read before the block's
//! transactions and handed to the pre-block observer.
//!
//! A non-deposit transaction is priced against the L1 block contract: op-revm reads the L1 base
//! fee, the Ecotone blob base fee and fee scalars, the overhead when the scalars are empty and
//! the operator fee scalars once per block, and block execution reads the DA footprint gas scalar
//! before every non-deposit transaction. Both read the block's state directly, not through a
//! transaction's journal, so the account and the slots they read land in no transaction's
//! returned state. A stateless witness built from the returned states alone would miss them, and
//! a validator's database that answers a missing slot with zero would price the L1 fee, the
//! operator fee and the footprint differently without an error.
//!
//! [`read_l1_block_info`] reads the same account and slots before the block's transactions and
//! hands them back as read-only entries, as the reads of the `SequencerRegistry`'s pending changes
//! are handed back, so the pre-block states carry them and a witness built from the states is
//! complete. It reads exactly what the pricing reads, no more: a node that replays a block from a
//! recording of the block's reads must find every key of the entry in it. The read loads the
//! account and its slots into the block's state cache and changes nothing: an untouched entry is
//! not committed, and the transactions' own reads find the same values, or the ones the block's
//! L1 attributes deposit wrote over them.

use alloy_primitives::U256;
use op_revm::constants::{
    BASE_FEE_SCALAR_OFFSET, BLOB_BASE_FEE_SCALAR_OFFSET, ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT, EMPTY_SCALARS, L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT,
    L1_OVERHEAD_SLOT, OPERATOR_FEE_SCALARS_SLOT,
};
use revm::{
    state::{Account, EvmState, EvmStorageSlot, TransactionId},
    Database,
};

/// The slots of the L1 block contract a block's transactions are always priced against, in
/// ascending order: the L1 base fee (1), the Ecotone fee scalars (3), the Ecotone blob base fee
/// (7) and the operator fee scalars (8), which is also the slot of the DA footprint gas scalar.
///
/// The L1 fee overhead ([`L1_OVERHEAD_SLOT`], 5) is read beside them only when the Ecotone
/// scalars are empty ([`ecotone_scalars_are_empty`]), as the pricing reads it.
pub const L1_BLOCK_INFO_SLOTS: [U256; 4] = [
    L1_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT,
    ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    OPERATOR_FEE_SCALARS_SLOT,
];

/// Whether the Ecotone fee scalars word `scalars` is empty, in which case the pricing falls back
/// to the Bedrock cost function and reads the L1 fee overhead: the blob base fee scalar is zero
/// and the eight bytes of the two scalars are zero, as the pricing decides it.
pub fn ecotone_scalars_are_empty(scalars: U256) -> bool {
    let word = scalars.to_be_bytes::<32>();
    let blob_base_fee_scalar =
        U256::from_be_slice(&word[BLOB_BASE_FEE_SCALAR_OFFSET..BLOB_BASE_FEE_SCALAR_OFFSET + 4]);
    blob_base_fee_scalar.is_zero() &&
        word[BASE_FEE_SCALAR_OFFSET..BLOB_BASE_FEE_SCALAR_OFFSET + 4] == EMPTY_SCALARS
}

/// Reads the L1 block contract's account and the slots the pricing reads — [`L1_BLOCK_INFO_SLOTS`],
/// and the L1 fee overhead when the Ecotone scalars are empty — from `db` and answers them as a
/// read-only state: the account as loaded, neither touched nor created, with each slot as an
/// unchanged entry; or, for a chain that does not hold the contract, the account recorded as not
/// existing and no slot read, since the slots of an absent account are zero without a read.
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
    let mut read = |account: &mut Account, slot: U256| -> Result<U256, DB::Error> {
        let value = db.storage(L1_BLOCK_CONTRACT, slot)?;
        account.storage.insert(slot, EvmStorageSlot::new(value, TransactionId::ZERO));
        Ok(value)
    };
    let mut scalars = U256::ZERO;
    for slot in L1_BLOCK_INFO_SLOTS {
        let value = read(&mut account, slot)?;
        if slot == ECOTONE_L1_FEE_SCALARS_SLOT {
            scalars = value;
        }
    }
    if ecotone_scalars_are_empty(scalars) {
        read(&mut account, L1_OVERHEAD_SLOT)?;
    }
    Ok(EvmState::from_iter([(L1_BLOCK_CONTRACT, account)]))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use op_revm::constants::DA_FOOTPRINT_GAS_SCALAR_SLOT;

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

    /// The value the chain holds in every slot of the set: a distinct non-zero value, and the
    /// scalars word at the scalars slot, so the overhead is not read.
    fn values() -> [(U256, U256); 4] {
        let mut values = L1_BLOCK_INFO_SLOTS.map(|slot| (slot, slot + U256::from(1_000)));
        values[1] = (ECOTONE_L1_FEE_SCALARS_SLOT, scalars());
        values
    }

    /// A chain holding the L1 block contract with [`values`] and an overhead.
    fn chain() -> MemoryDatabase {
        let mut db = MemoryDatabase::default();
        db.set_account_code(L1_BLOCK_CONTRACT, Bytes::from(vec![0x00]));
        for (slot, value) in values() {
            db.set_account_storage(L1_BLOCK_CONTRACT, slot, value);
        }
        db.set_account_storage(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT, U256::from(188));
        db
    }

    /// The set names the footprint slot, holds no slot twice, is ascending, and leaves the
    /// overhead to the scalars.
    #[test]
    fn test_the_slots_are_distinct_ascending_and_hold_the_footprint_slot() {
        assert!(L1_BLOCK_INFO_SLOTS.contains(&DA_FOOTPRINT_GAS_SCALAR_SLOT));
        assert!(!L1_BLOCK_INFO_SLOTS.contains(&L1_OVERHEAD_SLOT));
        assert!(L1_BLOCK_INFO_SLOTS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// The scalars are empty when the blob base fee scalar and the eight bytes of the two
    /// scalars are zero, whatever the rest of the word holds; a scalar in either place fills them.
    #[test]
    fn test_the_scalars_are_empty_only_without_either_scalar() {
        assert!(ecotone_scalars_are_empty(U256::ZERO));
        let mut elsewhere = [0_u8; 32];
        elsewhere[0] = 1;
        elsewhere[31] = 1;
        assert!(ecotone_scalars_are_empty(U256::from_be_bytes(elsewhere)), "outside both");
        let mut base = [0_u8; 32];
        base[BASE_FEE_SCALAR_OFFSET + 3] = 1;
        assert!(!ecotone_scalars_are_empty(U256::from_be_bytes(base)), "a base fee scalar");
        let mut blob = [0_u8; 32];
        blob[BLOB_BASE_FEE_SCALAR_OFFSET + 3] = 1;
        assert!(!ecotone_scalars_are_empty(U256::from_be_bytes(blob)), "a blob base fee scalar");
    }

    /// A held contract is answered as loaded and unchanged, with every slot of the set as an
    /// unchanged entry holding the chain's value, and no overhead while the scalars are set.
    #[test]
    fn test_a_held_contract_is_read_as_unchanged_entries() {
        let mut db = chain();
        let state = read_l1_block_info(&mut db).expect("the reads succeed");
        assert_eq!(state.len(), 1, "the L1 block contract alone");
        let account = &state[&L1_BLOCK_CONTRACT];
        assert!(!account.is_touched(), "read, not written");
        assert!(!account.is_created());
        assert!(!account.is_loaded_as_not_existing());
        assert_eq!(account.info.code_hash, alloy_primitives::keccak256([0x00]));
        assert_eq!(account.storage.len(), L1_BLOCK_INFO_SLOTS.len(), "no overhead");
        for (slot, value) in values() {
            let entry = account.storage.get(&slot).expect("in the set");
            assert_eq!(entry.original_value, value, "{slot}");
            assert!(!entry.is_changed(), "{slot} is unchanged");
        }
        assert!(!account.storage.contains_key(&L1_OVERHEAD_SLOT));
    }

    /// With the Ecotone scalars empty the overhead is read too, as the pricing reads it; a
    /// database that cannot serve it fails the read only then.
    #[test]
    fn test_the_overhead_is_read_only_when_the_scalars_are_empty() {
        let mut db = chain();
        db.set_account_storage(L1_BLOCK_CONTRACT, ECOTONE_L1_FEE_SCALARS_SLOT, U256::ZERO);
        let state = read_l1_block_info(&mut db).expect("the reads succeed");
        let account = &state[&L1_BLOCK_CONTRACT];
        assert_eq!(account.storage.len(), L1_BLOCK_INFO_SLOTS.len() + 1);
        let overhead = account.storage.get(&L1_OVERHEAD_SLOT).expect("read with empty scalars");
        assert_eq!(overhead.original_value, U256::from(188));
        assert!(!overhead.is_changed());

        let mut failing = ErrorInjectingDatabase::new(chain());
        failing.fail_on_storage = Some((L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT));
        assert!(read_l1_block_info(&mut failing).is_ok(), "not read while the scalars are set");
    }

    /// A chain without the contract answers it as not existing and reads no slot: a slot read
    /// would fail on this database.
    #[test]
    fn test_an_absent_contract_is_recorded_absent_and_no_slot_is_read() {
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        db.fail_on_storage = Some((L1_BLOCK_CONTRACT, L1_BASE_FEE_SLOT));
        let state = read_l1_block_info(&mut db).expect("no slot is read");
        let account = &state[&L1_BLOCK_CONTRACT];
        assert!(account.is_loaded_as_not_existing());
        assert!(!account.is_touched());
        assert!(account.storage.is_empty());
    }

    /// A read the database cannot serve is the database's error.
    #[test]
    fn test_a_database_error_is_returned() {
        let mut db = ErrorInjectingDatabase::new(chain());
        db.fail_on_account = Some(L1_BLOCK_CONTRACT);
        assert!(read_l1_block_info(&mut db).is_err());

        let mut db = ErrorInjectingDatabase::new(chain());
        db.fail_on_storage = Some((L1_BLOCK_CONTRACT, OPERATOR_FEE_SCALARS_SLOT));
        assert!(read_l1_block_info(&mut db).is_err(), "a slot of the set");
    }
}
