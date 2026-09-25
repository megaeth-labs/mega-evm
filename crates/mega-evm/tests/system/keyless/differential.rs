//! A differential record of keyless deployments: a fixed set of scenarios, each run without an
//! inspector, under one that records everything it is told, under one that also rewrites every
//! frame result into a success, and under one that answers every creation itself, and each
//! rendered as a canonical text record — the result and the state, the gas by ledger, the usage,
//! the stop, what gas detention made of the transaction, and every event the inspector saw with
//! the journal depth it saw it at.
//!
//! The record is the way to hold a change to the engine's frame lifecycle to what it changes:
//! when `MEGA_KEYLESS_RECORD` names a file, the record is written there, and the records of two
//! builds can be compared line by line. The test itself holds every scenario's recorded run to its
//! plain one, on everything but the events.
//!
//! The scenarios: every rule a call can be refused by, and every charge it can run out of gas
//! on; a deployment from nonce 0 and from nonce 1, one that moves value, one by a delegated
//! signer, a constructor that reverts, halts, logs, writes, and destroys itself; the limits a
//! deployment crosses, from either nonce; gas detention holding the call and its creation; a
//! latched body; a system call; and the calls that are not dispatched. Each at a gas limit below
//! the execution cap and one above it, where it can run there.

use std::fmt::Write as _;

use alloy_primitives::{address, keccak256, Log};
use mega_evm::{
    constants::{MAX_INITCODE_SIZE, TX_GAS_LIMIT_CAP},
    system::{
        keyless::{
            tests::{CREATE2_FACTORY_TX, NON_CONTRACT_CREATION_TX, POST_EIP155_CHAIN_1_TX},
            KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
        MEGA_SYSTEM_ADDRESS,
    },
    test_utils::BytecodeBuilder,
    MegaTransaction,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLER, CREATE, GAS, INVALID, LOG0, POP, PUSH0, REVERT, SELFDESTRUCT, STATICCALL,
        STOP, TIMESTAMP,
    },
    context::{BlockEnv, CfgEnv, ContextTr, JournalTr},
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
    },
    state::{AccountInfo, Bytecode, EvmState},
    Database, InspectSystemCallEvm, Inspector, SystemCallEvm,
};

use super::{detention::reads_then_burns, *};
use crate::common::{context, CALLER as RELAYER};

/// Where the record is written, when set.
const RECORD_PATH: &str = "MEGA_KEYLESS_RECORD";

/// Every scenario, in every mode: the recorded runs match the plain ones, and the record is
/// written where `MEGA_KEYLESS_RECORD` says.
#[test]
fn test_keyless_differential_record() {
    let mut record = String::new();
    for scenario in scenarios() {
        let plain = scenario.run(Mode::Plain);
        let recorded = scenario.run(Mode::Recorded);
        assert_eq!(
            recorded.outcome, plain.outcome,
            "{}: an inspector that only records changes nothing",
            scenario.name
        );
        for mode in [Mode::Rewritten, Mode::Answered] {
            let run = scenario.run(mode);
            writeln!(record, "== {} [{mode:?}]\n{}{}", scenario.name, run.outcome, run.events)
                .unwrap();
        }
        writeln!(record, "== {} [Plain]\n{}", scenario.name, plain.outcome).unwrap();
        writeln!(
            record,
            "== {} [Recorded]\n{}{}",
            scenario.name, recorded.outcome, recorded.events
        )
        .unwrap();
    }
    if let Ok(path) = std::env::var(RECORD_PATH) {
        std::fs::write(&path, record).expect("the record is written");
    }
}

/* ---------- the scenarios ---------- */

/// One transaction to run.
#[derive(Clone)]
struct Scenario {
    name: String,
    db: MemoryDatabase,
    from: Address,
    to: Address,
    data: Bytes,
    value: U256,
    gas_limit: u64,
    limits: EvmTxRuntimeLimits,
    beneficiary: Address,
    eip3607_off: bool,
    /// Run as a system call from the system address, which takes no gas limit or value.
    system_call: bool,
}

