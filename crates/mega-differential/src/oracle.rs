//! The right arm: stock revm 43 on its mainnet handler.
//!
//! The oracle takes its configuration from `MegaEvm`'s, field by field: the same Ethereum spec,
//! the same gas table (copied entry by entry), the same switches. It has no OP layer, so the
//! differences an OP-specific mechanism makes are the registry's to explain.

use std::{cell::Cell, collections::BTreeMap, convert::Infallible, str::FromStr};

use mega_evm::{
    revm::context::CfgEnv as MegaCfgEnv,
    test_utils::{Scenario, TxSpecKind},
    MegaContext, MegaSpecId,
};
use revm_oracle::{
    context::{
        result::{EVMError, ExecutionResult, HaltReason, ResultGas},
        transaction::{
            AccessList, AccessListItem, Authorization, RecoveredAuthority, RecoveredAuthorization,
        },
        BlockEnv, CfgEnv, ContextSetters, ContextTr, JournalTr, TxEnv,
    },
    context_interface::cfg::GasParams,
    database::{CacheDB, EmptyDB},
    handler::{
        system_call::SystemCallTx, EthFrame, EvmTr, FrameResult, FrameTr, Handler, MainnetHandler,
    },
    interpreter::interpreter::EthInterpreter,
    primitives::{hardfork::SpecId, TxKind},
    state::{AccountInfo, Bytecode, EvmState},
    Context, Database, DatabaseCommit, MainBuilder, MainContext, MainnetEvm,
};

use crate::record::{AccountRecord, LogRecord, ScenarioRecord, TxRecord};

type OracleDb = CacheDB<EmptyDB>;
type OracleEvm<'a> = MainnetEvm<revm_oracle::handler::MainnetContext<&'a mut OracleDb>>;
type OracleError = EVMError<Infallible>;

/// Runs `scenario` on revm 43 and records every transaction.
pub fn run(scenario: &Scenario) -> ScenarioRecord {
    let cfg = cfg();
    let block = block(scenario);
    let mut db = database(scenario);
    let mut records = Vec::with_capacity(scenario.txs.len());
    for tx in &scenario.txs {
        let mut evm = Context::mainnet()
            .with_db(&mut db)
            .with_cfg(cfg.clone())
            .with_block(block.clone())
            .build_mainnet();
        let mut probe = ReservoirProbe::default();
        let result = match tx.kind {
            TxSpecKind::SystemCall => {
                evm.ctx().set_tx(TxEnv::new_system_tx_with_caller(
                    tx.caller,
                    tx.to.expect("validated: a system call has a target"),
                    tx.data.clone(),
                ));
                probe.run_system_call(&mut evm)
            }
            TxSpecKind::Call | TxSpecKind::Create => {
                let nonce =
                    evm.ctx().db_mut().basic(tx.caller).unwrap().map_or(0, |info| info.nonce);
                evm.ctx().set_tx(tx_env(tx, nonce));
                probe.run(&mut evm)
            }
        };
        let state = evm.ctx().journal_mut().finalize();
        drop(evm);
        let record = match result {
            Ok(result) => {
                let record = record(&result, probe.reservoir.get(), &state);
                db.commit(state);
                record
            }
            Err(err) => {
                TxRecord { outcome: format!("error:{}", error(&err)), ..Default::default() }
            }
        };
        records.push(record);
    }
    records
}

