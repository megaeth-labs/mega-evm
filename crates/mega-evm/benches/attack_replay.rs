//! Benchmark replay of a real mainnet attack contract deployment.
//!
//! Loads a saved prestate snapshot (captured via `debug_traceCall` +
//! `prestateTracer` on `MegaETH` mainnet at a fixed block) and replays the
//! deploy through `MegaEvm` for both supported specs, alongside a vanilla
//! revm baseline executing the same CREATE on the same in-memory state.
//!
//! ## Motivation
//!
//! Production sequencer-monitor measurements on this exact transaction:
//!
//! | path                                | wall time |
//! |-------------------------------------|-----------|
//! | `mega-evm` + Tracer + `RocksDB` state | ~33 ms    |
//! | vanilla revm + `RocksDB` state        | ~1.4 ms   |
//!
//! The bench reproduces the same ratio in a hermetic, `RocksDB`-free
//! environment:
//!
//! | arm                                  | wall time |
//! |--------------------------------------|-----------|
//! | `mega-evm` `EQUIVALENCE` spec          | ~1.15 ms  |
//! | `mega-evm` `MINI_REX` spec             | ~35.9 ms  |
//! | vanilla revm                         | ~1.10 ms  |
//!
//! Both specs execute the same 205,951 opcodes and deploy the same 582-byte
//! runtime; the 30× gap between `EQUIVALENCE` and `MINI_REX` comes entirely
//! from the multi-dimensional `AdditionalLimit` accounting (quadratic LOG /
//! compute / storage / data / KV buckets) that `MINI_REX` enables.
//!
//! Pairing the `mega-evm` and `pure_revm` arms gives a hermetic regression
//! target for any limit-tracker / hot-path optimization (e.g. caching
//! `net_usage` in `FrameLimitTracker`).
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench attack_replay
//! ```

#![allow(missing_docs)]

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use alloy_primitives::{hex, Address, Bytes, B256, U256};
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    test_utils::MemoryDatabase, EmptyExternalEnv, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};
use revm::{
    context::{
        result::{ExecResultAndState, ExecutionResult, Output},
        tx::TxEnvBuilder,
        TxEnv,
    },
    interpreter::{interpreter::EthInterpreter, Interpreter},
    primitives::TxKind,
    Context, ExecuteEvm, InspectEvm, Inspector, MainBuilder, MainContext,
};
use serde_json::Value;

/// Captured fixture; see `fixtures/known_attack_deploy.json` for provenance.
const FIXTURE_JSON: &str = include_str!("fixtures/known_attack_deploy.json");

/// Parsed transaction fields. Kept simple — manual extraction so the fixture
/// can stay free-form JSON without requiring serde feature flags on alloy.
struct TxFixture {
    caller: Address,
    nonce: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    value: U256,
    chain_id: u64,
    input: Bytes,
}

/// Per-account snapshot entry from `prestateTracer`.
struct AccountFixture {
    balance: U256,
    nonce: u64,
    code: Bytes,
    storage: Vec<(B256, U256)>,
}

/// Block environment captured at the same block as `prestate`.
struct BlockFixture {
    number: u64,
    timestamp: u64,
    base_fee_per_gas: u64,
    gas_limit: u64,
    beneficiary: Address,
    mix_hash: B256,
}

fn parse_u256(v: &Value) -> U256 {
    let s = v.as_str().unwrap();
    if let Some(stripped) = s.strip_prefix("0x") {
        U256::from_str_radix(stripped, 16).unwrap()
    } else {
        U256::from_str_radix(s, 10).unwrap()
    }
}

fn parse_bytes(v: &Value) -> Bytes {
    let s = v.as_str().unwrap();
    Bytes::from(hex::decode(s.trim_start_matches("0x")).unwrap())
}

fn parse_address(v: &Value) -> Address {
    let s = v.as_str().unwrap();
    Address::from_str(s).unwrap()
}

