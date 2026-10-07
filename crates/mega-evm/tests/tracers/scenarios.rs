//! The Satin scenarios the tracer goldens pin, listed in [`SCENARIOS`], one test each so a
//! mismatch names the case.
//!
//! Every test asserts what its scenario must show — what it is billed on each ledger, worked out
//! from the schedule; the limit that stops it; what the tracers make of it — before it compares
//! or writes a golden, so a golden cannot pin output a relation rejects. Where a tracer shows
//! something other than what the engine did, the shape is asserted by name with its reason, so a
//! change on either side fails that assertion rather than moving a golden.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_rpc_types_trace::geth::PreStateFrame;
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    satin_gas_params, satin_precompiles,
    system::{
        keyless::{
            decode_error_result, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
            KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
        ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{op_transaction, transfer_log, BytecodeBuilder, MemoryDatabase},
    tx_body_history_bytes, EvmTxRuntimeLimits, LimitKind, MegaTransaction,
    FRAME_DATA_SHARE_DENOMINATOR, FRAME_DATA_SHARE_NUMERATOR, LOG_BASE_SIZE, TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{
        CODECOPY, GAS as GAS_OP, JUMP, JUMPDEST, LOG0, LOG1, POP, PUSH0, RETURN, STATICCALL,
        TIMESTAMP,
    },
    context::{result::ExecutionResult, TxEnv},
    context_interface::cfg::{
        gas::{BASE, JUMPDEST as JUMPDEST_GAS, MID, VERYLOW},
        GasId,
    },
    precompile::kzg_point_evaluation as kzg,
};

use crate::{
    gas::{self, Ledgers},
    harness::{
        assert_compute_at_stop, assert_detention_step_reads_as_out_of_gas, assert_eip3155_agrees,
        assert_every_frame_stops, assert_keyless_steps,
        assert_keyless_struct_logs_miss_the_creation, assert_ledgers,
        assert_log_index_counts_discarded, assert_logs, assert_parent_does_not_resume,
        assert_prestate_covers_reads, assert_root_frame_settles, assert_root_gas_is_the_gas_limit,
        at_spec_prices, call_tx, create_tx, decode_stop, eip3155_steps, eip3155_summary_gas_used,
        goldens_dir, pin_tracer_views, step, Traced, VIEWS,
    },
};

const CALLER: Address = address!("0x0000000000000000000000000000000000400000");
const PAYEE: Address = address!("0x0000000000000000000000000000000000400001");
const CONTRACT: Address = address!("0x0000000000000000000000000000000000400002");
const CHILD: Address = address!("0x0000000000000000000000000000000000400003");

/// Every scenario, by the directory its views are pinned under.
const SCENARIOS: [&str; 15] = [
    "create",
    "data_size_body_stop",
    "data_size_stop_spans_frames",
    "deposit",
    "detention_stop_spans_frames",
    "eth_transfer",
    "frame_budget_child_revert",
    "identity_precompile",
    "keyless_refused",
    "keyless_success",
    "nested_inner_revert",
    "precompile_child",
    "reservoir_sstore",
    "sstore_new_slot",
    "timestamp_detention",
];

/// Room for the small programs these scenarios run, below the execution cap.
const TX_GAS: u64 = 5_000_000;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_000_000_u64))
}

/// Holds `traced` to the relations every scenario meets — it is billed `expected` — then pins its
/// views under `name`.
///
/// Every test calls this last, so a relation that fails stops the test before any golden is
/// compared or, under `UPDATE_GOLDENS=1`, written.
fn pin(name: &str, traced: &Traced, expected: Ledgers) {
    pin_given_back(name, traced, expected, 0);
}

/// [`pin`], for a transaction whose own frame failed after `given_back` of its state and history
/// gas had spilled onto its regular gas: the failure gives it back after the tracer read the
/// frame (see `assert_root_frame_settles`).
fn pin_given_back(name: &str, traced: &Traced, expected: Ledgers, given_back: u64) {
    assert!(SCENARIOS.contains(&name), "{name} is not listed in SCENARIOS");
    assert_ledgers(traced, expected);
    assert_root_frame_settles(traced, given_back);
    assert_eip3155_agrees(traced);
    assert_prestate_covers_reads(traced);
    assert_root_gas_is_the_gas_limit(traced);
    if at_spec_prices() {
        pin_tracer_views(name, traced);
    }
}

