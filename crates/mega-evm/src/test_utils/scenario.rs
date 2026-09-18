//! Transaction scenarios: a pre-state and a sequence of transactions, read from JSON.
//!
//! A [`Scenario`] is the input format of the differential harness (`crates/mega-differential`),
//! which runs every scenario through [`MegaEvm`] and through an independent EVM and compares the
//! two. The benches run slices of the same corpus, so their instruction counts move when the
//! semantics or the pricing of those scenarios move.
//!
//! The block, the configuration and the L1 fees are fixed, so a scenario is fully described by
//! its accounts and transactions:
//!
//! - the block is [`Scenario::block`]: number 1, timestamp 1, the scenario's coinbase, zero base
//!   fee, unlimited gas;
//! - the configuration is what [`MegaContext`] fixes for the spec;
//! - the L1 fees are zero ([`zero_fee_l1_block_info`], [`op_transaction`]), and so is the gas
//!   price, so the callers need no balance beyond the value they send.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{collections::BTreeMap, format, string::String, vec::Vec};

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use core::convert::Infallible;
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        transaction::{
            AccessList, AccessListItem, Authorization, RecoveredAuthority, RecoveredAuthorization,
        },
        BlockEnv, TxEnv,
    },
    state::{AccountInfo, Bytecode},
    Database, DatabaseCommit,
};
use serde::Deserialize;

use crate::{
    test_utils::{op_transaction, zero_fee_l1_block_info, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransactionError,
};

/// The outcome of one scenario transaction on [`MegaEvm`].
pub type ScenarioTxOutcome =
    Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>>;

/// A pre-state and the transactions to run on it, in order.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Name of the scenario; the harness reports differences under it.
    pub name: String,
    /// What the scenario exercises, for the reader.
    #[serde(default)]
    pub description: String,
    /// The block beneficiary.
    #[serde(default)]
    pub coinbase: Address,
    /// The accounts that exist before the first transaction.
    pub pre: BTreeMap<Address, PreAccount>,
    /// The transactions, run in order, each on the state the previous one left.
    pub txs: Vec<TxSpec>,
}

/// An account of the pre-state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreAccount {
    /// Account nonce.
    #[serde(default)]
    pub nonce: u64,
    /// Account balance.
    #[serde(default)]
    pub balance: U256,
    /// Legacy bytecode; empty for an account without code.
    #[serde(default)]
    pub code: Bytes,
    /// Storage slots.
    #[serde(default)]
    pub storage: BTreeMap<U256, U256>,
}

/// How a scenario transaction is run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxSpecKind {
    /// A message call to `to`.
    #[default]
    Call,
    /// A contract-creation transaction with `data` as init code.
    Create,
    /// A system call to `to`, as a block executor makes one: no fee, no nonce, the engine's
    /// system-call gas limit.
    SystemCall,
}

/// A scenario transaction.
///
/// The nonce is the caller's nonce in the state the transaction runs on. The transaction type
/// follows from the lists: EIP-7702 with an authorization list, EIP-2930 with only an access
/// list, legacy otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxSpec {
    /// Sender.
    pub caller: Address,
    /// Target of a call or a system call; absent for a create.
    #[serde(default)]
    pub to: Option<Address>,
    /// Calldata, or init code for a create.
    #[serde(default)]
    pub data: Bytes,
    /// Value sent.
    #[serde(default)]
    pub value: U256,
    /// Gas limit. Required for calls and creates; a system call runs with the engine's
    /// system-call gas limit and must not set one.
    #[serde(default)]
    pub gas_limit: Option<u64>,
    /// How the transaction is run.
    #[serde(default)]
    pub kind: TxSpecKind,
    /// EIP-2930 access list.
    #[serde(default)]
    pub access_list: Vec<AccessListEntry>,
    /// EIP-7702 authorization list.
    #[serde(default)]
    pub authorization_list: Vec<AuthorizationEntry>,
}

/// An EIP-2930 access list entry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessListEntry {
    /// Address to warm.
    pub address: Address,
    /// Storage keys of `address` to warm.
    #[serde(default)]
    pub storage_keys: Vec<U256>,
}

/// An EIP-7702 authorization with its signer already recovered.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationEntry {
    /// Chain id the authorization is valid on; zero for any chain.
    #[serde(default)]
    pub chain_id: U256,
    /// Address the authority delegates to.
    pub address: Address,
    /// Nonce the authority must have.
    pub nonce: u64,
    /// The recovered signer; absent for a signature that does not recover.
    #[serde(default)]
    pub authority: Option<Address>,
}

