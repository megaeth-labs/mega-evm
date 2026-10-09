//! A `SELFDESTRUCT` that cannot afford its charge keeps the frame's remaining gas.
//!
//! From Rex5 a `SELFDESTRUCT` that sends value to an empty beneficiary records the new account's
//! resource usage before the opcode runs. When the opcode then runs out of gas, the frame-end
//! check sees that usage over the frame's budget and turns the halt into a revert that hands the
//! frame's remaining gas back to its caller. The deployed implementation charged the opcode's
//! whole cost in one step and, when that charge failed, left the frame's gas untouched; revm 40
//! zeroes an out-of-gas frame unless the handler keeps it. Every expectation here was measured on
//! the deployed implementation.
//!
//! The program recurses into itself `depth` times and, on the way back up, every level runs
//! `SELFDESTRUCT` to an empty account. Only the first one to succeed moves value, so the deep
//! levels that cannot afford the new-account charge are exactly the frames under test.

use alloy_primitives::{address, hex, Address, Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionNew as _,
};
use revm::{
    context::{tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::Bytecode,
};

const SENDER: Address = address!("0000000000000000000000000000000000200000");
const RECURSER: Address = address!("0000000000000000000000000000000000200001");
const BENEFICIARY: Address = address!("00000000000000000000000000000000000000c0");

/// `n = calldata[0]; if n != 0 { CALL(gas, self, 0, [n - 1]) } SELFDESTRUCT(0x..06)`.
fn recurser_code() -> Bytes {
    hex::decode(format!(
        "5f35801561002c57600190035f525f5f60205f5f73{}5af1505b730000000000000000000000000000000000000006ff",
        hex::encode(RECURSER)
    ))
    .unwrap()
    .into()
}

/// Runs the recursion to `depth` and returns `(gas_used, compute_gas_used)`.
fn run(spec: MegaSpecId, depth: u64) -> (u64, u64) {
    let mut db =
        MemoryDatabase::default().account_balance(SENDER, U256::from(10).pow(U256::from(30)));
    let code = Bytecode::new_raw(recurser_code());
    let code_hash = code.hash_slow();
    let account = db.load_account(RECURSER).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.info.balance = U256::from(1_000_000u64);
    account.account_state = AccountState::None;

    let block = BlockEnv {
        number: U256::from(100),
        timestamp: U256::from(1_700_000_000u64),
        gas_limit: 30_000_000,
        beneficiary: BENEFICIARY,
        ..Default::default()
    };
    let mut context = MegaContext::new(&mut db, spec).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(SENDER)
            .call(RECURSER)
            .data(Bytes::from(U256::from(depth).to_be_bytes::<32>().to_vec()))
            .gas_limit(20_000_000)
            .gas_price(0)
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = evm.execute_transaction(tx).expect("tx must execute");
    let result = &outcome.result_and_state.result;
    assert!(result.is_success(), "{spec:?} depth {depth}: {result:?}");
    #[allow(deprecated)]
    let gas_used = result.gas_used();
    (gas_used, outcome.compute_gas_used)
}

#[test]
fn test_selfdestruct_out_of_gas_keeps_the_frame_gas_its_caller_gets_back() {
    for spec in [MegaSpecId::REX5, MegaSpecId::REX6] {
        // From depth 195 the deepest levels cannot afford the new-account charge; from 222 the
        // gas their callers get back changes which level first succeeds.
        for (depth, expected) in [
            (195, (2_350_573, 1_222_153)),
            (222, (1_509_354, 1_468_954)),
            (225, (1_509_843, 1_469_443)),
        ] {
            assert_eq!(run(spec, depth), expected, "{spec:?}: recursion depth {depth}");
        }
    }
}