fn load_fixture() -> (TxFixture, HashMap<Address, AccountFixture>, BlockFixture) {
    let json: Value = serde_json::from_str(FIXTURE_JSON).expect("fixture json parses");

    let tx_obj = &json["tx"];
    let tx = TxFixture {
        caller: parse_address(&tx_obj["caller"]),
        nonce: tx_obj["nonce"].as_u64().unwrap(),
        gas_limit: tx_obj["gas_limit"].as_u64().unwrap(),
        max_fee_per_gas: tx_obj["max_fee_per_gas"].as_u64().unwrap() as u128,
        max_priority_fee_per_gas: tx_obj["max_priority_fee_per_gas"].as_u64().unwrap() as u128,
        value: parse_u256(&tx_obj["value"]),
        chain_id: tx_obj["chain_id"].as_u64().unwrap(),
        input: parse_bytes(&tx_obj["input"]),
    };

    let mut prestate = HashMap::new();
    for (addr_str, acc) in json["prestate"].as_object().unwrap() {
        let addr = Address::from_str(addr_str).unwrap();
        let balance = acc.get("balance").map(parse_u256).unwrap_or_default();
        let nonce = acc.get("nonce").and_then(|v| v.as_u64()).unwrap_or(0);
        let code = acc.get("code").map(parse_bytes).unwrap_or_default();
        let storage: Vec<(B256, U256)> = acc
            .get("storage")
            .and_then(|s| s.as_object())
            .map(|m| m.iter().map(|(k, v)| (B256::from_str(k).unwrap(), parse_u256(v))).collect())
            .unwrap_or_default();
        prestate.insert(addr, AccountFixture { balance, nonce, code, storage });
    }

    let blk = &json["block"];
    let block = BlockFixture {
        number: blk["number"].as_u64().unwrap(),
        timestamp: blk["timestamp"].as_u64().unwrap(),
        base_fee_per_gas: blk["base_fee_per_gas"].as_u64().unwrap(),
        gas_limit: blk["gas_limit"].as_u64().unwrap(),
        beneficiary: parse_address(&blk["beneficiary"]),
        mix_hash: B256::from_str(blk["mix_hash"].as_str().unwrap()).unwrap(),
    };

    (tx, prestate, block)
}

/// Build a fresh `MemoryDatabase` populated with the captured prestate.
/// Called inside the per-iteration setup so each measurement starts from
/// the same on-chain state.
fn build_db(prestate: &HashMap<Address, AccountFixture>) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    for (addr, acc) in prestate {
        db.set_account_balance(*addr, acc.balance);
        if acc.nonce > 0 {
            db.set_account_nonce(*addr, acc.nonce);
        }
        if !acc.code.is_empty() {
            db.set_account_code(*addr, acc.code.clone());
        }
        for (slot, value) in &acc.storage {
            db.set_account_storage(*addr, (*slot).into(), *value);
        }
    }
    db
}

/// Build a `MegaContext` from the captured fixture for the given spec.
///
/// **`chain_id` matters**: leaving the default (1) silently sends `mega-evm`
/// down a short-circuit path that skips most of the limit-tracker work,
/// producing artificially-fast bench numbers (~80 µs vs ~36 ms). The
/// fixture's `chain_id` (4326 = `MegaETH` mainnet) must be threaded through.
fn build_mega_context(
    spec: MegaSpecId,
    prestate: &HashMap<Address, AccountFixture>,
    block: &BlockFixture,
    tx_fixture: &TxFixture,
) -> MegaContext<MemoryDatabase, EmptyExternalEnv> {
    let db = build_db(prestate);
    let mut ctx = MegaContext::new(db, spec);
    ctx.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    // MegaContext derefs to OpContext — cfg lives on the inner context.
    ctx.cfg.chain_id = tx_fixture.chain_id;
    ctx.modify_block(|b| {
        b.number = U256::from(block.number);
        b.timestamp = U256::from(block.timestamp);
        b.basefee = block.base_fee_per_gas;
        b.gas_limit = block.gas_limit;
        b.beneficiary = block.beneficiary;
        b.prevrandao = Some(block.mix_hash);
    });
    ctx
}

/// Build the `MegaTransaction` for the captured deploy.
fn build_mega_tx(tx: &TxFixture) -> MegaTransaction {
    let inner = TxEnvBuilder::new()
        .caller(tx.caller)
        .create()
        .data(tx.input.clone())
        .gas_limit(tx.gas_limit)
        .gas_price(tx.max_fee_per_gas)
        .gas_priority_fee(Some(tx.max_priority_fee_per_gas))
        .nonce(tx.nonce)
        .value(tx.value)
        .chain_id(Some(tx.chain_id))
        .build_fill();
    let mut mega_tx = MegaTransaction::new(inner);
    // The handler expects `enveloped_tx` to be set for EIP-2718 typed txns,
    // but the value isn't consumed by the limit-check / EVM path. Use empty.
    mega_tx.enveloped_tx = Some(Bytes::new());
    mega_tx
}