impl Scenario {
    /// A `keylessDeploy` transaction carrying `data` over `db` at `gas_limit`, with no runtime
    /// limit.
    fn new(name: impl Into<String>, db: MemoryDatabase, data: Bytes, gas_limit: u64) -> Self {
        Self {
            name: format!("{} @ {gas_limit}", name.into()),
            db,
            from: RELAYER,
            to: KEYLESS_DEPLOY_ADDRESS,
            data,
            value: U256::ZERO,
            gas_limit,
            limits: EvmTxRuntimeLimits::no_limits(),
            beneficiary: Address::ZERO,
            eip3607_off: false,
            system_call: false,
        }
    }

    /// A `keylessDeploy` of `deployment`, with the largest override.
    fn deploying(
        name: impl Into<String>,
        db: MemoryDatabase,
        deployment: &Deployment,
        gas_limit: u64,
    ) -> Self {
        Self::new(name, db, deployment.call_data(LARGE_OVERRIDE), gas_limit)
    }

    fn limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.limits = limits;
        self
    }

    fn beneficiary(mut self, beneficiary: Address) -> Self {
        self.beneficiary = beneficiary;
        self
    }

    fn value(mut self, value: U256) -> Self {
        self.value = value;
        self
    }

    fn from(mut self, from: Address) -> Self {
        self.from = from;
        self
    }

    fn to(mut self, to: Address) -> Self {
        self.to = to;
        self
    }

    fn eip3607_off(mut self) -> Self {
        self.eip3607_off = true;
        self
    }

    fn system_call(mut self) -> Self {
        self.system_call = true;
        self
    }

    fn context(&self) -> MegaContext<MemoryDatabase> {
        let mut ctx = context(self.db.clone());
        if self.eip3607_off {
            let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
            cfg.disable_eip3607 = true;
            ctx = ctx.with_cfg(cfg);
        }
        ctx.with_block(BlockEnv { beneficiary: self.beneficiary, ..block() })
            .with_tx_runtime_limits(self.limits)
    }

    fn tx(&self) -> MegaTransaction {
        let mut tx = call_tx(self.to, self.data.clone(), self.value);
        tx.0.base.gas_limit = self.gas_limit;
        tx.0.base.caller = self.from;
        tx
    }

    /// Runs the scenario in `mode`.
    fn run(&self, mode: Mode) -> Run {
        let evm = MegaEvm::new(self.context());
        let Some(recorder) = mode.recorder() else {
            let mut evm = evm;
            let outcome = if self.system_call {
                let result = SystemCallEvm::system_call_with_caller(
                    &mut evm,
                    MEGA_SYSTEM_ADDRESS,
                    self.to,
                    self.data.clone(),
                );
                render_system_call(result.map(|r| (r.result, r.state)))
            } else {
                render_outcome(evm.execute_transaction(self.tx()))
            };
            let outcome = outcome + &render_detention(evm.ctx());
            return Run { outcome, events: String::new() };
        };
        let mut evm = evm.with_inspector(recorder);
        let outcome = if self.system_call {
            let result = InspectSystemCallEvm::inspect_system_call_with_caller(
                &mut evm,
                MEGA_SYSTEM_ADDRESS,
                self.to,
                self.data.clone(),
            );
            render_system_call(result.map(|r| (r.result, r.state)))
        } else {
            render_outcome(evm.execute_transaction(self.tx()))
        };
        let outcome = outcome + &render_detention(evm.ctx());
        let recorder = evm.inspector();
        let mut events = String::new();
        for line in recorder.lines.iter().chain(recorder.pending_steps().as_ref()) {
            writeln!(events, "  {line}").unwrap();
        }
        Run { outcome, events }
    }
}

/// What a run rendered: everything but the events, and the events.
struct Run {
    outcome: String,
    events: String,
}

/// How a scenario is run.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Without an inspector.
    Plain,
    /// Under an inspector that records everything it is told.
    Recorded,
    /// Under one that also rewrites every frame result into a success, reviving creations.
    Rewritten,
    /// Under one that answers every creation with a revert in its place.
    Answered,
}

impl Mode {
    fn recorder(self) -> Option<Recorder> {
        match self {
            Self::Plain => None,
            Self::Recorded => Some(Recorder::default()),
            Self::Rewritten => Some(Recorder { rewrite: true, ..Default::default() }),
            Self::Answered => Some(Recorder { answer_creations: true, ..Default::default() }),
        }
    }
}

