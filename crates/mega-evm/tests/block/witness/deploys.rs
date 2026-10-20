//! Satin's activation block, its check of the predeploys, and the EIP-7997 factory through the
//! harness.

use alloy_primitives::{keccak256, Address, Bytes, U256};
use mega_evm::{
    system::{system_contract_specs, CREATE2_FACTORY_ADDRESS},
    PreBlockStateSource,
};
use revm::{
    bytecode::opcode::{CODECOPY, PUSH0, RETURN},
    state::Account,
};

use super::{
    basics::{slot, slot_writer, write_gas},
    harness::{call, deposit, Case, Run},
    state::creation_gas,
};
use crate::common::{self, CALLER, CONTRACT};

/// Init code that deploys a runtime of five bytes that stops.
fn init_code() -> Bytes {
    let runtime = [0x00_u8; 5];
    let tail = 11_u16.to_be_bytes();
    let mut code = vec![0x60, 5, 0x61, tail[0], tail[1], PUSH0, CODECOPY, 0x60, 5, PUSH0, RETURN];
    code.extend_from_slice(&runtime);
    code.into()
}

/// The state the activation block's check handed over for the predeploy at `address`.
fn check_state(run: &Run, address: Address) -> &Account {
    let (_, state) = run
        .pre_block
        .iter()
        .find(|(source, _)| *source == PreBlockStateSource::SystemContract(address))
        .unwrap_or_else(|| panic!("{address} has a check state"));
    &state[&address]
}

/// A `CREATE2` through the EIP-7997 factory: the factory's code travels by hash, the created
/// account is priced through SALT, and the deployment replays.
#[test]
fn test_a_deployment_through_the_factory_replays() {
    let mut input = vec![0x11_u8; 32];
    input.extend_from_slice(&init_code());
    let replay = Case::new("factory", common::database())
        .tx(call(0, CREATE2_FACTORY_ADDRESS, input.into(), creation_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    let output = run.tx(0).result.output().cloned().unwrap_or_default();
    let created = alloy_primitives::Address::from_slice(&output[output.len() - 20..]);
    assert!(run.tx(0).state[&created].info.code.as_ref().is_some_and(|code| !code.is_empty()));
    assert!(run.record.codes.contains_key(&mega_evm::system::CREATE2_FACTORY_CODE_HASH));
    if !common::state_is_free() {
        assert!(!run.bucket_ids.is_empty(), "the created account was priced");
    }
}

/// Satin's activation block over a chain shaped as Rex6 leaves it replays on both witnesses: it
/// admits deposits alone, so the deposit runs and the user transaction is refused, and its check
/// reads the seven predeploys, each with the chain's code hash, loads none of their code, and hands
/// each over as a read-only entry. The witness built from those entries holds, for every
/// predeploy, the code the chain held, under its hash: no check loaded it, so it is there because
/// the account was named, resolved against the chain.
///
/// An ordinary block over the same chain reads none of the predeploys the transactions do not
/// reach: no check state, and no record of their accounts.
///
/// Rule [S22.4]. Expected values `independent`: the code is the bytecode the test put in the chain,
/// and its hash is computed here.
#[test]
fn test_an_activation_block_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let activation = || {
        Case::new("activation block", db.clone())
            .ctx(
                common::unlimited_ctx()
                    .with_no_user_tx_activation_block(true)
                    .with_satin_activation_block(true),
            )
            .tx(deposit(
                CALLER,
                alloy_primitives::TxKind::Call(CONTRACT),
                0,
                U256::ZERO,
                slot(1),
                write_gas(),
            ))
            .tx(call(0, CONTRACT, slot(2), write_gas()))
    };
    let replay = activation().run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert!(run.refusal(1).contains("activation block"), "{}", run.refusal(1));
    let witness = activation().channel_witness(run);
    for spec in system_contract_specs() {
        let hash = keccak256(&spec.code);
        let info = run.record.accounts.get(&spec.address).expect("read").as_ref().expect("held");
        assert_eq!(info.code_hash, hash, "{} was read with the chain's code hash", spec.address);
        let entry = check_state(run, spec.address);
        assert!(!entry.is_touched() && !entry.is_created(), "{} was only read", spec.address);
        assert!(!run.record.codes.contains_key(&hash), "{}: the check loads no code", spec.address);
        assert!(
            witness.codes.get(&hash).is_some_and(|carried| carried.original_bytes() == spec.code),
            "{}: the witness carries the code the chain held",
            spec.address,
        );
    }

    let replay = Case::new("ordinary block", db).tx(call(0, CONTRACT, slot(2), write_gas())).run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert!(run
        .pre_block
        .iter()
        .all(|(source, _)| !matches!(source, PreBlockStateSource::SystemContract(_))));
    for spec in &system_contract_specs()[1..5] {
        assert!(!run.record.accounts.contains_key(&spec.address), "{} was read", spec.address);
    }
    assert!(!run.record.accounts.contains_key(&CREATE2_FACTORY_ADDRESS), "the factory was read");
}
