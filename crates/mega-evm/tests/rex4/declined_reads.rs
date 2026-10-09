//! The accounts and storage slots a transaction reads when revm 40 would read fewer.
//!
//! The deployed implementation read an account or a slot before charging for it. revm 40 declines
//! a cold read the frame cannot afford, halting it out of gas first. `MegaETH` makes those reads
//! anyway, so a transaction's read set — the accounts and slots in the state it hands back, from
//! which a node builds the stateless witness — stays what the deployed implementation read.
//! Nothing else changes: an entry read in a frame that then halts is cold again once the frame
//! reverts. Every expectation here was measured on the deployed implementation.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    ExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    OracleEnv, TestExternalEnvs, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{tx::TxEnvBuilder, BlockEnv},
    database::AccountState,
    state::{Bytecode, EvmState},
};

const CALLER: Address = address!("0000000000000000000000000000000000440000");
const OUTER: Address = address!("0000000000000000000000000000000000440001");
const INNER: Address = address!("0000000000000000000000000000000000440002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000440099");
/// A funded contract that is not pre-warmed: the account the declined reads target.
const TARGET: Address = address!("00000000000000000000000000000000004400aa");

/// The specs whose read sets match the deployed implementation's.
const SPECS: [MegaSpecId; 3] = [MegaSpecId::REX4, MegaSpecId::REX5, MegaSpecId::REX6];

/// The slot `INNER` reads.
const SLOT: u64 = 5;
/// What a cold `BALANCE` costs between two `GAS` readings: `PUSH20`, `BALANCE`, `POP`, `GAS`.
const COLD_BALANCE_PROBE: u64 = 3 + 2_600 + 2 + 2;
/// What a cold `SLOAD` costs between two `GAS` readings: `PUSH1`, `SLOAD`, `POP`, `GAS`.
const COLD_SLOAD_PROBE: u64 = 3 + 2_100 + 2 + 2;

/// Appends `GAS; <body>; GAS; SWAP1; SUB; PUSH1 0; SSTORE`: stores what `body` cost, plus the
/// second `GAS`, in slot 0 of the executing account.
fn measure(
    b: BytecodeBuilder,
    body: impl FnOnce(BytecodeBuilder) -> BytecodeBuilder,
) -> BytecodeBuilder {
    body(b.append(GAS)).append(GAS).append(SWAP1).append(SUB).push_number(0_u64).append(SSTORE)
}

