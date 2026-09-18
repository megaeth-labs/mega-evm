//! Checks of what the harness and its registry claim about the two arms.

use std::collections::BTreeMap;

use mega_differential::oracle;
use mega_evm::{
    alloy_primitives::{address, Address, Bytes, U256},
    revm::{
        context_interface::cfg::gas_params::GasId as ForkGasId,
        database::{states::bundle_state::BundleRetention, State},
        DatabaseCommit,
    },
    test_utils::{PreAccount, Scenario, TxSpec, TxSpecKind},
    MegaContext, MegaSpecId,
};
use revm_oracle::{
    context_interface::cfg::gas_params::GasId as OracleGasId, primitives::hardfork::SpecId,
};

const CALLER: Address = address!("0x0000000000000000000000000000000000000aaa");
const CALLEE: Address = address!("0x0000000000000000000000000000000000000bbb");

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
        coinbase: Address::ZERO,
        pre: BTreeMap::from([(
            CALLER,
            PreAccount { balance: U256::from(10), ..Default::default() },
        )]),
        txs: vec![TxSpec {
            caller: CALLER,
            to: Some(CALLEE),
            data: Bytes::new(),
            value: U256::from(1),
            gas_limit: Some(100_000),
            kind: TxSpecKind::Call,
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