impl Scenario {
    /// Checks that the pre-state code decodes and that each transaction carries the fields its
    /// kind needs.
    pub fn validate(&self) -> Result<(), String> {
        for (address, account) in &self.pre {
            if let Err(err) = Bytecode::new_raw_checked(account.code.clone()) {
                return Err(format!("{}: code of {address}: {err:?}", self.name));
            }
        }
        for (i, tx) in self.txs.iter().enumerate() {
            let problem = match tx.kind {
                TxSpecKind::Call if tx.to.is_none() => Some("a call needs `to`"),
                TxSpecKind::Create if tx.to.is_some() => Some("a create must not set `to`"),
                TxSpecKind::Create if !tx.authorization_list.is_empty() => {
                    Some("an EIP-7702 transaction cannot create")
                }
                TxSpecKind::Call | TxSpecKind::Create if tx.gas_limit.is_none() => {
                    Some("a call or a create needs `gas_limit`")
                }
                TxSpecKind::SystemCall if tx.to.is_none() => Some("a system call needs `to`"),
                TxSpecKind::SystemCall
                    if tx.gas_limit.is_some() ||
                        !tx.value.is_zero() ||
                        !tx.access_list.is_empty() ||
                        !tx.authorization_list.is_empty() =>
                {
                    Some("a system call takes only `caller`, `to` and `data`")
                }
                _ => None,
            };
            if let Some(problem) = problem {
                return Err(format!("{}: tx[{i}]: {problem}", self.name));
            }
        }
        Ok(())
    }

    /// The pre-state as a database.
    ///
    /// Code is decoded as the node decodes it: an EIP-7702 delegation designator becomes a
    /// delegation, anything else legacy bytecode.
    pub fn database(&self) -> MemoryDatabase {
        let mut db = MemoryDatabase::default();
        for (address, account) in &self.pre {
            let mut info = AccountInfo {
                nonce: account.nonce,
                balance: account.balance,
                ..Default::default()
            };
            if !account.code.is_empty() {
                let code = Bytecode::new_raw_checked(account.code.clone())
                    .expect("validated: the pre-state code decodes");
                info.code_hash = code.hash_slow();
                info.code = Some(code);
            }
            db.insert_account_info(*address, info);
            for (slot, value) in &account.storage {
                db.set_account_storage(*address, *slot, *value);
            }
        }
        db
    }

    /// The block every transaction of the scenario runs in.
    pub fn block(&self) -> BlockEnv {
        BlockEnv {
            number: U256::from(1),
            timestamp: U256::from(1),
            beneficiary: self.coinbase,
            basefee: 0,
            gas_limit: u64::MAX,
            ..Default::default()
        }
    }

    /// Runs every transaction on a fresh [`MegaEvm`] over `db`, committing the state of each
    /// successful one before the next. Returns the outcomes in order and the final database.
    pub fn run(&self, mut db: MemoryDatabase) -> (Vec<ScenarioTxOutcome>, MemoryDatabase) {
        let mut outcomes = Vec::with_capacity(self.txs.len());
        for tx in &self.txs {
            let ctx = MegaContext::new(&mut db, MegaSpecId::SATIN)
                .with_block(self.block())
                .with_chain(zero_fee_l1_block_info());
            let mut evm = MegaEvm::new(ctx);
            let outcome = match tx.kind {
                TxSpecKind::SystemCall => evm.transact_system_call(
                    tx.caller,
                    tx.to.expect("validated: a system call has a target"),
                    tx.data.clone(),
                ),
                TxSpecKind::Call | TxSpecKind::Create => {
                    let nonce = evm.db_mut().basic(tx.caller).unwrap().map_or(0, |info| info.nonce);
                    evm.transact_raw(OpTx(op_transaction(tx.tx_env(nonce))))
                }
            };
            drop(evm);
            if let Ok(outcome) = &outcome {
                db.commit(outcome.state.clone());
            }
            outcomes.push(outcome);
        }
        (outcomes, db)
    }
}

impl TxSpec {
    /// The EIP-2718 transaction type the lists imply.
    pub fn tx_type(&self) -> u8 {
        if !self.authorization_list.is_empty() {
            4
        } else if !self.access_list.is_empty() {
            1
        } else {
            0
        }
    }