/// The regular gas [`BytecodeBuilder::call`] spends before its callee runs: four `PUSH0`, a
/// `PUSH32` of the value, a `PUSH20` of the target, `GAS`, and the `CALL`'s cold access, with the
/// value transfer's charge when it carries value.
fn call_regular(carries_value: bool) -> u64 {
    let value = if carries_value { gas::entry(GasId::transfer_value_cost()) } else { 0 };
    4 * BASE + 2 * VERYLOW + BASE + gas::cold_account() + value
}

/// The regular gas [`BytecodeBuilder::sstore`] of a fresh, cold slot spends: two `PUSH32` and the
/// `SSTORE`.
fn sstore_regular() -> u64 {
    2 * VERYLOW + gas::sstore_fresh_cold()
}

/// The golden directories are exactly the scenarios, each holding exactly its views: a scenario
/// renamed or removed leaves no stale golden behind.
#[test]
fn test_goldens_are_exactly_the_scenarios() {
    let names = |dir: &Path| -> BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
            .map(|entry| entry.expect("a directory entry").file_name().into_string().unwrap())
            .collect()
    };
    let dir = goldens_dir();
    let scenarios: BTreeSet<String> = SCENARIOS.iter().map(|name| name.to_string()).collect();
    assert_eq!(names(&dir), scenarios, "the golden directories are the scenarios");
    let views: BTreeSet<String> = VIEWS.iter().map(|view| view.to_string()).collect();
    for scenario in &scenarios {
        assert_eq!(names(&dir.join(scenario)), views, "the views pinned for {scenario}");
    }
}

/// Ordinary ETH transfer. The 7708 transfer log is what `call_with_log` pins.
#[test]
fn test_eth_transfer_emits_7708_log() {
    let traced = Traced::run(
        funded(),
        call_tx(CALLER, PAYEE, Bytes::new(), U256::from(1), TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    assert_logs(&traced.call_frame(true).logs, &[transfer_log(CALLER, PAYEE, U256::from(1))]);
    assert_log_index_counts_discarded(&traced, &[0]);
    // The recipient does not exist: EIP-2780 charges the account the transfer adds, and the
    // transaction's own frame makes one record, the recipient's.
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], true),
        state: gas::account_state(),
        history: gas::body(0) + gas::records(1),
        floor: gas::call_floor(&[], true),
    };
    pin("eth_transfer", &traced, expected);
}

/// What a call that writes one fresh slot and stops is billed, whichever pool pays: the slot's
/// `SSTORE`, its state gas and its record.
fn fresh_slot_ledgers() -> Ledgers {
    Ledgers {
        regular: gas::call_intrinsic(&[], false) + sstore_regular(),
        state: gas::slot_state(),
        history: gas::body(0) + gas::records(1),
        floor: gas::call_floor(&[], false),
    }
}

/// A call that writes a fresh storage slot, so the receipt carries state gas.
#[test]
fn test_sstore_new_slot_charges_state_gas() {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build();
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    assert_eq!(traced.outcome.gas.state, gas::slot_state(), "a fresh slot costs state gas");
    // Below the execution cap there is no reservoir: the slot's state gas and its record's
    // history spill onto regular gas, so the opcode tracer shows them in the step's cost.
    assert_eq!(traced.outcome.gas.reservoir_remaining, 0, "no reservoir below the cap");
    let sstore = step(&traced, "SSTORE", 1);
    let spilled = gas::slot_state() + gas::records(1);
    assert_eq!(sstore.gas_cost, gas::sstore_fresh_cold() + spilled, "the SSTORE step's cost");
    let written = BTreeMap::from([(B256::with_last_byte(1), B256::with_last_byte(1))]);
    assert_eq!(sstore.storage, Some(written), "the SSTORE step shows the slot it filled");
    pin("sstore_new_slot", &traced, fresh_slot_ledgers());
}