/// Init code that logs `len` bytes of data and deploys a one-byte runtime.
fn logging(len: u64) -> Bytes {
    let prefix =
        BytecodeBuilder::default().push_number(len).push_number(0_u64).append(LOG0).build_vec();
    constructor(&prefix, &runtime(1))
}

/// Init code that fills a fresh slot and deploys a one-byte runtime.
fn filling_a_slot() -> Bytes {
    let prefix = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).build_vec();
    constructor(&prefix, &runtime(1))
}

/// The gas limit, below the execution cap, at which a call carrying `data` has `left` once it
/// paid its overhead.
fn leaving(data: &Bytes, left: u64) -> u64 {
    reference(data.clone(), GAS_LIMITS[0]).result.gas().total_gas_spent() +
        KEYLESS_DEPLOY_OVERHEAD_GAS +
        left
}

/// `db` with an EIP-7702 delegation to `delegate` on `signer`, at `nonce`.
fn delegated(
    mut db: MemoryDatabase,
    signer: Address,
    delegate: Address,
    nonce: u64,
) -> MemoryDatabase {
    let delegation = Bytecode::new_eip7702(delegate);
    db.insert_account_info(
        signer,
        AccountInfo {
            nonce,
            code_hash: delegation.hash_slow(),
            code: Some(delegation),
            ..Default::default()
        },
    );
    db
}

