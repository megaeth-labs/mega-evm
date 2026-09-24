//! `ExecuteEvm::transact` on the Satin engine next to op-revm's `OpEvm` on the same `CfgEnv`.
//!
//! Each workload runs through `MegaEvm` (`satin`) and through `OpEvm` (`op_revm`), so the gap is
//! the wrapper's own cost. EVM construction is setup and is not measured.
//!
//! - `empty_transaction`, `ether_transfer`: the per-transaction cost.
//! - `deep_calls`: a contract calling itself 64 deep, each frame writing a slot and logging, so
//!   every frame pays the frame lifecycle's lanes and every `SSTORE` and `LOG` its commit wrapper.
//! - `storage_writes`: 200 first writes to fresh slots, then 200 writes back, in one frame: the
//!   `SSTORE` wrapper's commit and refund.
//! - `logs`: 200 two-topic logs in one frame: the `LOG` wrapper's commit, and the one
//!   `record_history_cost` each log's bytes cost — no allocation, no second pass over the table.
//! - `calldata`: a call carrying 4 KiB of calldata, which is 4 KiB of the transaction body's
//!   history, priced once before the first frame runs.
//! - `intercepted_calls`: 200 `STATICCALL`s to `MegaAccessControl`'s
//!   `isVolatileDataAccessDisabled`, which the interceptor answers: the dispatch and the synthetic
//!   result, 200 times.
//! - `system_address_misses`: the same 200 calls with a selector the contract does not intercept,
//!   so each pays the dispatch's address match and selector peek and then runs the bytecode.
//! - `storage_reads`: 200 `SLOAD`s of slots nothing volatile holds: the Host's storage load and the
//!   read wrapper around `SLOAD`, which finds nothing to settle.
//! - `volatile_reads`: 200 rounds of `TIMESTAMP`, `NUMBER` and a `BALANCE` of the block
//!   beneficiary, then a call to the Oracle, whose code loads 200 of its slots. Every one is a read
//!   gas detention marks in the Host and commits in the opcode's wrapper; the first caps the frame,
//!   and the call starts and resumes under the limit.
//! - `data_size_limit`: 200 fresh slots and 200 two-topic logs in one frame, run under a
//!   transaction data-size limit equal to exactly what the transaction keeps, so every record is
//!   checked against a limit it is about to reach (`satin`); the same one byte short of it, so the
//!   last log crosses and the transaction is stopped (`stopped`); and through op-revm, which has no
//!   limit to check (`op_revm`).
//! - `state_limits`: the same transaction under a state-gas limit and a KV limit equal to exactly
//!   the state gas and the write records it keeps, so every fresh slot is held to both as it is
//!   written (`satin`); and under a state-gas limit one gas short, so the last slot crosses and the
//!   transaction is stopped (`stopped`). Its op-revm baseline is `data_size_limit/op_revm`.
//!
//! Two more run through `MegaEvm` alone, because they price something op-revm has no equivalent
//! of: `salt_storage_writes` and `salt_new_accounts` each draw one EIP-8037 state gas charge per
//! slot or per account, and are benchmarked twice — once against the default environment, where
//! every bucket is minimal, and once (`/crowded`) against a SALT environment holding every bucket
//! at eight times the minimum capacity. The gap is what the pricing hook costs on the hot path:
//! one environment read per bucket and a cache hit per charge after it.
//!
//! Every workload is run once before it is measured, and the run is held to what it must draw:
//! each pays its body's history, the logging one pays for the bytes its logs append and the
//! calldata one for the bytes it carries, and the data-size one keeps exactly the bytes and
//! records it is sized for, and stops at its last log one byte short of them; the state-limit one
//! keeps exactly the state gas and records its limits allow, and stops at its last slot one gas
//! short of them. A workload that
//! stopped drawing what it is here to measure would otherwise still benchmark, and measure the
//! wrong thing.
#![allow(missing_docs)]

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U160, U256};
use alloy_sol_types::SolCall;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    constants::COST_PER_HISTORY_BYTE,
    system::{
        IMegaAccessControl, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, ExternalEnvs, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm,
    MegaSpecId, TestExternalEnvs, VolatileDataAccess, LOG_BASE_SIZE, LOG_TOPIC_SIZE,
    MIN_BUCKET_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use op_revm::{L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    bytecode::opcode::{
        ADDRESS, BALANCE, CALL, CALLDATALOAD, DUP1, GAS, ISZERO, JUMPDEST, JUMPI, LOG0, LOG2,
        MSTORE, NUMBER, POP, PUSH0, PUSH1, SLOAD, SSTORE, STATICCALL, STOP, SUB, SWAP1, TIMESTAMP,
    },
    context::{BlockEnv, CfgEnv, Context, ContextTr, TxEnv},
    inspector::NoOpInspector,
    ExecuteEvm, Journal,
};

const CALLER: Address = address!("0x0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0x0000000000000000000000000000000000100001");
const RECURSIVE: Address = address!("0x0000000000000000000000000000000000100002");
const WRITER: Address = address!("0x0000000000000000000000000000000000100003");
const LOGGER: Address = address!("0x0000000000000000000000000000000000100004");
const SALT_WRITER: Address = address!("0x0000000000000000000000000000000000100005");
const SALT_CALLER: Address = address!("0x0000000000000000000000000000000000100006");
const INTERCEPTED: Address = address!("0x0000000000000000000000000000000000100007");
const MISSING: Address = address!("0x0000000000000000000000000000000000100008");
const LIMITED: Address = address!("0x0000000000000000000000000000000000100009");
const READER: Address = address!("0x000000000000000000000000000000000010000a");
const VOLATILE: Address = address!("0x000000000000000000000000000000000010000b");

/// The block beneficiary of the benchmark's block, `BlockEnv`'s default.
const BENEFICIARY: Address = Address::ZERO;

/// A selector `MegaAccessControl` intercepts, and one it does not.
const IS_DISABLED: [u8; 4] = IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR;
const UNKNOWN_SELECTOR: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

/// Depth the recursive contract reaches.
const DEPTH: u8 = 64;
/// Slots written, then written back, by the storage workload; logs emitted by the log workload.
const REPEAT: u64 = 200;

/// Calldata bytes the `calldata` workload carries, which is what its body's history prices.
const CALLDATA_LEN: usize = 4 * 1024;

/// Offset of the `JUMPDEST` the recursion ends at.
const DONE: u8 = 0x1f;

/// Slots written, and accounts created, by the two SALT workloads. Small enough that the whole
/// transaction fits in [`SALT_GAS_LIMIT`] at the crowded multiplier.
const SALT_REPEAT: u64 = 16;
/// How many minimum buckets a crowded bucket holds in the `/crowded` arms.
const SALT_MULTIPLIER: u64 = 8;
/// Room for `SALT_REPEAT` state charges at `SALT_MULTIPLIER`, on both arms alike.
const SALT_GAS_LIMIT: u64 = 60_000_000;
/// The first of `SALT_REPEAT` consecutive addresses the value calls create.
const SALT_ACCOUNT_BASE: u64 = 0x200000;

/// Reads `n` from calldata; while `n > 0` it writes slot `n`, logs, and calls itself with `n - 1`.
fn recursive_code() -> Bytes {
    #[rustfmt::skip]
    let body = [
        PUSH0, CALLDATALOAD,                             // n
        DUP1, ISZERO, PUSH1, DONE, JUMPI,                // n == 0: done
        DUP1, PUSH1, 1, SWAP1, SSTORE,                   // storage[n] = 1
        PUSH0, PUSH0, LOG0,                              // an empty log
        PUSH1, 1, SWAP1, SUB, PUSH0, MSTORE,             // memory[0..32] = n - 1
        PUSH0, PUSH0, PUSH1, 0x20, PUSH0, PUSH0, ADDRESS, GAS, CALL, // call self with n - 1
        STOP,
        JUMPDEST, STOP,                                  // done
    ];
    assert_eq!(body[DONE as usize], JUMPDEST);
    Bytes::from(body.to_vec())
}

/// Writes `REPEAT` fresh slots, then writes each back to zero.
fn writer_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 0..REPEAT {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    for slot in 0..REPEAT {
        code = code.sstore(U256::from(slot), U256::ZERO);
    }
    code.stop().build()
}

/// Emits `REPEAT` two-topic logs of 32 bytes.
fn logger_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for topic in 0..REPEAT {
        code = code
            .push_number(topic)
            .push_number(topic)
            .push_number(32_u8)
            .append(PUSH0)
            .append(LOG2);
    }
    code.stop().build()
}

