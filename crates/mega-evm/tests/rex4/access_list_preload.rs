//! The access-list entries a transaction reads before its first frame.
//!
//! The deployed implementation loaded every access-list entry that lists storage keys — the
//! account and each listed slot — before the first frame, so they are in the transaction's read
//! set, the accounts and slots of the state it hands back, from which a node builds the stateless
//! witness, whether or not execution touches them. An entry listing no keys was only marked warm.
//! revm 40 marks every entry warm and reads none of them up front; `MegaETH` loads the ones with
//! keys anyway. Every expectation here was measured on the deployed implementation.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
    context_interface::transaction::{AccessList, AccessListItem},
    database::AccountState,
    state::{Bytecode, EvmState},
};

const CALLER: Address = address!("0000000000000000000000000000000000450000");
/// The transaction's target: stores what an `SLOAD` of slot 7 costs in its slot 0.
const TARGET: Address = address!("0000000000000000000000000000000000450001");
/// An account only the access list names, with storage keys.
const LISTED: Address = address!("00000000000000000000000000000000004500bb");
/// An account only the access list names, without storage keys.
const LISTED_BARE: Address = address!("00000000000000000000000000000000004500cc");
/// A funded account only the access list names, without storage keys.
const LISTED_BARE_FUNDED: Address = address!("00000000000000000000000000000000004500dd");

/// What a warm `SLOAD` costs between two `GAS` readings: `PUSH1`, `SLOAD`, `POP`, `GAS`.
const WARM_SLOAD_PROBE: u64 = 3 + 100 + 2 + 2;

fn database() -> MemoryDatabase {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
        .account_balance(LISTED_BARE_FUNDED, U256::from(1));
    // GAS; PUSH1 7; SLOAD; POP; GAS; SWAP1; SUB; PUSH1 0; SSTORE; STOP
    let code = BytecodeBuilder::default()
        .append(GAS)
        .push_number(7_u64)
        .append(SLOAD)
        .append(POP)
        .append(GAS)
        .append(SWAP1)
        .append(SUB)
        .push_number(0_u64)
        .append(SSTORE)
        .stop()
        .build();
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    let account = db.load_account(TARGET).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
    db
}

/// Runs a call from `CALLER` to `TARGET` with the fixture's access list and returns the
/// transaction's state, or the error it failed with.
fn run<DB: alloy_evm::Database>(db: DB, spec: MegaSpecId) -> Result<EvmState, String> {
    let entry = |address: Address, keys: &[u64]| AccessListItem {
        address,
        storage_keys: keys.iter().map(|&k| B256::from(U256::from(k))).collect(),
    };
    let access_list = AccessList(vec![
        entry(TARGET, &[7]),
        entry(LISTED, &[9, 10]),
        entry(LISTED_BARE, &[]),
        entry(LISTED_BARE_FUNDED, &[]),
    ]);
    let mut context = MegaContext::new(db, spec).with_block(BlockEnv::default());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default()
            .caller(CALLER)
            .call(TARGET)
            .gas_limit(5_000_000)
            .access_list(access_list)
            .tx_type(Some(1))
            .build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx).map_err(|e| format!("{e:?}"))?;
    assert!(outcome.result.is_success(), "{spec:?}: {:?}", outcome.result);
    Ok(outcome.state)
}

/// An access-list entry that lists storage keys is read before the first frame — the account and
/// each listed slot — whether or not the transaction touches it; an entry without keys is not read.
/// A listed slot is warm when the transaction reads it.
#[test]
fn test_access_list_entries_with_storage_keys_are_read_before_execution() {
    for spec in [
        MegaSpecId::EQUIVALENCE,
        MegaSpecId::MINI_REX,
        MegaSpecId::REX,
        MegaSpecId::REX2,
        MegaSpecId::REX3,
        MegaSpecId::REX4,
        MegaSpecId::REX5,
        MegaSpecId::REX6,
    ] {
        let state = run(database(), spec).unwrap();
        for key in [9, 10] {
            assert!(
                state[&LISTED].storage.contains_key(&U256::from(key)),
                "{spec:?}: listed slot {key} is read",
            );
        }
        for bare in [LISTED_BARE, LISTED_BARE_FUNDED] {
            assert!(!state.contains_key(&bare), "{spec:?}: an entry without keys is not read");
        }
        assert_eq!(
            state[&TARGET].storage[&U256::ZERO].present_value,
            U256::from(WARM_SLOAD_PROBE),
            "{spec:?}: a listed slot is warm",
        );

        let mut failing = ErrorInjectingDatabase::new(database());
        failing.fail_on_account = Some(LISTED);
        assert!(
            run(failing, spec).is_err(),
            "{spec:?}: a database error on a listed account is the transaction's",
        );
        let mut failing = ErrorInjectingDatabase::new(database());
        failing.fail_on_storage = Some((LISTED, U256::from(10)));
        assert!(
            run(failing, spec).is_err(),
            "{spec:?}: a database error on a listed slot is the transaction's",
        );
        let mut failing = ErrorInjectingDatabase::new(database());
        failing.fail_on_account = Some(LISTED_BARE_FUNDED);
        assert!(run(failing, spec).is_ok(), "{spec:?}: an entry without keys is never read");
    }
}