/// Build a vanilla revm `TxEnv` mirroring the captured fixture.
fn build_revm_tx(tx_fixture: &TxFixture) -> TxEnv {
    TxEnv {
        caller: tx_fixture.caller,
        gas_limit: tx_fixture.gas_limit,
        gas_price: tx_fixture.max_fee_per_gas,
        gas_priority_fee: Some(tx_fixture.max_priority_fee_per_gas),
        kind: TxKind::Create,
        value: tx_fixture.value,
        data: tx_fixture.input.clone(),
        nonce: tx_fixture.nonce,
        chain_id: Some(tx_fixture.chain_id),
        ..Default::default()
    }
}

/// Minimal `Inspector` that just counts how many opcodes the interpreter
/// executed. Used as a sanity check that the bench is not silently
/// short-circuiting. The count is exposed via a shared atomic so it can be
/// read back after the EVM consumes the inspector by value.
#[derive(Clone, Default)]
struct OpcodeCounter {
    steps: Arc<AtomicU64>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for OpcodeCounter {
    fn step(&mut self, _interp: &mut Interpreter<EthInterpreter>, _ctx: &mut CTX) {
        self.steps.fetch_add(1, Ordering::Relaxed);
    }
}

/// Minimum opcode count we expect the deploy to execute. If the bench
/// silently short-circuits (e.g. wrong `chain_id` in setup, validation
/// reject wrapped as `Ok`), step count drops to single digits — assert
/// catches that before users mistake an early-exit for a "fast" bench.
const MIN_EXPECTED_OPCODE_STEPS: u64 = 100_000;

fn sanity_check_mega(
    name: &str,
    spec: MegaSpecId,
    prestate: &HashMap<Address, AccountFixture>,
    block: &BlockFixture,
    tx_fixture: &TxFixture,
) {
    // First run: confirm Success variant + inspect deployed output / state.
    let ctx = build_mega_context(spec, prestate, block, tx_fixture);
    let mut evm = MegaEvm::new(ctx);
    let ExecResultAndState { result, state } =
        evm.transact(build_mega_tx(tx_fixture)).expect("mega evm.transact ok");

    let variant = match &result {
        ExecutionResult::Success { reason, output, .. } => match output {
            Output::Create(code, addr) => format!(
                "Success(Create, reason={reason:?}, deployed_code_len={}, addr={:?})",
                code.len(),
                addr,
            ),
            Output::Call(data) => {
                format!("Success(Call, reason={reason:?}, output_len={})", data.len())
            }
        },
        ExecutionResult::Revert { output, .. } => format!("Revert(output_len={})", output.len()),
        ExecutionResult::Halt { reason, .. } => format!("Halt({reason:?})"),
    };
    let slot_count: usize = state.values().map(|a| a.storage.len()).sum();
    eprintln!(
        "sanity[{name}]: gas_used={}  variant={}  accounts={}  storage_slots={}",
        result.gas_used(),
        variant,
        state.len(),
        slot_count,
    );
    assert!(
        result.is_success(),
        "deploy did not succeed on {name} — prestate / block env / spec config incomplete: {variant}",
    );

    // Second run: count opcode steps actually executed. Without this, a
    // short-circuit (e.g. tx rejected in validation but wrapped as Ok)
    // would look like a fast benchmark instead of failing loudly.
    let ctx = build_mega_context(spec, prestate, block, tx_fixture);
    let counter = OpcodeCounter::default();
    let counter_handle = counter.clone();
    let mut evm = MegaEvm::new(ctx).with_inspector(counter);
    let _ = evm.inspect_one_tx(build_mega_tx(tx_fixture)).expect("inspect_one_tx ok");
    let steps = counter_handle.steps.load(Ordering::Relaxed);
    eprintln!("sanity[{name}]: opcode_steps={}", steps);
    assert!(
        steps >= MIN_EXPECTED_OPCODE_STEPS,
        "bench appears to short-circuit on {name}: only {steps} opcode steps \
         executed (expected ≥ {MIN_EXPECTED_OPCODE_STEPS}). Check that \
         chain_id and block env are wired correctly.",
    );
}

fn sanity_check_pure_revm(
    prestate: &HashMap<Address, AccountFixture>,
    block: &BlockFixture,
    tx_fixture: &TxFixture,
) {
    let mut evm = Context::mainnet()
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = tx_fixture.chain_id;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
        })
        .modify_block_chained(|b| {
            b.number = U256::from(block.number);
            b.timestamp = U256::from(block.timestamp);
            b.basefee = block.base_fee_per_gas;
            b.gas_limit = block.gas_limit;
            b.beneficiary = block.beneficiary;
            b.prevrandao = Some(block.mix_hash);
        })
        .with_db(build_db(prestate))
        .build_mainnet();
    let res = evm.transact_one(build_revm_tx(tx_fixture)).expect("revm transact_one ok");
    let variant = match &res {
        ExecutionResult::Success { output: Output::Create(code, _), .. } => {
            format!("Success(Create, deployed_code_len={})", code.len())
        }
        ExecutionResult::Success { .. } => "Success(non-Create)".to_string(),
        ExecutionResult::Revert { output, .. } => format!("Revert(output_len={})", output.len()),
        ExecutionResult::Halt { reason, .. } => format!("Halt({reason:?})"),
    };
    eprintln!("sanity[pure_revm]: gas_used={}  variant={}", res.gas_used(), variant);
    assert!(
        variant.starts_with("Success(Create"),
        "pure revm did not deploy successfully: {variant}",
    );
}