/// Loads `REPEAT` slots of its own storage.
fn reader_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 0..REPEAT {
        code = code.push_number(slot).append(SLOAD).append(POP);
    }
    code.stop().build()
}

/// Reads the block's timestamp and number and the beneficiary's balance `REPEAT` times, then calls
/// the Oracle, whose code is [`reader_code`].
fn volatile_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..REPEAT {
        code = code
            .append_many([TIMESTAMP, POP, NUMBER, POP])
            .push_address(BENEFICIARY)
            .append(BALANCE)
            .append(POP);
    }
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

/// Writes `REPEAT` fresh slots, then emits `REPEAT` two-topic logs of 32 bytes: both kinds of
/// bytes the data-size limit counts, each checked against the limit as it is kept.
fn limited_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 0..REPEAT {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    for topic in 0..REPEAT {
        code = code
            .push_number(topic)
            .push_number(topic)
            .push_number(32_u8)
            .append(PUSH0)
            .append(LOG2);
    }
    code.stop().build()
}

/// What [`limited_code`] keeps: its body, one write record per slot, and each log's bytes.
const LIMITED_KEPT: u64 =
    TX_BODY_SIZE + REPEAT * WRITE_RECORD_SIZE + REPEAT * (LOG_BASE_SIZE + 2 * LOG_TOPIC_SIZE + 32);