/// Every scenario the record holds.
fn scenarios() -> Vec<Scenario> {
    let mut all = Vec::new();
    let small = Deployment::new(deploying(&runtime(1)));
    let one_byte_init = deploying(&runtime(1)).len();

    for gas_limit in GAS_LIMITS {
        let at = |name: &str| name.to_string();
        let submit = |name: &str, db: MemoryDatabase, tx: &[u8], gas_limit_override: u64| {
            Scenario::new(
                name,
                db,
                keyless_deploy_call(tx, U256::from(gas_limit_override)),
                gas_limit,
            )
        };

        /* The rules. */
        all.push(
            Scenario::deploying(at("rule: value"), system_db(), &small, gas_limit).value(U256::ONE),
        );
        all.push(
            Scenario::new(
                "rule: value, arguments that do not decode",
                system_db(),
                Bytes::from(IKeylessDeploy::keylessDeployCall::SELECTOR.to_vec()),
                gas_limit,
            )
            .value(U256::ONE),
        );
        all.push(Scenario::new(
            "rule: arguments that do not decode",
            system_db(),
            Bytes::from([&IKeylessDeploy::keylessDeployCall::SELECTOR[..], &[0xab; 7]].concat()),
            gas_limit,
        ));
        all.push(submit(
            "rule: malformed transaction",
            system_db(),
            &[0xde, 0xad, 0xbe, 0xef],
            LARGE_OVERRIDE,
        ));
        let mut trailing = CREATE2_FACTORY_TX.to_vec();
        trailing.push(0);
        all.push(submit("rule: a trailing byte", system_db(), &trailing, LARGE_OVERRIDE));
        all.push(submit(
            "rule: not a creation",
            system_db(),
            NON_CONTRACT_CREATION_TX,
            LARGE_OVERRIDE,
        ));
        all.push(submit(
            "rule: not pre-EIP-155",
            system_db(),
            POST_EIP155_CHAIN_1_TX,
            LARGE_OVERRIDE,
        ));
        let nonce_one = Deployment::signed(1, SIGNED_GAS_LIMIT, U256::ZERO, deploying(&runtime(1)));
        all.push(Scenario::deploying(
            "rule: signed at nonce 1",
            system_db(),
            &nonce_one,
            gas_limit,
        ));
        all.push(submit(
            "rule: override below the signed gas limit",
            system_db(),
            CREATE2_FACTORY_TX,
            99_999,
        ));
        let mut corrupted = CREATE2_FACTORY_TX.to_vec();
        corrupted[102..134].fill(0xff);
        all.push(submit("rule: invalid signature", system_db(), &corrupted, LARGE_OVERRIDE));
        all.push(Scenario::deploying(
            "rule: signer nonce 2",
            system_db().account_nonce(small.signer, 2),
            &small,
            gas_limit,
        ));
        all.push(Scenario::deploying(
            "rule: signer with code",
            system_db().account_code(small.signer, Bytes::from_static(&[0x00])),
            &small,
            gas_limit,
        ));
        all.push(
            Scenario::deploying(
                "rule: signer with code, EIP-3607 off",
                system_db().account_code(small.signer, Bytes::from_static(&[0x00])),
                &small,
                gas_limit,
            )
            .eip3607_off(),
        );
        all.push(Scenario::deploying(
            "rule: occupied address",
            system_db().account_code(small.address, Bytes::from_static(&[0x60, 0x00])),
            &small,
            gas_limit,
        ));
        let carrying = Deployment::with_value(deploying(&runtime(1)), U256::from(10));
        all.push(Scenario::deploying(
            "rule: value the signer cannot fund",
            db_for(&carrying, U256::from(9)),
            &carrying,
            gas_limit,
        ));

        /* Deployments. */
        all.push(Scenario::deploying(
            "deploys from nonce 0, empty signer",
            system_db(),
            &small,
            gas_limit,
        ));
        all.push(Scenario::deploying(
            "deploys from nonce 0, funded signer",
            db_for(&small, U256::ONE),
            &small,
            gas_limit,
        ));
        all.push(Scenario::deploying(
            "deploys from nonce 1",
            system_db().account_nonce(small.signer, 1),
            &small,
            gas_limit,
        ));
        all.push(Scenario::deploying(
            "the address already has a balance",
            system_db().account_balance(small.address, U256::from(5)),
            &small,
            gas_limit,
        ));
        all.push(Scenario::deploying(
            "moves value",
            db_for(&carrying, U256::from(10)),
            &carrying,
            gas_limit,
        ));
        all.push(Scenario::deploying(
            "moves value from nonce 1",
            db_for(&carrying, U256::from(10)).account_nonce(carrying.signer, 1),
            &carrying,
            gas_limit,
        ));
        all.push(submit("the CREATE2 factory", system_db(), CREATE2_FACTORY_TX, LARGE_OVERRIDE));
        let reverting = Deployment::new(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
        let halting = Deployment::new(Bytes::from_static(&[INVALID]));
        let logging_deployment = Deployment::new(logging(64));
        let writing = Deployment::new(filling_a_slot());
        let empty = Deployment::new(Bytes::from_static(&[STOP]));
        let destroying = Deployment::new(Bytes::from_static(&[CALLER, SELFDESTRUCT]));
        for (name, deployment) in [
            ("a constructor that reverts", &reverting),
            ("a constructor that halts", &halting),
            ("a constructor that logs", &logging_deployment),
            ("a constructor that writes a slot", &writing),
            ("a constructor that deploys nothing", &empty),
            ("a constructor that destroys itself", &destroying),
        ] {
            all.push(Scenario::deploying(
                format!("{name}, nonce 0"),
                system_db(),
                deployment,
                gas_limit,
            ));
            all.push(Scenario::deploying(
                format!("{name}, nonce 1"),
                system_db().account_nonce(deployment.signer, 1),
                deployment,
                gas_limit,
            ));
        }
        let starved = Deployment::signed(0, 30_000, U256::ZERO, deploying(&runtime(1)));
        all.push(Scenario::new(
            "a constructor out of gas",
            system_db(),
            starved.call_data(30_000),
            gas_limit,
        ));

        /* A signer that sends its own deployment. */
        let rich = U256::from(1_000_000_000_000_u64);
        all.push(
            Scenario::deploying("the signer sends it", db_for(&small, rich), &small, gas_limit)
                .from(small.signer),
        );
        all.push(
            Scenario::deploying(
                "the signer sends it, and it reverts",
                db_for(&reverting, rich),
                &reverting,
                gas_limit,
            )
            .from(reverting.signer),
        );

        /* Delegated signers. */
        let delegate = address!("0x00000000000000000000000000000000000d1e6a");
        all.push(Scenario::deploying(
            "a delegated signer",
            delegated(system_db(), small.signer, delegate, 1),
            &small,
            gas_limit,
        ));
        let creating_twice = Bytes::from_static(&[
            PUSH0, PUSH0, PUSH0, CREATE, POP, PUSH0, PUSH0, PUSH0, CREATE, POP, STOP,
        ]);
        let calling_the_signer = [PUSH0, PUSH0, PUSH0, PUSH0, PUSH0, CALLER, GAS, CALL, POP];
        for (name, init_code) in [
            (
                "a delegated signer spending nonces, deploys",
                constructor(&calling_the_signer, &runtime(1)),
            ),
            (
                "a delegated signer spending nonces, reverts",
                Bytes::from([&calling_the_signer[..], &[PUSH0, PUSH0, REVERT]].concat()),
            ),
        ] {
            let deployment = Deployment::new(init_code);
            let db = delegated(
                system_db().account_code(delegate, creating_twice.clone()),
                deployment.signer,
                delegate,
                1,
            );
            all.push(Scenario::deploying(name, db, &deployment, gas_limit));
        }

        /* Limits. */
        let data_size = Deployment::new(logging(2_000));
        let body = reference(data_size.call_data(LARGE_OVERRIDE), GAS_LIMITS[0]).usage.data_size;
        let slot = Deployment::new(filling_a_slot());
        let compute = Deployment::new(reads_then_burns(TIMESTAMP, 1_000));
        for nonce in [0, 1] {
            let db = |d: &Deployment| system_db().account_nonce(d.signer, nonce);
            all.push(
                Scenario::deploying(
                    format!("limit: tx data size, nonce {nonce}"),
                    db(&data_size),
                    &data_size,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(body + 80 + 100)),
            );
            let signer = if nonce == 0 { entry(GasId::new_account_state_gas()) } else { 0 };
            let used =
                signer + entry(GasId::create_state_gas()) + entry(GasId::sstore_set_state_gas());
            all.push(
                Scenario::deploying(
                    format!("limit: state gas, nonce {nonce}"),
                    db(&slot),
                    &slot,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(used - 1)),
            );
            all.push(
                Scenario::deploying(
                    format!("limit: compute, nonce {nonce}"),
                    db(&compute),
                    &compute,
                    gas_limit,
                )
                .limits(
                    EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(1_000),
                ),
            );
            all.push(
                Scenario::deploying(
                    format!("limit: frame data size, nonce {nonce}"),
                    db(&data_size),
                    &data_size,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(1_000)),
            );
            all.push(
                Scenario::deploying(
                    format!("limit: the call's budget, nonce {nonce}"),
                    db(&small),
                    &small,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(39)),
            );
            all.push(
                Scenario::deploying(
                    format!("limit: tx KV, nonce {nonce}"),
                    db(&slot),
                    &slot,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_tx_kv_update_limit(2)),
            );
            all.push(
                Scenario::deploying(
                    format!("limit: frame KV, nonce {nonce}"),
                    db(&slot),
                    &slot,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_frame_kv_update_limit(3)),
            );
            all.push(
                Scenario::deploying(
                    format!("limit: the creation's start, nonce {nonce}"),
                    db(&small),
                    &small,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(body + 40)),
            );
            let upfront = entry(GasId::new_account_state_gas()) + entry(GasId::create_state_gas());
            all.push(
                Scenario::deploying(
                    format!("limit: upfront state gas, nonce {nonce}"),
                    db(&small),
                    &small,
                    gas_limit,
                )
                .limits(EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(upfront - 1)),
            );
        }
        all.push(
            Scenario::deploying("limit: a latched body", system_db(), &small, gas_limit)
                .limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(100)),
        );
        all.push(
            Scenario::deploying("limit: the default limits", system_db(), &small, gas_limit)
                .limits(EvmTxRuntimeLimits::default()),
        );

        /* Gas detention. */
        let capped =
            |cap| EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
        all.push(
            Scenario::deploying(
                "detention: a signer that is the beneficiary",
                system_db(),
                &small,
                gas_limit,
            )
            .beneficiary(small.signer)
            .limits(capped(20_000_000)),
        );
        all.push(
            Scenario::deploying(
                "detention: a signer that is the beneficiary, crossing at the CREATE charge",
                system_db(),
                &small,
                gas_limit,
            )
            .beneficiary(small.signer)
            .limits(capped(create_regular(one_byte_init) - 1)),
        );
        all.push(
            Scenario::deploying(
                "detention: the sender is the beneficiary, below the overhead",
                system_db(),
                &small,
                gas_limit,
            )
            .beneficiary(RELAYER)
            .limits(capped(1)),
        );
        let burning = Deployment::new(reads_then_burns(PUSH0, 100));
        for cap in [60_000, 150_000, 400_000] {
            all.push(
                Scenario::deploying(
                    format!("detention: the sender is the beneficiary, cap {cap}"),
                    system_db(),
                    &burning,
                    gas_limit,
                )
                .beneficiary(RELAYER)
                .limits(capped(cap)),
            );
        }
        all.push(
            Scenario::deploying(
                "detention: a constructor that reads, within the cap",
                system_db(),
                &compute,
                gas_limit,
            )
            .limits(capped(20_000_000)),
        );

        /* Not dispatched. */
        all.push(Scenario::new(
            "not dispatched: version()",
            system_db(),
            // `version()`.
            Bytes::from_static(&[0x54, 0xfd, 0x4d, 0x50]),
            gas_limit,
        ));
        all.push(Scenario::new(
            "not dispatched: no selector",
            system_db(),
            Bytes::new(),
            gas_limit,
        ));
        let relaying = BytecodeBuilder::default()
            .mstore(0, &small.call_data(LARGE_OVERRIDE))
            .push_number(0_u8)
            .push_number(0_u8)
            .push_number(u16::try_from(small.call_data(LARGE_OVERRIDE).len()).unwrap())
            .push_number(0_u8)
            .push_number(0_u8)
            .push_address(KEYLESS_DEPLOY_ADDRESS)
            .append_many([GAS, CALL, POP, STOP])
            .build();
        let relayer = address!("0x0000000000000000000000000000000000c0de01");
        all.push(
            Scenario::new(
                "not dispatched: from a contract",
                system_db().account_code(relayer, relaying),
                Bytes::new(),
                gas_limit,
            )
            .to(relayer),
        );
        let static_relaying = BytecodeBuilder::default()
            .mstore(0, &small.call_data(LARGE_OVERRIDE))
            .push_number(0_u8)
            .push_number(0_u8)
            .push_number(u16::try_from(small.call_data(LARGE_OVERRIDE).len()).unwrap())
            .push_number(0_u8)
            .push_address(KEYLESS_DEPLOY_ADDRESS)
            .append_many([GAS, STATICCALL, POP, STOP])
            .build();
        all.push(
            Scenario::new(
                "not dispatched: a STATICCALL from a contract",
                system_db().account_code(relayer, static_relaying),
                Bytes::new(),
                gas_limit,
            )
            .to(relayer),
        );

        /* Where the contract is not deployed. */
        all.push(Scenario::deploying(
            "no contract code",
            MemoryDatabase::default().account_balance(RELAYER, U256::from(1_000_000_000_000_u64)),
            &small,
            gas_limit,
        ));

        /* A system call. */
        all.push(
            Scenario::deploying("a system call", system_db(), &small, gas_limit).system_call(),
        );
    }

    /* Below the execution cap only: the charges a call can run out of gas on. */
    let data = small.call_data(LARGE_OVERRIDE);
    all.push(Scenario::new(
        "out of gas: the overhead",
        system_db(),
        data.clone(),
        leaving(&data, 0) - 1,
    ));
    all.push(Scenario::new(
        "out of gas: the signer's account",
        system_db(),
        data.clone(),
        leaving(&data, 1_000),
    ));
    let signed = Deployment::signed(0, 500_000, U256::ZERO, deploying(&runtime(1)));
    let signed_data = signed.call_data(LARGE_OVERRIDE);
    let new_account = entry(GasId::new_account_state_gas());
    all.push(Scenario::new(
        "rule: the signer's account takes the forward below the signed gas limit",
        system_db(),
        signed_data.clone(),
        leaving(&signed_data, new_account + 500_000 - 1),
    ));
    let small_signed = Deployment::signed(0, 1_000, U256::ZERO, deploying(&runtime(1)));
    let small_data = small_signed.call_data(LARGE_OVERRIDE);
    all.push(Scenario::new(
        "out of gas: the creation's charges",
        db_for(&small_signed, U256::ONE),
        small_data.clone(),
        leaving(&small_data, 1_500),
    ));
    all.push(Scenario::new(
        "rule: the creation's charges take the forward below the signed gas limit",
        db_for(&signed, U256::ONE),
        signed_data.clone(),
        leaving(&signed_data, 500_000),
    ));
    all.push(Scenario::new(
        "the forward at the signed gas limit exactly",
        db_for(&signed, U256::ONE),
        signed_data.clone(),
        leaving(&signed_data, 500_000) +
            create_regular(one_byte_init) +
            entry(GasId::create_state_gas()) +
            2 * record(),
    ));

    /* Above the execution cap only: a forward near the cap. */
    let near_cap =
        Deployment::signed(0, TX_GAS_LIMIT_CAP - 50_000, U256::ZERO, deploying(&runtime(1)));
    all.push(Scenario::deploying(
        "rule: a forward below a signed gas limit near the cap",
        db_for(&near_cap, U256::ONE),
        &near_cap,
        GAS_LIMITS[1],
    ));

    /* A mebibyte of init code, at gas limits that cover its body. */
    let too_large = Deployment::new(vec![0; MAX_INITCODE_SIZE + 1].into());
    for gas_limit in [TX_GAS_LIMIT_CAP * 3 / 4, 10 * TX_GAS_LIMIT_CAP] {
        all.push(Scenario::deploying(
            "rule: init code over the limit",
            system_db(),
            &too_large,
            gas_limit,
        ));
    }
    all
}

