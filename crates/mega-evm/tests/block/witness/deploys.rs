//! The pre-block deploys, the EIP-7997 factory and an activation block through the harness.

use alloy_primitives::{Bytes, U256};
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE},
        system_contract_specs, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE,
        CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{BytecodeBuilder, MemoryDatabase},
    PreBlockStateSource,
};
use revm::bytecode::opcode::{CODECOPY, PUSH0, RETURN};

use super::{
    basics::{slot, slot_writer, write_gas},
    harness::{call, deposit, Case},
};
use crate::common::{self, CALLER, CONTRACT};

/// A database holding every system contract and the factory, as a chain does after its first
/// Satin block.
fn chain_with_contracts() -> MemoryDatabase {
    let db = common::database()
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE)
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
        .account_code(CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_CODE);
    db.account_nonce(CREATE2_FACTORY_ADDRESS, 1)
}

/// Init code that deploys a runtime of five bytes that stops.
fn init_code() -> Bytes {
    let runtime = [0x00_u8; 5];
    let tail = 11_u16.to_be_bytes();
    let mut code = vec![0x60, 5, 0x61, tail[0], tail[1], PUSH0, CODECOPY, 0x60, 5, PUSH0, RETURN];
    code.extend_from_slice(&runtime);
    code.into()
}

/// The first Satin block over a chain without the contracts: every deploy asks about an absent
/// account, which the record holds as absent, and creates it; a validator given those absences
/// deploys the same code.
#[test]
fn test_the_first_block_deploys_from_absent_accounts() {
    let replay = Case::new("first block", common::database()).run();
    let run = &replay.recorded;
    for spec in system_contract_specs(&common::registry_config()) {
        assert_eq!(run.record.accounts.get(&spec.address), Some(&None), "{}", spec.address);
        let created = run
            .pre_block
            .iter()
            .find(|(source, _)| *source == PreBlockStateSource::SystemContract(spec.address))
            .map(|(_, state)| state[&spec.address].is_created());
        assert_eq!(created, Some(true), "{} was created", spec.address);
    }
}

/// A later block over a chain holding the contracts: every deploy reads its account, which the
/// record holds with its code hash and without its code, and creates nothing.
#[test]
fn test_a_later_block_reads_the_deployed_contracts() {
    let replay = Case::new("later block", chain_with_contracts()).run();
    let run = &replay.recorded;
    for spec in system_contract_specs(&common::registry_config()) {
        let info = run.record.accounts.get(&spec.address).expect("read").as_ref().expect("held");
        assert_eq!(info.code_hash, spec.code_hash);
        assert!(info.code.is_none(), "code travels by hash, not with the account");
        let created = run
            .pre_block
            .iter()
            .find(|(source, _)| *source == PreBlockStateSource::SystemContract(spec.address))
            .map(|(_, state)| state[&spec.address].is_created());
        assert_eq!(created, Some(false), "{} was read", spec.address);
    }
    assert!(
        !run.record.codes.contains_key(&mega_evm::system::CREATE2_FACTORY_CODE_HASH),
        "no deploy loads code"
    );
}

/// A `CREATE2` through the EIP-7997 factory: the factory's code travels by hash, the created
/// account is priced through SALT, and the deployment replays.
#[test]
fn test_a_deployment_through_the_factory_replays() {
    let mut input = vec![0x11_u8; 32];
    input.extend_from_slice(&init_code());
    let replay = Case::new("factory", chain_with_contracts())
        .tx(call(0, CREATE2_FACTORY_ADDRESS, input.into(), 5_000_000))
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

/// An activation block admits deposits alone: the deposit runs and the user transaction is
/// refused, on both runs.
#[test]
fn test_an_activation_block_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("activation block", db)
        .ctx(common::unlimited_ctx().with_no_user_tx_activation_block(true))
        .tx(deposit(
            CALLER,
            alloy_primitives::TxKind::Call(CONTRACT),
            0,
            U256::ZERO,
            slot(1),
            write_gas(),
        ))
        .tx(call(0, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert!(run.refusal(1).contains("activation block"), "{}", run.refusal(1));
    let _ = BytecodeBuilder::default();
}