fn bench_attack_replay(c: &mut Criterion) {
    let (tx_fixture, prestate, block) = load_fixture();

    // Run all sanity checks first so failures abort before criterion warms up.
    for (name, spec) in
        [("equivalence", MegaSpecId::EQUIVALENCE), ("mini_rex", MegaSpecId::MINI_REX)]
    {
        sanity_check_mega(name, spec, &prestate, &block, &tx_fixture);
    }
    sanity_check_pure_revm(&prestate, &block, &tx_fixture);

    let mut group = c.benchmark_group("attack_replay");

    for (name, spec) in
        [("equivalence", MegaSpecId::EQUIVALENCE), ("mini_rex", MegaSpecId::MINI_REX)]
    {
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let ctx = build_mega_context(spec, &prestate, &block, &tx_fixture);
                    (MegaEvm::new(ctx), build_mega_tx(&tx_fixture))
                },
                |(mut evm, mega_tx)| black_box(evm.transact(black_box(mega_tx))),
                BatchSize::SmallInput,
            )
        });
    }

    // Apples-to-apples comparison arm: vanilla revm running the same CREATE
    // on the same in-memory state. With the bench setup correct, this comes
    // back at ~1.1 ms — very close to the `equivalence` arm — which is what
    // we expect: same opcodes, similar interpreter overhead. The interesting
    // delta is `mini_rex` vs both at ~36 ms, isolating the cost of the
    // multi-dimensional `AdditionalLimit` accounting.
    //
    // Note: vanilla revm doesn't enforce OP-stack rules (L1 data fee, etc.)
    // and uses its own SpecId set, but for a self-contained CREATE that
    // doesn't touch L1Block / OP-specific precompiles, the opcode-level
    // work is the same.
    group.bench_function("pure_revm", |b| {
        b.iter_batched(
            || {
                let evm = Context::mainnet()
                    .modify_cfg_chained(|cfg| {
                        cfg.chain_id = tx_fixture.chain_id;
                        cfg.disable_balance_check = true;
                        cfg.disable_base_fee = true;
                    })
                    .modify_block_chained(|b| {
                        b.number = U256::from(block.number);
                        b.timestamp = U256::from(block.timestamp);
                        b.basefee = block.base_fee_per_gas;
                        b.gas_limit = block.gas_limit;
                        b.beneficiary = block.beneficiary;
                        b.prevrandao = Some(block.mix_hash);
                    })
                    .with_db(build_db(&prestate))
                    .build_mainnet();
                (evm, build_revm_tx(&tx_fixture))
            },
            |(mut evm, tx)| black_box(evm.transact_one(black_box(tx))),
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_attack_replay);
criterion_main!(benches);