/* ---------- the recorder ---------- */

/// Records every frame start and end, every log, every self-destruct and every interpreter
/// initialized, with the journal depth it was told at; folds the steps between two of those into
/// a count and a digest.
#[derive(Default)]
struct Recorder {
    lines: Vec<String>,
    steps: u64,
    digest: u64,
    /// Rewrites every frame result into a success, a creation's at its address.
    rewrite: bool,
    /// Answers every creation with a revert in its place.
    answer_creations: bool,
}

impl Recorder {
    fn mix(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.digest = (self.digest ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }

    /// The steps folded since the last event, as a line.
    fn pending_steps(&self) -> Option<String> {
        (self.steps > 0).then(|| format!("steps n={} digest={:016x}", self.steps, self.digest))
    }

    fn push(&mut self, line: String) {
        if let Some(steps) = self.pending_steps() {
            self.lines.push(steps);
            self.steps = 0;
            self.digest = 0;
        }
        self.lines.push(line);
    }
}

fn depth<DB: Database>(ctx: &MegaContext<DB>) -> usize {
    ctx.journal_ref().depth()
}

fn render_gas(gas: &Gas) -> String {
    format!("{gas:?}")
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Recorder {
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        ctx: &mut MegaContext<DB>,
    ) {
        let line = format!(
            "initialize_interp d={} target={} code={} gas={}",
            depth(ctx),
            interp.input.target_address,
            interp.bytecode.hash().unwrap_or_default(),
            render_gas(&interp.gas),
        );
        self.push(line);
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, ctx: &mut MegaContext<DB>) {
        self.steps += 1;
        self.mix(depth(ctx) as u64);
        self.mix(interp.bytecode.pc() as u64);
        self.mix(u64::from(interp.bytecode.opcode()));
        self.mix(interp.gas.remaining());
        self.mix(interp.gas.reservoir());
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.mix(interp.gas.remaining());
        self.mix(interp.gas.state_gas_spent() as u64);
        self.mix(interp.gas.history_gas_spent() as u64);
    }

    fn log(&mut self, ctx: &mut MegaContext<DB>, log: Log) {
        self.push(format!("log d={} {log:?}", depth(ctx)));
    }

    fn call(&mut self, ctx: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let input = inputs.input.bytes(ctx);
        let line = format!(
            "call d={} {} -> {} code_at={} value={:?} scheme={:?} static={} gas_limit={} \
             reservoir={} charged={} input={}:{} mem={:?} known_code={}",
            depth(ctx),
            inputs.caller,
            inputs.target_address,
            inputs.bytecode_address,
            inputs.value,
            inputs.scheme,
            inputs.is_static,
            inputs.gas_limit,
            inputs.reservoir,
            inputs.charged_new_account_state_gas,
            input.len(),
            keccak256(&input),
            inputs.return_memory_offset,
            inputs.known_bytecode.0,
        );
        self.push(line);
        None
    }

    fn call_end(
        &mut self,
        ctx: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let line = format!(
            "call_end d={} {} -> {} result={:?} output={} gas={} mem={:?} precompile={} \
             precompile_logs={} charged={} charged_at={} known_code={}",
            depth(ctx),
            inputs.caller,
            inputs.target_address,
            outcome.result.result,
            outcome.result.output,
            render_gas(&outcome.result.gas),
            outcome.memory_offset,
            outcome.was_precompile_called,
            outcome.precompile_call_logs.len(),
            outcome.charged_new_account_state_gas,
            outcome.charged_state_gas_address,
            inputs.known_bytecode.0,
        );
        self.push(line);
        if self.rewrite {
            outcome.result.result = InstructionResult::Stop;
            outcome.result.output = Bytes::new();
        }
    }

    fn create(
        &mut self,
        ctx: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let line = format!(
            "create d={} {} scheme={:?} value={} gas_limit={} reservoir={} init={}:{} \
             charged={} charged_at={}",
            depth(ctx),
            inputs.caller(),
            inputs.scheme(),
            inputs.value(),
            inputs.gas_limit(),
            inputs.reservoir(),
            inputs.init_code().len(),
            keccak256(inputs.init_code()),
            inputs.charged_create_state_gas(),
            inputs.charged_state_gas_address(),
        );
        self.push(line);
        self.answer_creations.then(|| CreateOutcome {
            result: InterpreterResult::new(
                InstructionResult::Revert,
                Bytes::new(),
                Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit(), inputs.reservoir()),
            ),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
            charged_state_gas_address: inputs.charged_state_gas_address(),
        })
    }