/// Room for `REPEAT` fresh slots and `REPEAT` logs, far below the execution cap.
const LIMITED_GAS_LIMIT: u64 = 60_000_000;

/// Writes `SALT_REPEAT` fresh slots, each drawing one state gas charge the pricing hook prices.
fn salt_writer_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 0..SALT_REPEAT {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    code.stop().build()
}

/// Sends one wei to each of `SALT_REPEAT` accounts that do not exist, each drawing one
/// new-account state gas charge.
fn salt_caller_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for i in 0..SALT_REPEAT {
        code = code
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(0_u64)
            .push_number(1_u64)
            .push_address(Address::from(U160::from(SALT_ACCOUNT_BASE + i)))
            .push_number(1_000_000_u64)
            .append(CALL)
            .append(POP);
    }
    code.stop().build()
}

/// A SALT environment holding every bucket at `m` times the minimum capacity.
fn salt_envs(m: u64) -> TestExternalEnvs {
    TestExternalEnvs::new().with_default_bucket_capacity(MIN_BUCKET_SIZE as u64 * m)
}

/// A context over `db` reading `envs`.
fn salt_context(
    db: MemoryDatabase,
    envs: TestExternalEnvs,
) -> MegaContext<MemoryDatabase, TestExternalEnvs> {
    MegaContext::new_with_external_envs(db, MegaSpecId::SATIN, ExternalEnvs::from(envs))
        .with_chain(zero_fee_l1_block_info())
}

/// `STATICCALL`s `MegaAccessControl` with `selector` `REPEAT` times, discarding the answers.
fn system_caller_code(selector: [u8; 4]) -> Bytes {
    let mut code = BytecodeBuilder::default().mstore(0x0, selector);
    for _ in 0..REPEAT {
        code = code
            .push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(4_u64) // argsSize
            .push_number(0_u64) // argsOffset
            .push_address(ACCESS_CONTROL_ADDRESS)
            .push_number(100_000_u64)
            .append(STATICCALL)
            .append(POP);
    }
    code.stop().build()
}

fn call_tx(to: Address, data: Bytes, gas_limit: u64) -> OpTransaction<TxEnv> {
    op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data,
        gas_limit,
        ..Default::default()
    })
}

type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;

fn tx(value: U256) -> OpTransaction<TxEnv> {
    op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value,
        gas_limit: 1_000_000,
        ..Default::default()
    })
}

fn mega_context(db: MemoryDatabase) -> MegaContext<MemoryDatabase> {
    MegaContext::new(db, MegaSpecId::SATIN).with_chain(zero_fee_l1_block_info())
}