/// Appends `CALL(gas, to, 0, 0, in_len, 0, 0); POP`.
fn call(b: BytecodeBuilder, to: Address, gas: u64, in_len: u64) -> BytecodeBuilder {
    b.push_number(0_u64)
        .push_number(0_u64)
        .push_number(in_len)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(to)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// `OUTER`: hand `INNER` `budget` gas; then, with `probe`, store what a `BALANCE` of `TARGET`
/// costs in slot 0.
fn outer_account_probe(budget: u64, probe: bool) -> Bytes {
    let mut b = call(BytecodeBuilder::default(), INNER, budget, 0);
    if probe {
        b = measure(b, |b| b.push_address(TARGET).append(BALANCE).append(POP));
    }
    b.stop().build()
}

fn install(db: &mut MemoryDatabase, address: Address, code: Bytes) {
    let code = Bytecode::new_raw(code);
    let code_hash = code.hash_slow();
    let account = db.load_account(address).expect("in-memory account load");
    account.info.code = Some(code);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

/// The fixture every case starts from: a funded caller, `OUTER`, `INNER`, and a funded `TARGET`
/// with code.
fn database(outer: Bytes, inner: Bytes) -> MemoryDatabase {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
        .account_balance(TARGET, U256::from(1))
        .account_balance(BENEFICIARY, U256::from(1));
    install(&mut db, TARGET, Bytes::from_static(&[STOP]));
    install(&mut db, OUTER, outer);
    install(&mut db, INNER, inner);
    db
}

/// Runs a call from `CALLER` to `OUTER` and returns the transaction's state, or the error it failed
/// with.
fn run<DB, Envs>(db: DB, envs: ExternalEnvs<Envs>, spec: MegaSpecId) -> Result<EvmState, String>
where
    DB: alloy_evm::Database,
    Envs: mega_evm::ExternalEnvTypes,
{
    let mut context = MegaContext::new(db, spec)
        .with_block(BlockEnv { beneficiary: BENEFICIARY, ..Default::default() })
        .with_external_envs(envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(
        TxEnvBuilder::default().caller(CALLER).call(OUTER).gas_limit(5_000_000).build_fill(),
    );
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = alloy_evm::Evm::transact_raw(&mut evm, tx).map_err(|e| format!("{e:?}"))?;
    assert!(outcome.result.is_success(), "{spec:?}: {:?}", outcome.result);
    Ok(outcome.state)
}

fn empty_envs() -> ExternalEnvs<TestExternalEnvs> {
    TestExternalEnvs::new().into()
}

fn stored(state: &EvmState, address: Address, key: u64) -> U256 {
    state[&address].storage[&U256::from(key)].present_value
}

/// `BALANCE`, `EXTCODESIZE` and `EXTCODEHASH` of a cold account in a frame too poor for the cold
/// access: the deployed opcode read the account and then ran out of gas on the charge.
#[test]
fn test_declined_account_read_is_in_the_read_set_and_left_cold() {
    for spec in SPECS {
        for opcode in [BALANCE, EXTCODESIZE, EXTCODEHASH] {
            let inner = BytecodeBuilder::default()
                .push_address(TARGET)
                .append(opcode)
                .append(POP)
                .stop()
                .build();
            let what = format!("{spec:?}: opcode 0x{opcode:02x}");

            let state =
                run(database(outer_account_probe(1_000, false), inner.clone()), empty_envs(), spec)
                    .unwrap();
            assert!(state.contains_key(&TARGET), "{what}: the declined read is in the read set");

            let state =
                run(database(outer_account_probe(1_000, true), inner.clone()), empty_envs(), spec)
                    .unwrap();
            assert_eq!(
                stored(&state, OUTER, 0),
                U256::from(COLD_BALANCE_PROBE),
                "{what}: the account is cold again after the halting frame",
            );

            let mut db =
                ErrorInjectingDatabase::new(database(outer_account_probe(1_000, false), inner));
            db.fail_on_account = Some(TARGET);
            assert!(
                run(db, empty_envs(), spec).is_err(),
                "{what}: a database error on the declined read is the transaction's",
            );
        }
    }
}

/// `INNER`: with empty calldata, `SLOAD(SLOT)`; otherwise store what that `SLOAD` costs in slot 0.
fn inner_sload() -> Bytes {
    let read = BytecodeBuilder::default().push_number(SLOT).append(SLOAD).append(POP).stop();
    // PUSH1 0, CALLDATALOAD, PUSH1 dest, JUMPI: 6 bytes.
    let dest = 6 + read.len() as u64;
    let b = BytecodeBuilder::default()
        .push_number(0_u8)
        .append(CALLDATALOAD)
        .push_number(dest as u8)
        .append(JUMPI)
        .append_many(read.build_vec())
        .append(JUMPDEST);
    assert_eq!(b.len() as u64, dest + 1, "jump layout");
    measure(b, |b| b.push_number(SLOT).append(SLOAD).append(POP)).stop().build()
}

/// `OUTER`: hand `INNER` `budget` gas with empty calldata; then, with `probe`, call `INNER` again
/// with a non-empty word so it measures its `SLOAD`.
fn outer_sload_probe(budget: u64, probe: bool) -> Bytes {
    let mut b = call(BytecodeBuilder::default(), INNER, budget, 0);
    if probe {
        b = b.push_number(1_u64).push_number(0_u64).append(MSTORE);
        b = call(b, INNER, 100_000, 32);
    }
    b.stop().build()
}

/// An `SLOAD` of a cold slot in a frame too poor for the cold access: the deployed opcode read the
/// slot and then ran out of gas on the charge.
#[test]
fn test_declined_slot_read_is_in_the_read_set_and_left_cold() {
    for spec in SPECS {
        let state =
            run(database(outer_sload_probe(1_000, false), inner_sload()), empty_envs(), spec)
                .unwrap();
        assert!(
            state[&INNER].storage.contains_key(&U256::from(SLOT)),
            "{spec:?}: the declined read is in the read set",
        );

        let state =
            run(database(outer_sload_probe(1_000, true), inner_sload()), empty_envs(), spec)
                .unwrap();
        assert_eq!(
            stored(&state, INNER, 0),
            U256::from(COLD_SLOAD_PROBE),
            "{spec:?}: the slot is cold again after the halting frame",
        );

        let mut db =
            ErrorInjectingDatabase::new(database(outer_sload_probe(1_000, false), inner_sload()));
        db.fail_on_storage = Some((INNER, U256::from(SLOT)));
        assert!(
            run(db, empty_envs(), spec).is_err(),
            "{spec:?}: a database error on the declined read is the transaction's",
        );
    }
}

/// An oracle environment that serves [`ORACLE_VALUE_SLOT`] and records every slot it is asked for.
#[derive(Debug, Clone, Default)]
struct RecordingOracle {
    lookups: Rc<RefCell<Vec<U256>>>,
}

/// The one oracle slot [`RecordingOracle`] has a value for.
const ORACLE_VALUE_SLOT: u64 = 1;

impl OracleEnv for RecordingOracle {
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        self.lookups.borrow_mut().push(slot);
        (slot == U256::from(ORACLE_VALUE_SLOT)).then_some(U256::from(42))
    }
}

/// `OUTER`: `CALL(budget, ORACLE, 0, 0, 32, 0, 0)` with `slot` as calldata; the oracle contract
/// reads the slot its calldata names.
fn outer_oracle(budget: u64, slot: u64) -> Bytes {
    let b = BytecodeBuilder::default().push_number(slot).push_number(0_u64).append(MSTORE);
    call(b, ORACLE_CONTRACT_ADDRESS, budget, 32).stop().build()
}

fn oracle_database(slot: u64) -> MemoryDatabase {
    let mut db = database(outer_oracle(1_000, slot), Bytes::from_static(&[STOP]));
    let oracle = BytecodeBuilder::default()
        .push_number(0_u64)
        .append(CALLDATALOAD)
        .append(SLOAD)
        .append(POP)
        .stop()
        .build();
    install(&mut db, ORACLE_CONTRACT_ADDRESS, oracle);
    db
}

/// An oracle `SLOAD` in a frame too poor for it: the deployed read asked the oracle environment
/// for the slot and, when it had no value, read the slot from state, before the charge ran the
/// frame out of gas. What the environment is asked for is what a sequencer's oracle service
/// records as read.
#[test]
fn test_declined_oracle_read_asks_the_environment_then_reads_state() {
    for spec in SPECS {
        for (slot, read_from_state) in [(ORACLE_VALUE_SLOT, false), (2, true)] {
            let oracle = RecordingOracle::default();
            let envs = ExternalEnvs::<(TestExternalEnvs, RecordingOracle)> {
                salt_env: TestExternalEnvs::new(),
                oracle_env: oracle.clone(),
            };
            let state = run(oracle_database(slot), envs, spec).unwrap();
            assert_eq!(
                *oracle.lookups.borrow(),
                vec![U256::from(slot)],
                "{spec:?}: slot {slot} is asked of the oracle environment",
            );
            assert_eq!(
                state[&ORACLE_CONTRACT_ADDRESS].storage.contains_key(&U256::from(slot)),
                read_from_state,
                "{spec:?}: slot {slot} is read from state only without an environment value",
            );
        }

        let mut db = ErrorInjectingDatabase::new(oracle_database(2));
        db.fail_on_storage = Some((ORACLE_CONTRACT_ADDRESS, U256::from(2)));
        let envs = ExternalEnvs::<(TestExternalEnvs, RecordingOracle)> {
            salt_env: TestExternalEnvs::new(),
            oracle_env: RecordingOracle::default(),
        };
        assert!(
            run(db, envs, spec).is_err(),
            "{spec:?}: a database error on the declined read is the transaction's",
        );
    }
}

/// `INNER`: `SELFDESTRUCT(TARGET)`.
fn inner_selfdestruct() -> Bytes {
    BytecodeBuilder::default().push_address(TARGET).append(SELFDESTRUCT).build()
}

/// A `SELFDESTRUCT` to a cold beneficiary in a frame too poor for the cold access: the deployed
/// opcode read the beneficiary and then ran out of gas on the charge. Before `REX4` the opcode's
/// static gas is charged ahead of its body, so the frame needs it on top.
#[test]
fn test_declined_selfdestruct_beneficiary_read_is_in_the_read_set_and_left_cold() {
    let cases = [
        (MegaSpecId::EQUIVALENCE, 6_000),
        (MegaSpecId::REX3, 6_000),
        (MegaSpecId::REX4, 2_000),
        (MegaSpecId::REX5, 2_000),
        (MegaSpecId::REX6, 2_000),
    ];
    for (spec, budget) in cases {
        let state = run(
            database(outer_account_probe(budget, false), inner_selfdestruct()),
            empty_envs(),
            spec,
        )
        .unwrap();
        assert!(state.contains_key(&TARGET), "{spec:?}: the declined read is in the read set");

        let state = run(
            database(outer_account_probe(budget, true), inner_selfdestruct()),
            empty_envs(),
            spec,
        )
        .unwrap();
        assert_eq!(
            stored(&state, OUTER, 0),
            U256::from(COLD_BALANCE_PROBE),
            "{spec:?}: the beneficiary is cold again after the halting frame",
        );

        let mut db = ErrorInjectingDatabase::new(database(
            outer_account_probe(budget, false),
            inner_selfdestruct(),
        ));
        db.fail_on_account = Some(TARGET);
        assert!(
            run(db, empty_envs(), spec).is_err(),
            "{spec:?}: a database error on the declined read is the transaction's",
        );
    }
}
