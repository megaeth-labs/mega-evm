//! System transactions and the registry's rotation through the harness: a block that applies a
//! system-address change before its transactions, then runs a system transaction from the new
//! address and refuses one from the old, replays from its witness.

use alloy_primitives::{address, keccak256, Address, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        storage_slots::{
            CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        },
        IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::MemoryDatabase,
    PreBlockStateSource,
};

use super::harness::{legacy_from, Case};
use crate::common::{self, BLOCK_NUMBER};

/// The system address a pending change rotates to.
const NEXT_SYSTEM_ADDRESS: Address = address!("0x3000000000000000000000000000000000000003");

fn word(address: Address) -> U256 {
    U256::from_be_bytes(address.into_word().0)
}

/// A database holding a deployed registry naming [`MEGA_SYSTEM_ADDRESS`], with a system-address
/// change to [`NEXT_SYSTEM_ADDRESS`] due at `activation`.
fn registry_rotating_at(activation: u64) -> MemoryDatabase {
    let mut db = common::database();
    db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE);
    for (slot, value) in [
        (CURRENT_SYSTEM_ADDRESS, word(MEGA_SYSTEM_ADDRESS)),
        (CURRENT_SEQUENCER, word(common::SEQUENCER)),
        (PENDING_SYSTEM_ADDRESS, word(NEXT_SYSTEM_ADDRESS)),
        (SYSTEM_ADDRESS_ACTIVATION_BLOCK, U256::from(activation)),
    ] {
        db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, slot, value);
    }
    db
}

/// A legacy call from `sender` to the Oracle's `getSlot(0)`: a system transaction when `sender`
/// is the live system address, an ordinary one — from an account holding nothing — otherwise.
fn oracle_call_from(sender: Address) -> super::harness::Tx {
    let input = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let gas =
        1_000_000 + common::new_account_state_gas() + common::body_history(input.len() as u64);
    legacy_from(
        sender,
        0,
        alloy_primitives::TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        U256::ZERO,
        input.into(),
        gas,
    )
}

/// The rotation is applied before the block's transactions, the new address's transaction runs
/// as the protocol's own, the old address's is refused, and the whole block replays: the
/// pending slots the decision read are in the record, and the live address the transaction read
/// is the one the block's own pre-block call wrote.
#[test]
fn test_a_rotation_and_a_system_transaction_replay() {
    let replay = Case::new("rotation", registry_rotating_at(BLOCK_NUMBER))
        .tx(oracle_call_from(NEXT_SYSTEM_ADDRESS))
        .tx(oracle_call_from(MEGA_SYSTEM_ADDRESS))
        .run();
    let run = &replay.recorded;
    assert!(
        run.pre_block.iter().any(|(source, _)| *source == PreBlockStateSource::ApplyPendingChanges),
        "the change was applied"
    );
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(run.tx(0).gas.history, 0, "the protocol's own transaction pays no history");
    assert!(run.refusal(1).contains("lack of funds"), "{}", run.refusal(1));
    assert_eq!(run.receipts.len(), 1);
    assert!(run.bucket_ids.is_empty(), "a system transaction prices at the minimum bucket");

    let registry_slots =
        |slot| run.record.storage.contains_key(&(SEQUENCER_REGISTRY_ADDRESS, slot));
    assert!(registry_slots(PENDING_SYSTEM_ADDRESS) && registry_slots(PENDING_SEQUENCER));
    assert!(registry_slots(SYSTEM_ADDRESS_ACTIVATION_BLOCK), "the due role's activation block");
    assert_eq!(
        run.record.storage.get(&(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS)),
        Some(&word(MEGA_SYSTEM_ADDRESS)),
        "the chain's live address, read by the call that rotated it; the transaction read the \
         rotated one from the block's own writes"
    );
}

/// A system transaction in a block with nothing due reads the live address out of the witness:
/// the registry's account and its one slot are in the record and in the transaction's own state,
/// and the registry's code is not: no read the block made loaded it, so a witness need not carry
/// it.
///
/// Rule [S17.4]. Expected values `independent`: the registry's code hash is computed here from the
/// bytecode the test put in the chain.
#[test]
fn test_a_system_transaction_reads_the_live_address_from_the_witness() {
    let replay = Case::new("system transaction", registry_rotating_at(BLOCK_NUMBER + 1))
        .tx(oracle_call_from(MEGA_SYSTEM_ADDRESS))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert!(run.record.storage.contains_key(&(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS)));
    let registry = run.tx(0).state.get(&SEQUENCER_REGISTRY_ADDRESS).expect("the registry is read");
    assert!(
        registry.storage.contains_key(&CURRENT_SYSTEM_ADDRESS),
        "the slot is in the transaction's state"
    );
    assert!(
        !run.record.codes.contains_key(&keccak256(SEQUENCER_REGISTRY_CODE)),
        "the registry's code was never loaded"
    );
    assert!(!run
        .pre_block
        .iter()
        .any(|(source, _)| *source == PreBlockStateSource::ApplyPendingChanges));
}