fn bench_transact(c: &mut Criterion) {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(1))
        .account_code(RECURSIVE, recursive_code())
        .account_code(WRITER, writer_code())
        .account_code(LOGGER, logger_code())
        .account_code(SALT_WRITER, salt_writer_code())
        .account_balance(SALT_CALLER, U256::from(10u64.pow(9)))
        .account_code(SALT_CALLER, salt_caller_code())
        .account_code(INTERCEPTED, system_caller_code(IS_DISABLED))
        .account_code(MISSING, system_caller_code(UNKNOWN_SELECTOR))
        .account_code(LIMITED, limited_code())
        .account_code(READER, reader_code())
        .account_code(VOLATILE, volatile_code())
        .account_code(ORACLE_CONTRACT_ADDRESS, reader_code())
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE);
    let cfg = mega_context(db.clone()).cfg().clone();

    let mut depth = [0u8; 32];
    depth[31] = DEPTH;
    let workloads = [
        ("empty_transaction", tx(U256::ZERO)),
        ("ether_transfer", tx(U256::from(1))),
        ("deep_calls", call_tx(RECURSIVE, Bytes::from(depth.to_vec()), 30_000_000)),
        ("storage_writes", call_tx(WRITER, Bytes::new(), 30_000_000)),
        ("logs", call_tx(LOGGER, Bytes::new(), 30_000_000)),
        ("calldata", call_tx(CALLEE, Bytes::from(vec![0xab_u8; CALLDATA_LEN]), 30_000_000)),
        ("intercepted_calls", call_tx(INTERCEPTED, Bytes::new(), 30_000_000)),
        ("system_address_misses", call_tx(MISSING, Bytes::new(), 30_000_000)),
        ("storage_reads", call_tx(READER, Bytes::new(), 30_000_000)),
        ("volatile_reads", call_tx(VOLATILE, Bytes::new(), 30_000_000)),
    ];

    let mut group = c.benchmark_group("transact");
    for (workload, tx) in workloads {
        let mut evm = MegaEvm::new(mega_context(db.clone()));
        let satin = evm.execute_transaction(OpTx(tx.clone())).unwrap();
        assert!(satin.result.is_success(), "{workload}: {:?}", satin.result);
        // Only the volatile workload reads what detention caps, and it must read every kind it is
        // here to measure.
        let detention = evm.ctx().detention();
        if workload == "volatile_reads" {
            assert_eq!(
                detention.accessed(),
                VolatileDataAccess::TIMESTAMP |
                    VolatileDataAccess::BLOCK_NUMBER |
                    VolatileDataAccess::BENEFICIARY_BALANCE |
                    VolatileDataAccess::ORACLE,
                "volatile_reads: every read must be marked",
            );
            assert!(detention.compute_limit().is_some(), "volatile_reads: the reads must detain");
        } else {
            assert_eq!(detention.compute_limit(), None, "{workload}: nothing here is volatile");
        }
        // Every transaction pays its body's history; the two workloads that are here to measure a
        // history charge pay what that charge is worth.
        let body = TX_BODY_SIZE + tx.base.data.len() as u64;
        assert!(
            satin.gas.history >= body * COST_PER_HISTORY_BYTE,
            "{workload}: every transaction pays for its body",
        );
        match workload {
            "deep_calls" => {
                let written =
                    satin.state[&RECURSIVE].storage.values().filter(|s| s.is_changed()).count();
                assert_eq!(written, DEPTH as usize, "every frame of the recursion ran");
            }
            "logs" => assert_eq!(
                satin.gas.history,
                (TX_BODY_SIZE + REPEAT * (LOG_BASE_SIZE + 2 * LOG_TOPIC_SIZE + 32)) *
                    COST_PER_HISTORY_BYTE,
                "the logs must draw history gas, which is what this arm measures",
            ),
            "calldata" => assert_eq!(
                satin.gas.history,
                body * COST_PER_HISTORY_BYTE,
                "the calldata must draw history gas, which is what this arm measures",
            ),
            _ => {}
        }
        group.bench_function(format!("{workload}/satin"), |b| {
            b.iter_batched(
                || MegaEvm::new(mega_context(db.clone())),
                |mut evm| evm.transact(OpTx(tx.clone())).unwrap(),
                BatchSize::SmallInput,
            );
        });
        group.bench_function(format!("{workload}/op_revm"), |b| {
            b.iter_batched(
                || {
                    let ctx = OpContext::new(db.clone(), OpSpecId::KARST)
                        .with_cfg(cfg.clone())
                        .with_chain(zero_fee_l1_block_info());
                    OpEvm::new(ctx, NoOpInspector)
                },
                |mut evm| evm.transact(tx.clone()).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }

    // The data-size arms: one transaction under a limit it exactly reaches and under one a byte
    // short of it, next to op-revm running it with no limit at all.
    let limited_tx = call_tx(LIMITED, Bytes::new(), LIMITED_GAS_LIMIT);
    let limited = |limit| {
        mega_context(db.clone())
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit))
    };
    let at_limit =
        MegaEvm::new(limited(LIMITED_KEPT)).execute_transaction(OpTx(limited_tx.clone())).unwrap();
    assert!(at_limit.result.is_success(), "data_size_limit: {:?}", at_limit.result);
    assert_eq!(
        at_limit.usage,
        LimitUsage { data_size: LIMITED_KEPT, write_records: REPEAT },
        "data_size_limit: the arm must keep every byte and record it is sized for",
    );
    let stopped = MegaEvm::new(limited(LIMITED_KEPT - 1))
        .execute_transaction(OpTx(limited_tx.clone()))
        .unwrap();
    assert_eq!(
        stopped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: LIMITED_KEPT - 1,
            used: LIMITED_KEPT,
            frame_local: false,
        }),
        "data_size_limit/stopped: the last log must be the byte that crosses",
    );
    for (arm, limit) in [("satin", LIMITED_KEPT), ("stopped", LIMITED_KEPT - 1)] {
        group.bench_function(format!("data_size_limit/{arm}"), |b| {
            b.iter_batched(
                || MegaEvm::new(limited(limit)),
                |mut evm| evm.transact(OpTx(limited_tx.clone())).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    group.bench_function("data_size_limit/op_revm", |b| {
        b.iter_batched(
            || {
                let ctx = OpContext::new(db.clone(), OpSpecId::KARST)
                    .with_cfg(cfg.clone())
                    .with_chain(zero_fee_l1_block_info());
                OpEvm::new(ctx, NoOpInspector)
            },
            |mut evm| evm.transact(limited_tx.clone()).unwrap(),
            BatchSize::SmallInput,
        );
    });

    // The state-limit arms: the same transaction under a state-gas and a KV limit it exactly
    // reaches, and under a state-gas limit one gas short of it.
    let state_gas = MegaEvm::new(mega_context(db.clone()))
        .execute_transaction(OpTx(limited_tx.clone()))
        .unwrap()
        .gas
        .state;
    let state_limited = |limit| {
        mega_context(db.clone()).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_state_gas_limit(limit)
                .with_tx_kv_update_limit(REPEAT),
        )
    };
    let at_limit = MegaEvm::new(state_limited(state_gas))
        .execute_transaction(OpTx(limited_tx.clone()))
        .unwrap();
    assert!(at_limit.result.is_success(), "state_limits: {:?}", at_limit.result);
    assert_eq!(at_limit.gas.state, state_gas, "state_limits: the arm must reach its state gas");
    assert_eq!(at_limit.usage.write_records, REPEAT, "state_limits: and its records");
    let stopped = MegaEvm::new(state_limited(state_gas - 1))
        .execute_transaction(OpTx(limited_tx.clone()))
        .unwrap();
    assert_eq!(
        stopped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::StateGrowth,
            limit: state_gas - 1,
            used: state_gas,
            frame_local: false,
        }),
        "state_limits/stopped: the last slot must be the gas that crosses",
    );
    for (arm, limit) in [("satin", state_gas), ("stopped", state_gas - 1)] {
        group.bench_function(format!("state_limits/{arm}"), |b| {
            b.iter_batched(
                || MegaEvm::new(state_limited(limit)),
                |mut evm| evm.transact(OpTx(limited_tx.clone())).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }

    // The SALT arms: the same transaction against a minimal environment and against one where
    // every bucket is crowded, both through `MegaEvm` — op-revm has nothing to compare to.
    let salt_workloads = [
        ("salt_storage_writes", call_tx(SALT_WRITER, Bytes::new(), SALT_GAS_LIMIT)),
        ("salt_new_accounts", call_tx(SALT_CALLER, Bytes::new(), SALT_GAS_LIMIT)),
    ];
    for (workload, tx) in salt_workloads {
        let minimal = salt_envs(1);
        let crowded = salt_envs(SALT_MULTIPLIER);
        let at_minimum = MegaEvm::new(salt_context(db.clone(), minimal.clone()))
            .execute_transaction(OpTx(tx.clone()))
            .unwrap();
        let at_crowded = MegaEvm::new(salt_context(db.clone(), crowded.clone()))
            .execute_transaction(OpTx(tx.clone()))
            .unwrap();
        assert!(at_minimum.result.is_success(), "{workload}: {:?}", at_minimum.result);
        assert!(at_crowded.result.is_success(), "{workload}: {:?}", at_crowded.result);
        assert_eq!(
            at_crowded.gas.state,
            at_minimum.gas.state * SALT_MULTIPLIER,
            "{workload}: the crowded arm must actually pay the crowded price",
        );

        for (arm, envs) in [("satin", minimal), ("crowded", crowded)] {
            group.bench_function(format!("{workload}/{arm}"), |b| {
                b.iter_batched(
                    || MegaEvm::new(salt_context(db.clone(), envs.clone())),
                    |mut evm| evm.transact(OpTx(tx.clone())).unwrap(),
                    BatchSize::SmallInput,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_transact);
criterion_main!(benches);
