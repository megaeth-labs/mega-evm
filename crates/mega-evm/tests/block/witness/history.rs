//! EIP-2935 and `BLOCKHASH` through the harness: the pre-block call writes the parent hash into
//! the history contract, a transaction reads two hashes inside the window and two outside it, and
//! the record and the executor's export hold exactly the two the database served.

use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_primitives::{Bytes, B256, U256};
use mega_evm::{test_utils::BytecodeBuilder, BlockLimits, MegaBlockExecutionCtx};
use revm::bytecode::opcode::{BLOCKHASH, POP};

use super::harness::{call, Case};
use crate::common::{self, BLOCK_NUMBER, CONTRACT};

/// Code that reads the hashes of the four `numbers` and discards them.
fn reads(numbers: [u64; 4]) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for number in numbers {
        code = code.push_number(number).append(BLOCKHASH).append(POP);
    }
    code.stop().build()
}

/// The two reads inside the window are served and recorded; the current block and one 300
/// blocks back are answered zero without a read; the EIP-2935 call's write is in the state
/// changes.
#[test]
fn test_block_hash_reads_replay() {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(
        CONTRACT,
        reads([BLOCK_NUMBER - 1, BLOCK_NUMBER - 2, BLOCK_NUMBER, BLOCK_NUMBER - 300]),
    );
    let parent = B256::repeat_byte(0x29);
    let replay = Case::new("block hashes", db)
        .ctx(MegaBlockExecutionCtx::new(
            parent,
            Some(B256::ZERO),
            Bytes::new(),
            BlockLimits::no_limits(),
        ))
        .tx(call(0, CONTRACT, Bytes::new(), common::empty_call_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(
        run.record.block_hashes.keys().copied().collect::<Vec<_>>(),
        vec![BLOCK_NUMBER - 2, BLOCK_NUMBER - 1],
        "only the reads inside the window reach the database"
    );
    assert_eq!(run.block_hashes, run.record.block_hashes);
    let ring_slot =
        U256::from(BLOCK_NUMBER - 1) % U256::from(alloy_eips::eip2935::HISTORY_SERVE_WINDOW);
    let written = run
        .bundle
        .state
        .get(&HISTORY_STORAGE_ADDRESS)
        .and_then(|account| account.storage.get(&ring_slot))
        .map(|slot| slot.present_value);
    assert_eq!(written, Some(U256::from_be_bytes(parent.0)), "the parent hash was written");
}
