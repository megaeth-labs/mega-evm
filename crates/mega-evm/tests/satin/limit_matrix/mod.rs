//! The resource-limit matrix of the specification: one test per case of its rules, written from
//! `docs/spec/upgrades/satin.md` alone, independently of the engine — the KV-update and
//! state-growth limits, the reservoir and pool shares, the stop, the block budgets, deposits,
//! keyless deployment and the same-transaction destruction table. A test's doc states the rule it
//! pins.
//!
//! The numbers below are the page's. A test does not import an engine constant for a figure the
//! page states. Gas room around a scenario may follow the schedule at the prices in effect, so
//! the same test still runs when a measurement build changes a byte price.

pub(super) mod block;
pub(super) mod deposits;
pub(super) mod destruction;
pub(super) mod forwarding;
pub(super) mod keyless;
pub(super) mod limits;
pub(super) mod records;
pub(super) mod stop;

use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{note_price_guard, op_transaction, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaLimitExceeded, MegaTransaction,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, CREATE2, DELEGATECALL, GAS, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE,
    },
    context::{
        result::{ExecutionResult, HaltReason},
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Interpreter,
    },
    primitives::KECCAK_EMPTY,
    Database, Inspector,
};

use crate::common::{self, context};

/// `TX_DATA_LIMIT` and `BLOCK_DATA_LIMIT` on the page.
pub(super) const TX_DATA_LIMIT: u64 = 13_107_200;
/// `TX_KV_UPDATE_LIMIT` and `BLOCK_KV_UPDATE_LIMIT` on the page.
pub(super) const TX_KV_UPDATE_LIMIT: u64 = 500_000;
/// `TX_STATE_GROWTH_LIMIT` and `BLOCK_STATE_GROWTH_LIMIT` on the page.
pub(super) const TX_STATE_GROWTH_LIMIT: u64 = 1_000;
/// History gas per byte. SALT does not scale it.
pub(super) const HISTORY_GAS_PER_BYTE: u64 = 88;
/// One write record, in bytes. Also its history bytes.
pub(super) const WRITE_RECORD_SIZE: u64 = 40;
/// Data size of one EIP-7708 transfer log, which pays no history.
pub(super) const TRANSFER_LOG_SIZE: u64 = 160;
/// History gas of one write record at the spec's byte price: `40 × 88`.
pub(super) const RECORD_HISTORY: u64 = 3_520;
/// The body's fixed bytes: the envelope and five records.
pub(super) const BODY_BYTES: u64 = 310;
/// History gas of an empty-calldata body at the spec's byte price: `310 × 88`.
pub(super) const BODY_HISTORY: u64 = 27_280;
/// State gas of one fresh slot at the minimum bucket: `64 × 1,530`.
pub(super) const SLOT_STATE_GAS_M1: u64 = 97_920;
/// State gas of one new account leaf at the minimum bucket: `120 × 1,530`.
pub(super) const ACCOUNT_LEAF_STATE_GAS_M1: u64 = 183_600;
/// State gas of one deployed code byte at the minimum bucket.
pub(super) const CODE_DEPOSIT_STATE_PER_BYTE: u64 = 1_530;
/// State gas of one delegation indicator, charged at most once per authority.
pub(super) const INDICATOR_STATE_GAS: u64 = 35_190;
/// Regular gas of a value-transferring `CALL`. The 2,300 stipend is already inside it.
pub(super) const VALUE_TRANSFER_GAS: u64 = 11_300;
/// Stipend added to a value-transferring `CALL` or `CALLCODE` after the cap.
pub(super) const CALL_STIPEND: u64 = 2_300;
/// Warm account access, charged as the `CALL` opcode's static gas.
pub(super) const WARM_READ: u64 = 100;
/// History allowance of a value `CALL` below the transaction's own frame, in bytes.
pub(super) const HISTORY_ALLOWANCE_BYTES: u64 = 160;
/// Gas of that allowance: `160 × 88`. It is not gas and not reservoir.
pub(super) const HISTORY_ALLOWANCE_GAS: u64 = 14_080;
/// The execution cap. Gas above it is the reservoir. Not a transaction-size cap.
pub(super) const EXECUTION_CAP: u64 = 200_000_000;
/// Percent of reservoir a caller keeps. The division truncates toward zero.
pub(super) const RESERVOIR_KEPT_PERCENT: u64 = 2;