/// The oracle's configuration: `MegaEvm`'s, field by field.
///
/// # Panics
///
/// If `MegaEvm` switches on a fork-only mechanism revm 43 cannot run: EIP-7708 transfer logs
/// before Amsterdam, or the system-call state-gas margin. The harness then needs a model of it or
/// a registry entry before it can compare.
pub fn cfg() -> CfgEnv {
    let ctx = MegaContext::new(mega_evm::revm::database::EmptyDB::default(), MegaSpecId::SATIN);
    let mega: &MegaCfgEnv<MegaSpecId> = ctx.mega_cfg();
    let eth_spec: &'static str = mega.spec.into_eth_spec().into();
    let spec = SpecId::from_str(eth_spec).expect("revm 43 knows every spec the fork knows");
    assert!(
        !mega.enable_amsterdam_eip7708 || spec.is_enabled_in(SpecId::AMSTERDAM),
        "MegaEvm switches EIP-7708 on before Amsterdam, which revm 43 cannot do"
    );
    assert!(
        !mega.system_call_state_gas_margin_in_reservoir,
        "MegaEvm reserves the system-call state-gas margin, which revm 43 does not have"
    );

    let mut cfg = CfgEnv::new_with_spec_and_gas_params(spec, gas_params(mega.gas_params.table()));
    cfg.chain_id = mega.chain_id;
    cfg.tx_chain_id_check = mega.tx_chain_id_check;
    cfg.limit_contract_code_size = mega.limit_contract_code_size;
    cfg.limit_contract_initcode_size = mega.limit_contract_initcode_size;
    cfg.disable_nonce_check = mega.disable_nonce_check;
    cfg.max_blobs_per_tx = mega.max_blobs_per_tx;
    cfg.blob_base_fee_update_fraction = mega.blob_base_fee_update_fraction;
    cfg.tx_gas_limit_cap = mega.tx_gas_limit_cap;
    cfg.memory_limit = mega.memory_limit;
    cfg.disable_balance_check = mega.disable_balance_check;
    cfg.disable_block_gas_limit = mega.disable_block_gas_limit;
    cfg.disable_eip3541 = mega.disable_eip3541;
    cfg.disable_eip3607 = mega.disable_eip3607;
    cfg.disable_eip7623 = mega.disable_eip7623;
    cfg.disable_base_fee = mega.disable_base_fee;
    cfg.enable_amsterdam_eip8037 = mega.enable_amsterdam_eip8037;
    cfg.enable_amsterdam_eip2780 = mega.enable_amsterdam_eip2780;
    cfg.amsterdam_eip7708_disabled = mega.amsterdam_eip7708_disabled;
    cfg.amsterdam_eip8246_delayed_clear_disabled = mega.amsterdam_eip8246_delayed_clear_disabled;
    cfg
}

/// A revm 43 gas table holding `table` entry for entry.
///
/// The fork numbers its gas ids as upstream does, so an index names the same price on both
/// sides; the fork's own ids sit at the top of the table, where upstream has none.
pub fn gas_params(table: &[u64; 256]) -> GasParams {
    GasParams::new(std::sync::Arc::new(*table))
}

fn block(scenario: &Scenario) -> BlockEnv {
    let mega = scenario.block();
    BlockEnv {
        number: mega.number,
        beneficiary: mega.beneficiary,
        timestamp: mega.timestamp,
        gas_limit: mega.gas_limit,
        basefee: mega.basefee,
        difficulty: mega.difficulty,
        prevrandao: mega.prevrandao,
        ..Default::default()
    }
}

fn database(scenario: &Scenario) -> OracleDb {
    let mut db = OracleDb::default();
    for (address, account) in &scenario.pre {
        let mut info =
            AccountInfo { nonce: account.nonce, balance: account.balance, ..Default::default() };
        if !account.code.is_empty() {
            let code = Bytecode::new_legacy(account.code.clone());
            info.code_hash = code.hash_slow();
            info.code = Some(code);
        }
        db.insert_account_info(*address, info);
        for (slot, value) in &account.storage {
            db.insert_account_storage(*address, *slot, *value).expect("in-memory database");
        }
    }
    db
}

fn tx_env(tx: &mega_evm::test_utils::TxSpec, nonce: u64) -> TxEnv {
    let access_list = AccessList(
        tx.access_list
            .iter()
            .map(|entry| AccessListItem {
                address: entry.address,
                storage_keys: entry.storage_keys.iter().map(|key| (*key).into()).collect(),
            })
            .collect(),
    );
    let authorizations = tx
        .authorization_list
        .iter()
        .map(|entry| {
            RecoveredAuthorization::new_unchecked(
                Authorization {
                    chain_id: entry.chain_id,
                    address: entry.address,
                    nonce: entry.nonce,
                },
                entry.authority.map_or(RecoveredAuthority::Invalid, RecoveredAuthority::Valid),
            )
        })
        .collect();
    TxEnv::builder()
        .tx_type(Some(tx.tx_type()))
        .caller(tx.caller)
        .kind(tx.to.map_or(TxKind::Create, TxKind::Call))
        .data(tx.data.clone())
        .value(tx.value)
        .gas_limit(tx.gas_limit.unwrap_or_default())
        .gas_price(0)
        .gas_priority_fee((tx.tx_type() == 4).then_some(0))
        .nonce(nonce)
        .access_list(access_list)
        .authorization_list_recovered(authorizations)
        .build_fill()
}

/// revm 43's mainnet handler, reading the reservoir left over at the point the fork records it.
///
/// revm 43's `ResultGas` has no `reservoir_remaining`. The fork reads it from the frame's gas
/// right after the refund, before the EIP-7623 floor may absorb it; this handler reads the same
/// gas at the same point, in the floor check that runs right after (for a system call, which
/// has no floor check, in `execution_result`, which gets the same gas).
#[derive(Default)]
struct ReservoirProbe<'a> {
    inner: MainnetHandler<OracleEvm<'a>, OracleError, EthFrame<EthInterpreter>>,
    reservoir: Cell<Option<u64>>,
}