    fn create_end(
        &mut self,
        ctx: &mut MegaContext<DB>,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let line = format!(
            "create_end d={} {} result={:?} address={:?} output={} gas={} charged={} charged_at={}",
            depth(ctx),
            inputs.caller(),
            outcome.result.result,
            outcome.address,
            outcome.result.output,
            render_gas(&outcome.result.gas),
            outcome.charged_create_state_gas,
            outcome.charged_state_gas_address,
        );
        self.push(line);
        if self.rewrite {
            outcome.result.result = InstructionResult::Return;
            outcome.result.output = Bytes::new();
            outcome.address = Some(inputs.created_address(1));
        }
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        self.push(format!("selfdestruct {contract} -> {target} value={value}"));
    }
}

/* ---------- rendering ---------- */

fn render_state(state: &EvmState) -> String {
    let mut accounts: Vec<_> = state.iter().collect();
    accounts.sort_by_key(|(address, _)| **address);
    let mut out = String::new();
    for (address, account) in accounts {
        let info = &account.info;
        let code =
            info.code.as_ref().map_or("unloaded".to_string(), |code| format!("{}", code.len()));
        writeln!(
            out,
            "  {address}: status={:?} tx={:?} nonce={} balance={} code_hash={} code={code}",
            account.status, account.transaction_id, info.nonce, info.balance, info.code_hash,
        )
        .unwrap();
        let mut slots: Vec<_> = account.storage.iter().collect();
        slots.sort_by_key(|(key, _)| **key);
        for (key, slot) in slots {
            writeln!(out, "    {key}: {slot:?}").unwrap();
        }
    }
    out
}

fn render_outcome<E: core::fmt::Debug>(outcome: Result<MegaTransactionOutcome, E>) -> String {
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return format!("error: {error:?}\n"),
    };
    format!(
        "result: {:?}\nledgers: {:?}\nusage: {:?}\nlimit_exceeded: {:?}\nstate:\n{}",
        outcome.result,
        outcome.gas,
        outcome.usage,
        outcome.limit_exceeded,
        render_state(&outcome.state),
    )
}

fn render_system_call<R: core::fmt::Debug, E: core::fmt::Debug>(
    result: Result<(R, EvmState), E>,
) -> String {
    match result {
        Ok((result, state)) => format!("result: {result:?}\nstate:\n{}", render_state(&state)),
        Err(error) => format!("error: {error:?}\n"),
    }
}

fn render_detention<DB: Database>(ctx: &MegaContext<DB>) -> String {
    let detention = ctx.detention();
    format!(
        "detention: limit={:?} accessed={:?} detains={}\n",
        detention.compute_limit(),
        detention.accessed(),
        detention.detains(),
    )
}