/// The caller every transaction is sent from.
pub(super) const CALLER: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1000");
/// The contract a transaction calls.
pub(super) const CONTRACT: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1001");
/// A callee.
pub(super) const CHILD: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1002");
/// A callee of [`CHILD`].
pub(super) const GRAND: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1003");
/// A frame started after another child returned.
pub(super) const NEXT: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1004");
/// Where a `SELFDESTRUCT` sends the balance.
pub(super) const BENEFICIARY: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1005");
/// An account that already exists and holds no code worth running.
pub(super) const EXISTING: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab1006");
/// The state-growth probe: a callee that fills fresh slots until its frame budget stops it.
pub(super) const PROBE: Address =
    alloy_primitives::address!("0000000000000000000000000000000000ab100f");

/// Reservoir a child receives: `remaining − remaining × 2 / 100`.
pub(super) const fn reservoir_share(remaining: u64) -> u64 {
    remaining - remaining * RESERVOIR_KEPT_PERCENT / 100
}

/// Frame budget a child receives: `remaining × 98 / 100`, rounded down.
pub(super) const fn frame_share(remaining: u64) -> u64 {
    remaining * 98 / 100
}

/// Regular-gas cap: the same subtraction as [`reservoir_share`].
pub(super) const fn gas_cap(remaining: u64) -> u64 {
    reservoir_share(remaining)
}

/// Bytecode that stores `1` into slots `1..=n` and stops.
///
/// The loop is `PUSH2 n`, a `JUMPDEST` at pc 3, and a back-edge to it. The exit `JUMPDEST` is pc
/// 20 (`PUSH1 0x14`). Slot 0 is left untouched.
pub(super) fn store_n_slots(n: u16) -> Bytes {
    Bytes::from(store_n_prefix(n, 1, true))
}

/// The loop of [`store_n_slots`] storing `value`, without the trailing `STOP` when `stop` is false,
/// so a caller can append another opcode.
pub(super) fn store_n_prefix(n: u16, value: u8, stop: bool) -> Vec<u8> {
    let mut code = vec![
        0x61,
        (n >> 8) as u8,
        (n & 0xff) as u8,
        0x5b, // JUMPDEST at pc 3
        0x80, // DUP1
        0x15, // ISZERO
        0x60,
        0x14, // PUSH1 20, the exit JUMPDEST
        0x57, // JUMPI
        0x60,
        value,
        0x81, // DUP2
        0x55, // SSTORE
        0x60,
        0x01, // PUSH1 1
        0x90, // SWAP1
        0x03, // SUB
        0x60,
        0x03, // PUSH1 3
        0x56, // JUMP
        0x5b, // JUMPDEST at pc 20
        0x50, // POP
    ];
    if stop {
        code.push(0x00);
    }
    code
}

/// [`store_n_prefix`] placed at `origin`, so a loop appended after other code jumps to its own
/// `JUMPDEST`s. The jump targets are `PUSH2`. `PUSH2` costs the same static gas as the `PUSH1`
/// the origin-zero loop uses.
pub(super) fn store_n_prefix_at(origin: usize, n: u16, value: u8, stop: bool) -> Vec<u8> {
    let entry = u16::try_from(origin + 3).expect("the loop's entry fits a PUSH2");
    let exit = u16::try_from(origin + 22).expect("the loop's exit fits a PUSH2");
    let mut code = vec![
        0x61,
        (n >> 8) as u8,
        n as u8,
        0x5b, // JUMPDEST at origin + 3
        0x80, // DUP1
        0x15, // ISZERO
        0x61,
        (exit >> 8) as u8,
        exit as u8,
        0x57, // JUMPI
        0x60,
        value,
        0x81, // DUP2
        0x55, // SSTORE
        0x60,
        0x01, // PUSH1 1
        0x90, // SWAP1
        0x03, // SUB
        0x61,
        (entry >> 8) as u8,
        entry as u8,
        0x56, // JUMP
        0x5b, // JUMPDEST at origin + 22
        0x50, // POP
    ];
    if stop {
        code.push(0x00);
    }
    code
}

/// Code that ends a scenario by calling [`PROBE`] with all its gas and returning what the probe
/// returned.
///
/// The probe tries to fill [`TX_STATE_GROWTH_LIMIT`] fresh slots of its own, more than any child
/// budget, so it always stops, kind 3, at its budget: `floor((1,000 − g) × 98 / 100)` when the
/// scenario before it kept a state-growth net of `g`. For `g` up to 49 the budget pins `g`.
pub(super) fn then_probe(scenario: impl AsRef<[u8]>) -> Bytes {
    let mut code = scenario.as_ref().to_vec();
    code.extend_from_slice(
        &BytecodeBuilder::default().call(PROBE, U256::ZERO).return_returndata().build(),
    );
    Bytes::from(code)
}