impl<'a> Handler for ReservoirProbe<'a> {
    type Evm = OracleEvm<'a>;
    type Error = OracleError;
    type HaltReason = HaltReason;

    fn eip7623_check_gas_floor(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        init_and_floor_gas: revm_oracle::context_interface::cfg::gas::InitialAndFloorGas,
    ) {
        self.reservoir.set(Some(exec_result.gas().reservoir()));
        self.inner.eip7623_check_gas_floor(evm, exec_result, init_and_floor_gas);
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<HaltReason>, Self::Error> {
        if self.reservoir.get().is_none() {
            self.reservoir.set(Some(result.gas().reservoir()));
        }
        self.inner.execution_result(evm, result, result_gas)
    }
}

fn record(result: &ExecutionResult, reservoir: Option<u64>, state: &EvmState) -> TxRecord {
    TxRecord {
        outcome: match result {
            ExecutionResult::Success { .. } => "success".into(),
            ExecutionResult::Revert { .. } => "revert".into(),
            ExecutionResult::Halt { reason, .. } => format!("halt:{reason:?}"),
        },
        gas: gas(result.gas(), reservoir),
        output: result.output().cloned().unwrap_or_default(),
        created: result.created_address(),
        logs: result
            .logs()
            .iter()
            .map(|log| LogRecord {
                address: log.address,
                topics: log.topics().to_vec(),
                data: log.data.data.clone(),
            })
            .collect(),
        state: state
            .iter()
            .filter(|(_, account)| account.is_touched())
            .map(|(address, account)| {
                let record = AccountRecord {
                    created: account.is_created(),
                    selfdestructed: account.is_selfdestructed(),
                    balance: account.info.balance,
                    nonce: account.info.nonce,
                    code_hash: account.info.code_hash,
                    storage: account
                        .storage
                        .iter()
                        .filter(|(_, slot)| slot.is_changed())
                        .map(|(key, slot)| (*key, slot.present_value))
                        .collect(),
                };
                (*address, record)
            })
            .collect(),
    }
}

/// The gas figures of `gas` as [`crate::mega`] names them, with the probed reservoir in place of
/// the field revm 43 lacks.
fn gas(gas: &ResultGas, reservoir: Option<u64>) -> BTreeMap<String, u64> {
    let mut figures = crate::serialized_u64_fields(gas);
    if let Some(reservoir) = reservoir {
        figures.insert("reservoir_remaining".into(), reservoir);
    }
    figures.insert("tx_gas_used".into(), gas.tx_gas_used());
    figures.insert("block_regular_gas_used".into(), gas.block_regular_gas_used());
    figures.insert("block_state_gas_used".into(), gas.block_state_gas_used());
    figures.insert("final_refunded".into(), gas.final_refunded());
    figures
}

fn error(err: &OracleError) -> String {
    match err {
        EVMError::Transaction(invalid) => format!("invalid:{invalid:?}"),
        EVMError::Header(header) => format!("header:{header:?}"),
        other => format!("other:{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_oracle::{ExecuteEvm, SystemCallEvm};

    /// Runs `scenario` with revm 43's own entry points instead of the probe.
    fn run_stock(scenario: &Scenario) -> ScenarioRecord {
        let mut db = database(scenario);
        let mut records = Vec::new();
        for tx in &scenario.txs {
            let mut evm = Context::mainnet()
                .with_db(&mut db)
                .with_cfg(cfg())
                .with_block(block(scenario))
                .build_mainnet();
            let outcome = match tx.kind {
                TxSpecKind::SystemCall => {
                    evm.system_call_with_caller(tx.caller, tx.to.unwrap(), tx.data.clone())
                }
                _ => {
                    let nonce = evm.ctx().db_mut().basic(tx.caller).unwrap().map_or(0, |i| i.nonce);
                    evm.transact(tx_env(tx, nonce))
                }
            };
            drop(evm);
            let outcome = outcome.unwrap();
            records.push(record(&outcome.result, None, &outcome.state));
            db.commit(outcome.state);
        }
        records
    }

    /// The probe only reads the reservoir: without that figure, every scenario records the same
    /// as through revm 43's own `transact` and `system_call_with_caller`.
    #[test]
    fn test_reservoir_probe_changes_nothing_else() {
        let scenarios = crate::load_corpus(&crate::corpus_dir().join("handwritten")).unwrap();
        assert!(scenarios.iter().any(|s| s.txs.iter().any(|tx| tx.kind == TxSpecKind::SystemCall)));
        for scenario in &scenarios {
            let mut probed = run(scenario);
            for record in &mut probed {
                assert!(record.gas.remove("reservoir_remaining").is_some(), "{}", scenario.name);
            }
            assert_eq!(probed, run_stock(scenario), "{}", scenario.name);
        }
    }
}
