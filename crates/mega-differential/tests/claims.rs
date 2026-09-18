//! Checks of what the harness and its registry claim about the two arms.

use std::collections::BTreeMap;

use mega_differential::{corpus_dir, mega, oracle};
use mega_evm::{
    alloy_primitives::{address, hex, keccak256, Address, Bytes, U256},
    revm::{
        context_interface::cfg::gas_params::GasId as ForkGasId,
        database::{states::bundle_state::BundleRetention, State},
        DatabaseCommit,
    },
    test_utils::{PreAccount, Scenario, ScenarioTx, ScenarioTxKind},
    MegaContext, MegaSpecId,
};
use revm_oracle::{
    context_interface::cfg::gas_params::GasId as OracleGasId, primitives::hardfork::SpecId,
};

const CALLER: Address = address!("0x0000000000000000000000000000000000000aaa");
const CALLEE: Address = address!("0x0000000000000000000000000000000000000bbb");

/// The proxy of the `precompile_point_evaluation_*` scenarios: it calls the precompile and
/// records the call's status in slot 0, the size of the return data in slot 1 and its hash in
/// slot 2.
const KZG_PROXY: Address = address!("0x000000000000000000000000000000000000990a");

/// What a successful point evaluation returns: `FIELD_ELEMENTS_PER_BLOB` and the BLS modulus,
/// each as a 32-byte big-endian value.
const POINT_EVALUATION_OUTPUT: [u8; 64] = hex!(
    "0000000000000000000000000000000000000000000000000000000000001000"
    "73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001"
);

/// The gas ids the fork adds on top of upstream's. revm 43 has no price at these indexes.
const FORK_ONLY_GAS_IDS: &[&str] = &["code_deposit_history_gas"];

/// The oracle copies `MegaEvm`'s gas table index by index, which is only sound if every index
/// names the same price on both sides.
#[test]
fn test_gas_table_indexes_name_the_same_prices() {
    let mut fork_only = Vec::new();
    for id in 0..=u8::MAX {
        let (fork, oracle) = (ForkGasId::new(id).name(), OracleGasId::new(id).name());
        if oracle == "unknown" && fork != "unknown" {
            fork_only.push(fork);
        } else {
            assert_eq!(fork, oracle, "gas id {id}");
        }
    }
    assert_eq!(fork_only, FORK_ONLY_GAS_IDS);
}

/// A price only the fork has is one the oracle cannot charge, so it must be zero while the
/// corpus is expected to match; the mechanism that prices it registers the difference.
#[test]
fn test_fork_only_gas_ids_are_priced_at_zero() {
    let ctx = MegaContext::new(mega_evm::revm::database::EmptyDB::default(), MegaSpecId::SATIN);
    for name in FORK_ONLY_GAS_IDS {
        let id = ForkGasId::from_name(name).unwrap();
        assert_eq!(ctx.mega_cfg().gas_params.get(id), 0, "{name}");
    }
}

#[test]
fn test_oracle_runs_the_satin_configuration() {
    let cfg = oracle::cfg();
    assert_eq!(cfg.spec, SpecId::OSAKA);
    let ctx = MegaContext::new(mega_evm::revm::database::EmptyDB::default(), MegaSpecId::SATIN);
    assert_eq!(cfg.gas_params.table(), ctx.mega_cfg().gas_params.table());
    assert!(cfg.enable_amsterdam_eip8037);
    assert!(cfg.enable_amsterdam_eip2780);
    assert_eq!(cfg.tx_gas_limit_cap, Some(200_000_000));
    assert_eq!(cfg.chain_id, ctx.mega_cfg().chain_id);
}