/// The probe's code: [`TX_STATE_GROWTH_LIMIT`] fresh slots.
pub(super) fn probe_code() -> Bytes {
    store_n_slots(TX_STATE_GROWTH_LIMIT as u16)
}

/// Asserts that the scenario before [`then_probe`] kept a state-growth net of `kept`: the probe
/// stopped at `floor((1,000 − kept) × 98 / 100)`.
pub(super) fn assert_growth(outcome: &MegaTransactionOutcome, kept: u64) {
    assert!(kept < 50, "the probe pins a net below 50");
    assert_eq!(
        returned_stop(outcome),
        MegaLimitExceeded { kind: 3, limit: frame_share(TX_STATE_GROWTH_LIMIT - kept) },
        "the probe's budget is the share of what a kept net of {kept} leaves"
    );
}

/// Runs `scenario` twice: ending in `STOP`, for its figures, and ending in [`then_probe`], for
/// the state-growth net it kept. `db` builds the database around the code [`CONTRACT`] runs.
/// Under the state-growth limit the probe is stopped and keeps nothing; an engine without one
/// keeps its slots and their charges, so the scenario's figures are read off the plain run.
pub(super) fn plain_and_probed(
    scenario: &[u8],
    db: impl Fn(Bytes) -> MemoryDatabase,
) -> Option<(MegaTransactionOutcome, MegaTransactionOutcome)> {
    let gas = probe_gas(10)?;
    let mut plain = scenario.to_vec();
    plain.push(0x00);
    let outcome = execute(db(Bytes::from(plain)), call_tx(gas));
    let probed = execute(db(then_probe(scenario)), call_tx(gas));
    assert!(probed.result.is_success(), "the probed run: {:?}", probed.result);
    Some((outcome, probed))
}

/// Gas for a scenario of `slots` fresh slots followed by the probe.
pub(super) fn probe_gas(slots: u64) -> Option<u64> {
    slots_gas(slots + TX_STATE_GROWTH_LIMIT)
}

/// `CALL` of `target` carrying `value`, forwarding exactly `gas`, with no calldata. Leaves the
/// success flag.
pub(super) fn call_with_gas(
    code: BytecodeBuilder,
    target: Address,
    value: U256,
    gas: u32,
) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_u256(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// Copies the last call's return data to memory at `offset`.
pub(super) fn keep_returndata(code: BytecodeBuilder, offset: u16) -> BytecodeBuilder {
    code.append_many([RETURNDATASIZE, PUSH0]).push_number(offset).append(RETURNDATACOPY)
}

/// `RETURN` of the 256 bytes from `0x100`, where [`keep_returndata`] kept two return datas, at
/// `0x100` and `0x180`.
pub(super) fn return_kept(code: BytecodeBuilder) -> BytecodeBuilder {
    code.push_number(0x100_u16).push_number(0x100_u16).append(RETURN)
}

/// The two stops a successful transaction returned through [`return_kept`].
pub(super) fn returned_stops(
    outcome: &MegaTransactionOutcome,
) -> (MegaLimitExceeded, MegaLimitExceeded) {
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("expected a success that returns two stops, got {:?}", outcome.result);
    };
    let data = output.data();
    let decode = |at: usize| {
        MegaLimitExceeded::abi_decode(&data[at..at + 68]).unwrap_or_else(|err| {
            panic!("return data at {at} is not MegaLimitExceeded ({err}); output {data:?}");
        })
    };
    (decode(0), decode(0x80))
}

/// `CALL` of `target` with all remaining gas, then `RETURN` of its return data.
pub(super) fn forward_to(target: Address) -> Bytes {
    BytecodeBuilder::default().call(target, U256::ZERO).return_returndata().build()
}

/// `DELEGATECALL` of `target` with all remaining gas. Leaves the success flag.
pub(super) fn delegatecall(target: Address) -> BytecodeBuilder {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(target)
        .append_many([GAS, DELEGATECALL])
}

/// A database with [`CALLER`] funded and [`PROBE`] installed. Each `(address, code)` exists with
/// balance 1, so a call to it is not an account-creation.
pub(super) fn world(code: &[(Address, Bytes)]) -> MemoryDatabase {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u128.pow(24)))
        .account_code(PROBE, probe_code())
        .account_balance(PROBE, U256::from(1));
    for (address, bytecode) in code {
        db = db.account_code(*address, bytecode.clone()).account_balance(*address, U256::from(1));
    }
    db
}