/// A creation transaction that deposits a non-empty runtime: the deposit is charged its hashing,
/// state and history gas, and the code is in the post-state the diff tracer reports.
#[test]
fn test_create_deploys_runtime_code() {
    let traced = Traced::run(
        funded(),
        create_tx(CALLER, deploying_runtime(), TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    let created = CALLER.create(0);
    let code = traced.outcome.state[&created].info.code.as_ref().expect("the code is in the state");
    assert_eq!(code.original_bytes(), runtime(), "the created account holds the runtime");
    let PreStateFrame::Diff(diff) = traced.prestate(true) else { panic!("the diff mode") };
    assert_eq!(diff.post[&created].code.as_ref(), Some(&runtime()), "the diff shows the code");
    // The created account and its record, the init code, and the deposit: its hashing, the code's
    // state gas and its history.
    let init = deploying_runtime();
    let len = runtime().len();
    let expected = Ledgers {
        regular: gas::create_intrinsic(&init) + deploying_runtime_regular(),
        state: gas::created_state(len),
        history: gas::body(init.len()) + gas::records(1) + gas::history(len as u64),
        floor: gas::create_floor(&init),
    };
    // The EIP-3155 summary reads the frame's gas after its last step, the `RETURN`; the deposit is
    // charged after it, in `return_create`.
    let summary = eip3155_summary_gas_used(&traced);
    assert_eq!(summary + gas::deposit(len), expected.spent(), "EIP-3155 summary");
    pin("create", &traced, expected);
}

/// A value call whose callee writes a fresh slot and emits a log, then reverts; the caller then
/// emits a log of its own. Everything the callee was charged on the state and history ledgers is
/// given back: the slot's state gas, its record, its log, and the two records of the value
/// transfer its start made. Its logs, the EIP-7708 transfer log of the value included, are
/// discarded with it.
#[test]
fn test_nested_call_inner_reverts() {
    let child = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .push_number(0x42_u8)
        .append_many([PUSH0, PUSH0, LOG1])
        .revert()
        .build();
    let parent = BytecodeBuilder::default()
        .call(CHILD, U256::from(1))
        .append_many([POP, PUSH0, PUSH0, LOG0])
        .stop()
        .build();
    let traced = Traced::run(
        funded()
            .account_code(CONTRACT, parent)
            .account_balance(CONTRACT, U256::from(1))
            .account_code(CHILD, child),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "the caller succeeds: {:?}", traced.outcome.result);
    // The callee's charge was made: below the execution cap its slot's state gas and the slot's
    // record spilled onto its regular gas, which the opcode tracer shows in the step's cost.
    let sstore = step(&traced, "SSTORE", 2);
    let spilled = gas::slot_state() + gas::records(1);
    assert_eq!(sstore.gas_cost, gas::sstore_fresh_cold() + spilled, "the callee's SSTORE step");
    // And given back: no state gas stands, and the history the transaction keeps is its body
    // and the caller's log alone.
    assert_eq!(traced.outcome.gas.state, 0, "the callee's slot is given back");
    assert_eq!(
        traced.outcome.gas.history,
        gas::body(0) + gas::history(LOG_BASE_SIZE),
        "the callee's records and log are given back"
    );
    let root = traced.call_frame(true);
    let [callee] = root.calls.as_slice() else { panic!("one callee: {root:?}") };
    assert_eq!(callee.error.as_deref(), Some("execution reverted"));
    // The call tracer's `gasUsed` of the reverted callee counts the state and history gas its
    // revert gave back: revm gives a failed frame that gas back when its caller merges the
    // result, after the tracer read the frame.
    let callee_regular = sstore_regular() + VERYLOW + 2 * BASE + gas::log(1, 0) + 2 * BASE;
    assert_eq!(
        callee.gas_used,
        U256::from(callee_regular + spilled),
        "the reverted callee's gasUsed counts what its revert gave back"
    );
    assert!(callee.logs.is_empty(), "the callee's logs are discarded");
    assert_eq!(root.logs.len(), 1, "the caller's log alone, no transfer log");
    assert_eq!(root.logs[0].address, Some(CONTRACT));
    // The receipt keeps the caller's log alone, at index 0; before it the callee's transfer log
    // and its `LOG1` were emitted and discarded with the callee.
    assert_log_index_counts_discarded(&traced, &[2]);
    // The callee ran on what the caller forwarded and the value's stipend, and handed back what it
    // left, so the caller pays the callee's regular spend less the stipend; the callee's state and
    // history gas came back with its revert.
    let caller = call_regular(true) + BASE + 2 * BASE + gas::log(0, 0);
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], false) + caller + callee_regular -
            gas::entry(GasId::call_stipend()),
        state: 0,
        history: gas::body(0) + gas::history(LOG_BASE_SIZE),
        floor: gas::call_floor(&[], false),
    };
    pin("nested_inner_revert", &traced, expected);
}

fn system_db() -> MemoryDatabase {
    funded()
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE)
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
}

/// A pre-EIP-155 signed creation, as Nick's Method makes one.
struct Deployment {
    tx: Bytes,
    /// The address the signature recovers to.
    signer: Address,
}