/// The registry's vault entries rest on this: a vault op-revm touches with a zero fee is
/// dropped by state clearing when the transaction's state is committed.
#[test]
fn test_zero_fee_vault_touch_leaves_no_committed_account() {
    let vaults = [
        address!("0x4200000000000000000000000000000000000019"),
        address!("0x420000000000000000000000000000000000001a"),
        address!("0x420000000000000000000000000000000000001b"),
    ];
    let scenario = Scenario {
        name: "transfer".into(),
        description: String::new(),
        coinbase: Address::ZERO,
        pre: BTreeMap::from([(
            CALLER,
            PreAccount { balance: U256::from(10), ..Default::default() },
        )]),
        txs: vec![ScenarioTx {
            caller: CALLER,
            to: Some(CALLEE),
            data: Bytes::new(),
            value: U256::from(1),
            gas_limit: Some(100_000),
            gas_price: 0,
            kind: ScenarioTxKind::Call,
            access_list: Vec::new(),
            authorization_list: Vec::new(),
        }],
    };
    let (outcomes, _) = scenario.run(scenario.database());
    let state = outcomes.into_iter().next().unwrap().unwrap().state;
    for vault in vaults {
        assert!(state[&vault].is_touched() && state[&vault].is_empty(), "{vault}");
    }

    let mut db = State::builder().with_database(scenario.database()).with_bundle_update().build();
    db.commit(state);
    db.merge_transitions(BundleRetention::PlainState);
    let bundle = db.take_bundle();

    assert!(bundle.account(&CALLEE).is_some(), "the transfer itself is committed");
    for vault in vaults {
        assert!(bundle.account(&vault).is_none(), "{vault}");
    }
}

/// Reads a scenario of `scenarios/harness/` by name.
fn harness_scenario(name: &str) -> Scenario {
    let path = corpus_dir().join("harness").join(format!("{name}.json"));
    let scenario: Scenario =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    scenario.validate().unwrap();
    scenario
}

/// The corpus compares the two arms, and two calls that fail agree as readily as two that
/// succeed, so the positive KZG case needs a pinned value on the left arm: this is what
/// `MegaEvm`'s KZG backend returns for a valid proof, and it is only produced after
/// `verify_kzg_proof` accepts it.
#[test]
fn test_point_evaluation_on_a_valid_proof_returns_the_field_parameters() {
    let scenario = harness_scenario("precompile_point_evaluation_valid");
    let record = mega::run(&scenario);

    assert_eq!(record.len(), 1);
    assert_eq!(record[0].outcome, "success");
    assert_eq!(record[0].output, Bytes::from_static(&POINT_EVALUATION_OUTPUT));
    // The proxy's record of the inner call: status 1, 64 bytes of return data, its hash.
    assert_eq!(
        record[0].state[&KZG_PROXY].storage,
        BTreeMap::from([
            (U256::ZERO, U256::from(1)),
            (U256::from(1), U256::from(64)),
            (U256::from(2), keccak256(POINT_EVALUATION_OUTPUT).into()),
        ]),
    );
}

/// The wrong-proof scenario differs from the valid one only in the proof, so its versioned hash
/// still matches its commitment: the call gets past that check and fails in verification, with
/// no status, no return data and the hash of an empty return.
#[test]
fn test_point_evaluation_on_a_wrong_proof_fails_in_verification() {
    let valid = harness_scenario("precompile_point_evaluation_valid");
    let scenario = harness_scenario("precompile_point_evaluation_wrong_proof");
    let (input, valid_input) = (&scenario.txs[0].data, &valid.txs[0].data);

    // | versioned hash 32 | z 32 | y 32 | commitment 48 | proof 48 |
    assert_eq!(input[..144], valid_input[..144], "same versioned hash, z, y and commitment");
    assert_ne!(input[144..192], valid_input[144..192], "a different proof");

    let record = mega::run(&scenario);
    assert_eq!(record.len(), 1);
    assert_eq!(record[0].outcome, "success");
    assert_eq!(record[0].output, Bytes::new());
    assert_eq!(
        record[0].state[&KZG_PROXY].storage,
        BTreeMap::from([(U256::from(2), keccak256([]).into())]),
        "storing zero into an empty slot is not a change, so only the hash slot is recorded",
    );
}