/// A call from [`CALLER`] to [`CONTRACT`].
pub(super) fn call_tx(gas_limit: u64) -> MegaTransaction {
    common::call(CALLER, CONTRACT, U256::ZERO, gas_limit)
}

/// A creation from [`CALLER`].
pub(super) fn create_tx(init_code: Bytes, gas_limit: u64) -> MegaTransaction {
    common::create(CALLER, init_code, gas_limit)
}

/// An EIP-2930 call that warms `warm` from the start, so the measured `CALL` pays no cold access.
pub(super) fn warm_call(gas_limit: u64, warm: &[Address]) -> MegaTransaction {
    let access_list = AccessList::from(
        warm.iter()
            .map(|address| AccessListItem { address: *address, storage_keys: vec![] })
            .collect::<Vec<_>>(),
    );
    OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip2930 as u8,
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit,
        access_list,
        ..Default::default()
    }))
}

/// Gas that covers `slots` fresh slots above the execution cap at the prices in effect: the
/// slots' state gas, the history of the body and one record each, and regular gas for the loop.
///
/// `None` when that sum does not fit a `u64`. The caller leaves a price-guard note and returns.
pub(super) fn gas_for_slots(slots: u64) -> Option<u64> {
    let state = slots.checked_mul(common::slot_state_gas())?;
    let bytes = BODY_BYTES.checked_add(slots.checked_mul(WRITE_RECORD_SIZE)?)?;
    let history = common::history(bytes);
    let regular = slots.checked_mul(40_000)?.checked_add(8_000_000)?;
    state.checked_add(history)?.checked_add(regular)?.checked_add(EXECUTION_CAP)
}

/// [`gas_for_slots`], or a note and `None` when the scenario does not fit.
pub(super) fn slots_gas(slots: u64) -> Option<u64> {
    match gas_for_slots(slots) {
        Some(gas) => Some(gas),
        None => {
            note_price_guard("the fresh-slot scenario does not fit one gas limit");
            None
        }
    }
}

/// Whether this process prices bytes at the spec's constants.
///
/// A test that pins a spec gas figure returns early otherwise.
/// [`common::runs_at_measurement_prices`] leaves the grid's note.
pub(super) fn at_spec_prices() -> bool {
    !common::runs_at_measurement_prices()
}

/// Runs `tx` and returns its outcome.
pub(super) fn execute(db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    common::execute(db, tx)
}

/// Runs `tx` under `limits`.
pub(super) fn execute_under(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    common::execute_under(db, tx, limits)
}

/// The stop a reverted transaction carries.
pub(super) fn reverted_stop(outcome: &MegaTransactionOutcome) -> MegaLimitExceeded {
    let ExecutionResult::Revert { output, .. } = &outcome.result else {
        panic!("expected a revert, got {:?}", outcome.result);
    };
    MegaLimitExceeded::abi_decode(output).unwrap_or_else(|err| {
        panic!("revert data is not MegaLimitExceeded ({err}); output {output:?}");
    })
}

/// The stop a successful transaction returned from a frame it caught.
pub(super) fn returned_stop(outcome: &MegaTransactionOutcome) -> MegaLimitExceeded {
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("expected a success that returns the stop, got {:?}", outcome.result);
    };
    MegaLimitExceeded::abi_decode(output.data()).unwrap_or_else(|err| {
        panic!("return data is not MegaLimitExceeded ({err}); output {output:?}");
    })
}

/// The value of `slot` after `outcome`, or zero when the account has no entry for it.
pub(super) fn slot_value(outcome: &MegaTransactionOutcome, address: Address, slot: u64) -> U256 {
    outcome
        .state
        .get(&address)
        .and_then(|account| account.storage.get(&U256::from(slot)))
        .map(|entry| entry.present_value())
        .unwrap_or(U256::ZERO)
}

/// Whether `outcome` destroyed `address`: an account created in the transaction that a
/// `SELFDESTRUCT` deleted. Its code can still be in the state entry, so code is not the test.
pub(super) fn is_destroyed(outcome: &MegaTransactionOutcome, address: Address) -> bool {
    outcome.state.get(&address).is_some_and(|account| account.is_selfdestructed())
}

/// Whether `address` has code that is not empty.
pub(super) fn has_code(outcome: &MegaTransactionOutcome, address: Address) -> bool {
    outcome.state.get(&address).is_some_and(|account| {
        account.info.code.as_ref().is_some_and(|code| !code.is_empty()) ||
            (account.info.code_hash != alloy_primitives::B256::ZERO &&
                account.info.code_hash != KECCAK_EMPTY)
    })
}