impl Deployment {
    fn signed(value: U256, init_code: Bytes) -> Self {
        let tx = mega_evm::alloy_consensus::TxLegacy {
            nonce: 0,
            gas_price: 100_000_000_000,
            gas_limit: 1_000_000,
            to: TxKind::Create,
            value,
            input: init_code,
            chain_id: None,
        };
        let word = U256::from_be_bytes(hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));
        let signed = mega_evm::alloy_consensus::Signed::new_unchecked(
            tx,
            Signature::new(word, word, false),
            B256::ZERO,
        );
        let mut encoded = Vec::new();
        signed.rlp_encode(&mut encoded);
        let signer = signed.recover_signer().expect("Nick's-Method signature recovers");
        Self { tx: encoded.into(), signer }
    }

    fn call_data(&self) -> Bytes {
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: self.tx.clone(),
            gasLimitOverride: U256::from(10_000_000_000_u64),
        }
        .abi_encode()
        .into()
    }
}

/// The runtime the creation scenarios deploy: it returns nothing.
fn runtime() -> Bytes {
    BytecodeBuilder::default().return_empty().build()
}

/// The regular gas [`deploying_runtime`] spends, its deposit's included: three pushes of a byte or
/// two and two `PUSH0`, the `CODECOPY` of the runtime into one word of memory, the `RETURN`, and
/// the deposit's regular part.
fn deploying_runtime_regular() -> u64 {
    let len = runtime().len() as u64;
    let codecopy = VERYLOW + gas::entry(GasId::copy_per_word()) * gas::words(len);
    3 * VERYLOW +
        2 * BASE +
        codecopy +
        gas::memory(gas::words(len)) +
        gas::deposit_regular(len as usize)
}

/// Init code that copies [`runtime`] from its own tail and deposits it.
fn deploying_runtime() -> Bytes {
    let runtime = runtime();
    let len = u8::try_from(runtime.len()).expect("a short runtime");
    let prefix = BytecodeBuilder::default()
        .push_number(len)
        .push_number(0_u16)
        .append(PUSH0)
        .append(CODECOPY)
        .push_number(len)
        .append(PUSH0)
        .append(RETURN);
    let tail = u16::try_from(prefix.len()).expect("a short prefix");
    BytecodeBuilder::default()
        .push_number(len)
        .push_number(tail)
        .append(PUSH0)
        .append(CODECOPY)
        .push_number(len)
        .append(PUSH0)
        .append(RETURN)
        .append_many(runtime)
        .build()
}

fn keyless_tx(data: Bytes) -> MegaTransaction {
    call_tx(CALLER, KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO, TX_GAS)
}

/// A `keylessDeploy` that deploys: the answer names the signer's first creation address and no
/// error, and that address holds the runtime afterwards.
#[test]
fn test_keyless_deploy_succeeds() {
    let deployment = Deployment::signed(U256::ZERO, deploying_runtime());
    let traced = Traced::run(
        system_db(),
        keyless_tx(deployment.call_data()),
        EvmTxRuntimeLimits::no_limits(),
    );
    let ExecutionResult::Success { output, .. } = &traced.outcome.result else {
        panic!("the keyless call succeeds: {:?}", traced.outcome.result);
    };
    let answer = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
        .expect("a keylessDeploy answer");
    let deployed = deployment.signer.create(0);
    assert_ne!(deployed, Address::ZERO);
    assert_eq!(answer.deployedAddress, deployed, "the answer names the deploy address");
    assert!(answer.errorData.is_empty(), "a deployment that deployed reports no error");
    let code =
        traced.outcome.state[&deployed].info.code.as_ref().expect("the code is in the state");
    assert_eq!(code.original_bytes(), runtime(), "the deploy address holds the runtime");
    // `gasUsed` is what the creation spent from both pools: its init code and its deposit, the
    // deposit's state and history gas included. Below the execution cap there is no reservoir,
    // so all of it drew the creation frame's regular gas, which the tracer measured.
    let len = runtime().len();
    let creation_spend = deploying_runtime_regular() +
        satin_gas_params().code_deposit_state_gas(len) +
        gas::history(len as u64);
    assert_eq!(answer.gasUsed, creation_spend, "gasUsed is the creation's init code and deposit");
    let nodes = traced.inspector.traces().nodes();
    let (call, creation) = (&nodes[0].trace, &nodes[1].trace);
    assert_eq!(answer.gasUsed, creation.gas_used, "and the tracer measured the creation the same");
    // The call forwards all it has left once it paid for itself: the overhead, the `CREATE`
    // opcode's regular gas, the signer's account and the created account, and the creation's two
    // records, which spilled onto its regular gas.
    let call_charges = KEYLESS_DEPLOY_OVERHEAD_GAS +
        satin_gas_params().create_cost() +
        satin_gas_params().initcode_cost(deploying_runtime().len()) +
        gas::account_state() +
        gas::entry(GasId::create_state_gas()) +
        gas::records(2);
    assert_eq!(
        creation.gas_limit,
        call.gas_limit - call_charges,
        "the creation gets what the call has left after its own charges"
    );
    assert_keyless_struct_logs_miss_the_creation(&traced);
    assert_keyless_steps(&traced, true);
    // The call's overhead, the `CREATE` opcode's regular gas, the signer's account (it was empty)
    // and the created account, the creation's two records (the signer's nonce and the created
    // account), and what the creation spent: its init code and its deposit.
    let calldata = deployment.call_data();
    let init = deploying_runtime();
    let expected = Ledgers {
        regular: gas::call_intrinsic(&calldata, false) +
            KEYLESS_DEPLOY_OVERHEAD_GAS +
            satin_gas_params().create_cost() +
            satin_gas_params().initcode_cost(init.len()) +
            deploying_runtime_regular(),
        state: gas::account_state() + gas::created_state(len),
        history: gas::body(calldata.len()) + gas::records(2) + gas::history(len as u64),
        floor: gas::call_floor(&calldata, false),
    };
    // The EIP-3155 summary reads the creation's gas after its last step, the `RETURN`, two frames
    // away from the receipt: the deposit is charged after it, in `return_create`, and the call
    // frame returns what is left of the creation's gas, spending nothing more.
    let summary = eip3155_summary_gas_used(&traced);
    assert_eq!(summary + gas::deposit(len), expected.spent(), "EIP-3155 summary");
    pin("keyless_success", &traced, expected);
}