    /// The transaction as the EVM executes it, with `nonce` as the sender's nonce.
    ///
    /// Only meaningful for calls and creates; a system call is built by the engine.
    pub fn tx_env(&self, nonce: u64) -> TxEnv {
        let access_list = AccessList(
            self.access_list
                .iter()
                .map(|entry| AccessListItem {
                    address: entry.address,
                    storage_keys: entry.storage_keys.iter().map(|key| (*key).into()).collect(),
                })
                .collect(),
        );
        let authorizations = self
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
            .tx_type(Some(self.tx_type()))
            .caller(self.caller)
            .kind(self.to.map_or(TxKind::Create, TxKind::Call))
            .data(self.data.clone())
            .value(self.value)
            .gas_limit(self.gas_limit.unwrap_or_default())
            .gas_price(0)
            .gas_priority_fee((self.tx_type() == 4).then_some(0))
            .nonce(nonce)
            .access_list(access_list)
            .authorization_list_recovered(authorizations)
            .build_fill()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::BytecodeBuilder;
    use alloy_primitives::address;

    const CALLER: Address = address!("0x0000000000000000000000000000000000000aaa");
    const CALLEE: Address = address!("0x0000000000000000000000000000000000000bbb");

    fn call(gas_limit: Option<u64>) -> TxSpec {
        TxSpec {
            caller: CALLER,
            to: Some(CALLEE),
            data: Bytes::new(),
            value: U256::ZERO,
            gas_limit,
            kind: TxSpecKind::Call,
            access_list: Vec::new(),
            authorization_list: Vec::new(),
        }
    }

    fn scenario(txs: Vec<TxSpec>) -> Scenario {
        let code = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(7)).stop().build();
        Scenario {
            name: "sstore".into(),
            description: String::new(),
            coinbase: Address::ZERO,
            pre: [
                (CALLER, PreAccount { balance: U256::from(10), ..Default::default() }),
                (CALLEE, PreAccount { nonce: 1, code, ..Default::default() }),
            ]
            .into_iter()
            .collect(),
            txs,
        }
    }

    #[test]
    fn test_validate_rejects_a_call_without_gas_limit() {
        assert!(scenario(vec![call(Some(100_000))]).validate().is_ok());
        let err = scenario(vec![call(None)]).validate().unwrap_err();
        assert_eq!(err, "sstore: tx[0]: a call or a create needs `gas_limit`");
    }

    #[test]
    fn test_validate_rejects_a_system_call_with_gas_limit() {
        let tx = TxSpec { kind: TxSpecKind::SystemCall, ..call(Some(100_000)) };
        let err = scenario(vec![tx]).validate().unwrap_err();
        assert_eq!(err, "sstore: tx[0]: a system call takes only `caller`, `to` and `data`");
    }

    /// Each transaction runs on the state the previous one committed, with the caller's
    /// current nonce.
    #[test]
    fn test_run_commits_each_transaction_before_the_next() {
        let scenario = scenario(vec![call(Some(100_000)), call(Some(100_000))]);
        let (outcomes, mut db) = scenario.run(scenario.database());

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|outcome| outcome.as_ref().unwrap().result.is_success()));
        assert_eq!(db.basic(CALLER).unwrap().unwrap().nonce, 2);
        assert_eq!(db.storage(CALLEE, U256::ZERO).unwrap(), U256::from(7));
    }

    /// A delegation designator in the pre-state is a delegation, not legacy code that starts
    /// with an invalid opcode.
    #[test]
    fn test_database_decodes_a_delegation_designator() {
        let mut scenario = scenario(vec![]);
        let designator: Bytes = [&[0xef, 0x01, 0x00][..], CALLEE.as_slice()].concat().into();
        scenario.pre.get_mut(&CALLER).unwrap().code = designator;
        assert!(scenario.validate().is_ok());

        let code = scenario.database().basic(CALLER).unwrap().unwrap().code.unwrap();
        assert!(code.is_eip7702());

        scenario.pre.get_mut(&CALLER).unwrap().code = Bytes::from_static(&[0xef, 0x01, 0x00]);
        assert!(scenario.validate().unwrap_err().starts_with("sstore: code of"));
    }

    #[test]
    fn test_tx_type_follows_the_lists() {
        let mut tx = call(Some(100_000));
        assert_eq!(tx.tx_env(0).tx_type, 0);
        tx.access_list.push(AccessListEntry { address: CALLEE, storage_keys: vec![U256::ZERO] });
        assert_eq!(tx.tx_env(0).tx_type, 1);
        tx.authorization_list.push(AuthorizationEntry {
            chain_id: U256::ZERO,
            address: CALLEE,
            nonce: 0,
            authority: None,
        });
        let env = tx.tx_env(3);
        assert_eq!((env.tx_type, env.nonce, env.gas_priority_fee), (4, 3, Some(0)));
    }

    #[test]
    fn test_scenario_parses_from_json() {
        let json = r#"{
            "name": "transfer",
            "pre": { "0x0000000000000000000000000000000000000aaa": { "balance": "0x10" } },
            "txs": [{
                "caller": "0x0000000000000000000000000000000000000aaa",
                "to": "0x0000000000000000000000000000000000000bbb",
                "value": "0x1",
                "gas_limit": 21000,
                "access_list": [{ "address": "0x0000000000000000000000000000000000000bbb", "storage_keys": ["0x1"] }]
            }]
        }"#;
        let scenario: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(scenario.pre[&CALLER].balance, U256::from(16));
        assert_eq!(scenario.txs[0].access_list[0].storage_keys, vec![U256::from(1)]);
        assert!(serde_json::from_str::<Scenario>(&json.replace("gas_limit", "gas")).is_err());
    }
}