/// The nonce `outcome` leaves `address`, or zero when the account was not loaded.
pub(super) fn nonce_of(outcome: &MegaTransactionOutcome, address: Address) -> u64 {
    outcome.state.get(&address).map(|account| account.info.nonce).unwrap_or(0)
}

/// The balance `outcome` leaves `address`, or zero when the account was not loaded.
pub(super) fn balance_of(outcome: &MegaTransactionOutcome, address: Address) -> U256 {
    outcome.state.get(&address).map(|account| account.info.balance).unwrap_or(U256::ZERO)
}

/// Whether `outcome` halted because a frame ran out of gas.
pub(super) fn halted_out_of_gas(outcome: &MegaTransactionOutcome) -> bool {
    matches!(
        outcome.result,
        ExecutionResult::Halt { reason: MegaHaltReason::Base(HaltReason::OutOfGas(_)), .. }
    )
}

/// One forwarded call: the gas the caller held at the opcode, the gas the child was given, and
/// the reservoir the child was given.
#[derive(Clone, Debug, Default)]
pub(super) struct Forward {
    want: Option<Address>,
    pending: Option<u64>,
    /// Regular gas the caller held at the opcode, before its static gas.
    pub(super) held: Option<u64>,
    /// Regular gas the child was given.
    pub(super) gas_limit: Option<u64>,
    /// Reservoir the child was given.
    pub(super) reservoir: Option<u64>,
    /// Every call, in order: target, gas limit, reservoir. The first is the transaction's own
    /// frame.
    pub(super) calls: Vec<(Address, u64, u64)>,
    /// Every call's end, in order: target, and the reservoir its frame ended with, before its
    /// caller (or, for the transaction's own frame, the transaction) settles it.
    pub(super) call_ends: Vec<(Address, u64)>,
    /// The creation's gas limit and reservoir, when one started.
    pub(super) created: Option<(u64, u64)>,
    /// The regular gas the creation returned with, when one ended.
    pub(super) create_end_remaining: Option<u64>,
}

impl Forward {
    /// Records the call whose target is `want`, and every call besides.
    pub(super) fn targeting(want: Address) -> Self {
        Self { want: Some(want), ..Self::default() }
    }

    /// Records every call and the creation.
    pub(super) fn any() -> Self {
        Self::default()
    }

    /// The reservoir the first call to `target` was given.
    pub(super) fn reservoir_given(&self, target: Address) -> u64 {
        let call = self.calls.iter().find(|(to, ..)| *to == target);
        call.unwrap_or_else(|| panic!("no call to {target}: {:?}", self.calls)).2
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Forward {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        let opcode = interp.bytecode.opcode();
        if matches!(opcode, CALL | DELEGATECALL | CREATE | CREATE2) {
            self.pending = Some(interp.gas.remaining());
        }
    }

    fn call(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        self.calls.push((inputs.target_address, inputs.gas_limit, inputs.reservoir));
        if self.want.is_none_or(|want| inputs.target_address == want) {
            self.held = self.pending;
            self.gas_limit = Some(inputs.gas_limit);
            self.reservoir = Some(inputs.reservoir);
        }
        None
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.call_ends.push((inputs.target_address, outcome.result.gas.reservoir()));
    }

    fn create(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.created = Some((inputs.gas_limit(), inputs.reservoir()));
        self.held = self.pending;
        self.gas_limit = Some(inputs.gas_limit());
        self.reservoir = Some(inputs.reservoir());
        None
    }

    fn create_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.create_end_remaining = Some(outcome.result.gas.remaining());
    }
}

/// Runs `tx` over `db` and returns the outcome together with what [`Forward`] recorded.
pub(super) fn execute_forward(
    db: MemoryDatabase,
    tx: MegaTransaction,
    probe: Forward,
) -> (MegaTransactionOutcome, Forward) {
    let mut evm = MegaEvm::new(context(db)).with_inspector(probe);
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let probe = evm.inspector().clone();
    (outcome, probe)
}

/// Same as [`execute_forward`], under `limits`.
pub(super) fn execute_forward_under(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
    probe: Forward,
) -> (MegaTransactionOutcome, Forward) {
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits)).with_inspector(probe);
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let probe = evm.inspector().clone();
    (outcome, probe)
}

/// A call from [`CALLER`] to [`CONTRACT`] whose gas limit is the execution cap plus `reservoir`
/// plus the body's history, so the reservoir left after the body is `reservoir`.
pub(super) fn call_with_reservoir(reservoir: u64) -> MegaTransaction {
    let gas = EXECUTION_CAP + common::body_history(0) + reservoir;
    call_tx(gas)
}