/// A `keylessDeploy` the rules refuse: value with an unfunded signer. No creation starts.
#[test]
fn test_keyless_deploy_refused() {
    let deployment = Deployment::signed(U256::from(1), deploying_runtime());
    let traced = Traced::run(
        system_db(),
        keyless_tx(deployment.call_data()),
        EvmTxRuntimeLimits::no_limits(),
    );
    let ExecutionResult::Revert { output, .. } = &traced.outcome.result else {
        panic!("the keyless call reverts: {:?}", traced.outcome.result);
    };
    assert_eq!(
        decode_error_result(output),
        Some(KeylessDeployError::InsufficientBalance),
        "the signer cannot fund the carried value"
    );
    assert_keyless_steps(&traced, false);
    // A refusal keeps the regular gas the call spent, the overhead, and gives back the signer's
    // account it charged before the balance rule refused.
    let calldata = deployment.call_data();
    let expected = Ledgers {
        regular: gas::call_intrinsic(&calldata, false) + KEYLESS_DEPLOY_OVERHEAD_GAS,
        state: 0,
        history: gas::body(calldata.len()),
        floor: gas::call_floor(&calldata, false),
    };
    // The call frame paid the signer's account out of its regular gas before the refusal gave it
    // back.
    pin_given_back("keyless_refused", &traced, expected, gas::account_state());
}

/// A parent that calls [`CHILD`] with all its gas and no value, then, if it resumes, writes a
/// slot and stops. A stop that reaches the parent must keep that write from running.
fn calling_child() -> Bytes {
    BytecodeBuilder::default()
        .call(CHILD, U256::ZERO)
        .append(POP)
        .sstore(U256::ZERO, U256::from(1))
        .stop()
        .build()
}

/// A frame that emits one `LOG0` of `data_len` bytes of memory, then stops.
fn logging(data_len: u8) -> Bytes {
    BytecodeBuilder::default().push_number(data_len).append_many([PUSH0, LOG0]).stop().build()
}

/// The regular gas [`logging`] spends: a `PUSH1`, a `PUSH0`, and the `LOG0` with its memory.
fn logging_regular(data_len: u8) -> u64 {
    let len = u64::from(data_len);
    VERYLOW + BASE + gas::log(0, len) + gas::memory(gas::words(len))
}

/// The data size of a `LOG0` of `data_len` bytes: its address word and its data.
fn log0_size(data_len: u8) -> u64 {
    LOG_BASE_SIZE + u64::from(data_len)
}

/// A body over the transaction's data-size limit, at the smallest limit a chain may carry: the
/// transaction is stopped before it runs, so its own frame is answered with the stop and runs no
/// instruction, and the recipient is never read.
#[test]
fn test_data_size_limit_stops_the_body() {
    let limit = TX_BODY_SIZE;
    let calldata = [0x01];
    assert!(tx_body_history_bytes(calldata.len() as u64, 0, 0, 0) > limit, "the body crosses");
    let traced = Traced::run(
        funded().account_code(CONTRACT, BytecodeBuilder::default().stop().build()),
        call_tx(CALLER, CONTRACT, Bytes::copy_from_slice(&calldata), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
    );
    assert_every_frame_stops(&traced, LimitKind::DataSize, limit, 1);
    assert!(traced.inspector.traces().nodes()[0].trace.steps.is_empty(), "no instruction ran");
    // The sender pays the intrinsic gas and the body's history, and nothing ran.
    let expected = Ledgers {
        regular: gas::call_intrinsic(&calldata, false),
        state: 0,
        history: gas::body(calldata.len()),
        floor: gas::call_floor(&calldata, false),
    };
    pin("data_size_body_stop", &traced, expected);
}

/// A child's `LOG0` that crosses the transaction's data-size limit and, with it, its own frame
/// budget. The transaction limit is the one reported, and every frame returns it: the child that
/// crossed it, and the parent, which does not resume.
#[test]
fn test_data_size_limit_stop_spans_frames() {
    // What the transaction may produce besides its body; the child's log is larger.
    let room = 100;
    let data_len = 128;
    assert!(log0_size(data_len) > room, "the log crosses the transaction's limit");
    let limit = TX_BODY_SIZE + room;
    let traced = Traced::run(
        funded().account_code(CONTRACT, calling_child()).account_code(CHILD, logging(data_len)),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
    );
    assert_every_frame_stops(&traced, LimitKind::DataSize, limit, 2);
    assert_parent_does_not_resume(&traced);
    // The child's `LOG0` charged its regular gas before its record crossed the limit; its log was
    // never made, so it cost no history.
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], false) + call_regular(false) + logging_regular(data_len),
        state: 0,
        history: gas::body(0),
        floor: gas::call_floor(&[], false),
    };
    pin("data_size_stop_spans_frames", &traced, expected);
}

/// A child's `LOG0` that crosses its own frame budget, 98/100 of what its parent has left, but
/// not the transaction's limit. Only the child reverts, with its budget; the parent resumes and
/// emits a log of its own, and the transaction succeeds.
#[test]
fn test_frame_budget_reverts_the_child_alone() {
    let room = 200;
    let budget = room * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR;
    let data_len = 166;
    assert!(log0_size(data_len) > budget, "the log crosses the child's budget");
    assert!(log0_size(data_len) <= room, "and not what the transaction has left");
    let parent = BytecodeBuilder::default()
        .call(CHILD, U256::ZERO)
        .append_many([POP, PUSH0, PUSH0, LOG0])
        .stop()
        .build();
    let traced = Traced::run(
        funded().account_code(CONTRACT, parent).account_code(CHILD, logging(data_len)),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE + room),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    assert!(traced.outcome.limit_exceeded.is_none(), "a frame budget latches nothing");
    let root = traced.call_frame(true);
    assert_eq!(root.error, None, "the parent succeeds");
    assert_eq!(root.logs.len(), 1, "the parent resumed and logged");
    // The tracer saw the child's `LOG0` before the frame budget refused it; the receipt keeps the
    // parent's log alone, at index 0.
    assert_log_index_counts_discarded(&traced, &[1]);
    let [child] = root.calls.as_slice() else { panic!("one child frame: {root:?}") };
    assert_eq!(child.error.as_deref(), Some("execution reverted"));
    assert_eq!(
        decode_stop(child.output.as_ref().expect("the child's revert data")),
        (LimitKind::DataSize, budget),
        "the child reverts with its own budget"
    );
    assert!(child.logs.is_empty(), "the child's log was never made");
    assert_eq!(
        traced.outcome.usage.data_size,
        TX_BODY_SIZE + LOG_BASE_SIZE,
        "the transaction keeps its body and the parent's log"
    );
    let parent = call_regular(false) + BASE + 2 * BASE + gas::log(0, 0);
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], false) + parent + logging_regular(data_len),
        state: 0,
        history: gas::body(0) + gas::history(LOG_BASE_SIZE),
        floor: gas::call_floor(&[], false),
    };
    pin("frame_budget_child_revert", &traced, expected);
}

/// A transaction's own frame that reads `TIMESTAMP` and then loops past the detention cap. The
/// crossing is in the transaction's own frame, which the engine settles when the transaction
/// ends rather than when a child returns.
#[test]
fn test_timestamp_detention_stop() {
    let cap = 80;
    let code = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .append(JUMPDEST)
        .push_number(2_u64)
        .append(JUMP)
        .build();
    // The compute at the read is the `TIMESTAMP` itself.
    let limit = BASE + cap;
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(cap),
    );
    assert_every_frame_stops(&traced, LimitKind::ComputeGas, limit, 1);
    assert_detention_step_reads_as_out_of_gas(&traced, "JUMP");
    // The transaction is billed its compute before the charge that crossed: the `TIMESTAMP`, the
    // `POP`, and the loop up to the limit.
    let compute = gas::compute_at_crossing(2 * BASE, limit, &[JUMPDEST_GAS, VERYLOW, MID]);
    assert_compute_at_stop(&traced, compute);
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], false) + compute,
        state: 0,
        history: gas::body(0),
        floor: gas::call_floor(&[], false),
    };
    pin("timestamp_detention", &traced, expected);
}

/// A parent that reads `TIMESTAMP` and calls a child that loops: the child crosses the compute
/// limit the parent's read set, and every frame returns the stop.
#[test]
fn test_detention_stop_spans_frames() {
    // Enough for the parent's call and a few dozen iterations of the child's loop.
    let cap = 3_000;
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, POP, TIMESTAMP, POP])
        .call(CHILD, U256::ZERO)
        .append(POP)
        .sstore(U256::ZERO, U256::from(1))
        .stop()
        .build();
    let child = BytecodeBuilder::default().append(JUMPDEST).push_number(0_u8).append(JUMP).build();
    // The compute at the read: `PUSH0`, `POP` and the `TIMESTAMP` itself.
    let limit = 3 * BASE + cap;
    let traced = Traced::run(
        funded().account_code(CONTRACT, parent).account_code(CHILD, child),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(cap),
    );
    assert_every_frame_stops(&traced, LimitKind::ComputeGas, limit, 2);
    assert_parent_does_not_resume(&traced);
    assert_detention_step_reads_as_out_of_gas(&traced, "JUMP");
    // The parent's work up to and with its call, then the child's loop up to the limit.
    let before_loop = 3 * BASE + BASE + call_regular(false);
    let compute = gas::compute_at_crossing(before_loop, limit, &[JUMPDEST_GAS, VERYLOW, MID]);
    assert_compute_at_stop(&traced, compute);
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], false) + compute,
        state: 0,
        history: gas::body(0),
        floor: gas::call_floor(&[], false),
    };
    pin("detention_stop_spans_frames", &traced, expected);
}

/// Gas limit above the 200M execution cap, so a fresh slot is paid from the reservoir first.
#[test]
fn test_reservoir_pays_state_gas() {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build();
    let reservoir = 50_000_000;
    let gas_limit = TX_GAS_LIMIT_CAP + reservoir;
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, gas_limit),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    assert_eq!(traced.outcome.gas.state, gas::slot_state());
    // The reservoir paid the body's history before the first frame, then the slot's state gas
    // and its record's history at the `SSTORE`; what is left goes back to the sender.
    let slot = gas::slot_state() + gas::records(1);
    assert_eq!(
        traced.outcome.gas.reservoir_remaining,
        reservoir - gas::body(0) - slot,
        "the reservoir left"
    );
    // The opcode tracer shows only the regular part of the `SSTORE`: the rest came out of the
    // reservoir, which only the EIP-3155 trace shows.
    assert_eq!(step(&traced, "SSTORE", 1).gas_cost, gas::sstore_fresh_cold(), "the regular part");
    let steps = eip3155_steps(&traced);
    let at = |op: &str| {
        let line = steps.iter().find(|line| line["opName"] == op).expect("the step");
        u64::from_str_radix(line["reservoir"].as_str().unwrap().trim_start_matches("0x"), 16)
            .unwrap()
    };
    assert_eq!(at("PUSH32"), reservoir - gas::body(0), "the reservoir when the frame starts");
    assert_eq!(at("STOP"), at("SSTORE") - slot, "the reservoir the SSTORE drew");
    // The pools change who pays, not what is billed.
    pin("reservoir_sstore", &traced, fresh_slot_ledgers());
}

/// A transaction to the identity precompile: its own frame is the precompile, which runs no
/// instruction and echoes its input.
#[test]
fn test_identity_precompile() {
    let precompile = Address::with_last_byte(4);
    let input = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
    let traced = Traced::run(
        funded(),
        call_tx(CALLER, precompile, input.clone(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    let ExecutionResult::Success { output, .. } = &traced.outcome.result else {
        panic!("the precompile succeeds: {:?}", traced.outcome.result);
    };
    assert_eq!(output.data(), &input, "identity echoes its input");
    let price = satin_precompiles()
        .get(&precompile)
        .and_then(|identity| identity.required_gas(&input))
        .expect("the Satin table prices identity");
    let expected = Ledgers {
        regular: gas::call_intrinsic(&input, false) + price,
        state: 0,
        history: gas::body(input.len()),
        floor: gas::call_floor(&input, false),
    };
    pin("identity_precompile", &traced, expected);
}

/// The c-kzg test vector `verify_kzg_proof_case_correct_proof_4_4`, as the precompile takes it:
/// `versioned_hash ++ z ++ y ++ commitment ++ proof`.
fn kzg_input() -> Vec<u8> {
    let commitment = hex!(
        "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca2\
         5f26936857bc3a7c2539ea8ec3a952b7"
    );
    let mut input = kzg::kzg_to_versioned_hash(&commitment).to_vec();
    input.extend(hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000"));
    input.extend(hex!("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9"));
    input.extend(commitment);
    input.extend(hex!(
        "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc216074\
         4faf0070725e00b60ad9a026a15b1a8c"
    ));
    input
}

/// A contract that `STATICCALL`s the KZG point evaluation precompile, the one Satin reprices: the
/// call tracer shows the precompile as a child frame that spent the Satin price.
#[test]
fn test_precompile_child_frame() {
    let input = kzg_input();
    let len = u8::try_from(input.len()).expect("192 bytes");
    let code = BytecodeBuilder::default()
        .mstore(0, &input)
        .append_many([PUSH0, PUSH0])
        .push_number(len)
        .append(PUSH0)
        .push_address(kzg::ADDRESS)
        .append_many([GAS_OP, STATICCALL, POP])
        .stop()
        .build();
    let traced = Traced::run(
        funded().account_code(CONTRACT, code),
        call_tx(CALLER, CONTRACT, Bytes::new(), U256::ZERO, TX_GAS),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    let price = satin_precompiles()
        .get(&kzg::ADDRESS)
        .and_then(|precompile| precompile.required_gas(&input))
        .expect("the Satin table prices KZG");
    let root = traced.call_frame(false);
    let [child] = root.calls.as_slice() else { panic!("one child frame: {root:?}") };
    assert_eq!(child.typ, "STATICCALL");
    assert_eq!(child.to, Some(kzg::ADDRESS));
    assert_eq!(child.error, None, "the proof verifies");
    assert_eq!(child.gas_used, U256::from(price), "the child spent the Satin price");
    let answer = child.output.as_ref().expect("the evaluation's answer");
    assert_eq!(answer.as_ref(), &kzg::RETURN_VALUE[..], "the evaluation's answer");
    // Six words stored (a `PUSH32`, a `PUSH8` of the offset and an `MSTORE` each) and the memory
    // they take, the call's five pushes and `GAS`, the precompile's warm access and its price, and
    // the `POP`.
    let words = gas::words(input.len() as u64);
    let stores = words * 3 * VERYLOW + gas::memory(words);
    let call = 2 * BASE + VERYLOW + BASE + VERYLOW + BASE + gas::warm_account() + price;
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], false) + stores + call + BASE,
        state: 0,
        history: gas::body(0),
        floor: gas::call_floor(&[], false),
    };
    pin("precompile_child", &traced, expected);
}

/// A deposit that mints and carries value to a contract which stops. The value is logged; the
/// mint is not. A deposit pays no history gas.
#[test]
fn test_deposit_transaction() {
    let mint = 1_000_u128;
    let value = U256::from(7);
    let mut tx = op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        value,
        gas_limit: TX_GAS,
        ..Default::default()
    });
    tx.deposit.source_hash = B256::repeat_byte(0x42);
    tx.deposit.mint = Some(mint);
    let traced = Traced::run(
        funded().account_code(CONTRACT, BytecodeBuilder::default().stop().build()),
        mega_evm::alloy_op_evm::OpTx(tx),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(traced.outcome.result.is_success(), "{:?}", traced.outcome.result);
    let history = (traced.outcome.gas.history, traced.outcome.gas.history_bytes);
    assert_eq!(history, (0, 0), "a deposit pays no history");
    let root = traced.call_frame(true);
    assert_logs(&root.logs, &[transfer_log(CALLER, CONTRACT, value)]);
    assert_log_index_counts_discarded(&traced, &[0]);
    let PreStateFrame::Diff(diff) = traced.prestate(true) else { panic!("the diff mode") };
    let before = diff.pre[&CALLER].balance.expect("the caller's balance");
    assert_eq!(
        diff.post[&CALLER].balance,
        Some(before + U256::from(mint) - value),
        "the caller holds its balance, plus the mint, less the value"
    );
    // A deposit pays its intrinsic gas and what it ran, here nothing, and no history.
    let expected = Ledgers {
        regular: gas::call_intrinsic(&[], true),
        state: 0,
        history: 0,
        floor: gas::call_floor(&[], true),
    };
    pin("deposit", &traced, expected);
}
