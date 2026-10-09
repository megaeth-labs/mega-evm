//! Deployed code is charged in the spec's order — the regular per-byte cost (zero), the hash at 6
//! gas per word, the state gas at the cost per state byte, the history at the cost per history
//! byte — and held to the limits only once every part of its deposit is charged.
//!
//! A creation that cannot pay one of the four runs out of gas there, alone, and the chain keeps
//! none of its code [S5.14]. Which charge it runs out on follows from where the gas comes from.
//! Above the execution cap the reservoir pays the code's state gas and history, so the only
//! regular charge at the deposit is the hash. Below it there is no reservoir, and the state gas
//! and then the history are paid out of regular gas after the hash [S5.13]. The regular deposit
//! cost is zero under EIP-8037, so it binds only on a configuration without it: the neutral one
//! of Osaka, where no state gas is charged and the data-size limit is the one that holds the
//! code.
//!
//! Every gas figure here is built by hand from the spec and the schedule, never read off a run
//! of the engine. The creation's regular budget is set exactly: `A` calls the creator `B` with an
//! explicit gas, `B` spends a known amount before its `CREATE`, and the EIP-150 split forwards all
//! but a 64th of what `B` holds, which [`holding_for`] inverts. Each case is one gas short of a
//! charge, or exactly enough, and reads every ledger against its derivation. The state-gas limit
//! and the data-size limit both hold the deposit after the last of those charges and before
//! `return_create` commits it: with exactly enough gas a limit the deposit crosses stops the
//! creation, and one gas short the creation runs out of gas under the limit exactly as without it.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{neutral_cfg, neutralize_evm, BytecodeBuilder, MemoryDatabase},
    EthSpecId, EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm,
    MegaTransactionOutcome, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, POP, PUSH0, RETURN, STOP},
    context_interface::cfg::GasId,
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, CreateInputs, CreateOutcome, InstructionResult,
        InterpreterResult,
    },
    Database, Inspector,
};

use crate::{
    common::{call, context, history, history_is_free, runs_at_measurement_prices, state_is_free},
    salt::entry,
};

const CALLER: Address = address!("0000000000000000000000000000000000a00000");
/// Calls `B` with a chosen gas and stops.
const A: Address = address!("0000000000000000000000000000000000a00001");
/// Creates the code with all but a 64th of what it has, and stops.
const B: Address = address!("0000000000000000000000000000000000a00002");

/// The words of zeros the creation deploys, and their bytes.
///
/// Enough that the deposit, and not `B`, binds at any byte price: below the execution cap `B`
/// pays the history of the creation's two write records out of the 64th it keeps, and the code's
/// state gas alone leaves it a 64th of millions.
const DEPLOYED_WORDS: u64 = 100;
const CODE_LEN: u64 = DEPLOYED_WORDS * 32;

/// The reservoir the runs above the execution cap carry: ample for every charge here.
const RESERVOIR: u64 = 100_000_000;

/// A frame cap below what the creation keeps with its code, and above what any other frame keeps.
const FRAME_CAP: u64 = 1_000;

/// Where the transaction runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pool {
    /// Satin above the execution cap: the reservoir pays every state and history charge, and the
    /// hash is the only regular charge of the deposit.
    AboveCap,
    /// Satin below the execution cap: there is no reservoir, and every charge is regular gas.
    BelowCap,
    /// The neutral configuration of Osaka, which has no EIP-8037 and charges the regular deposit
    /// cost.
    NeutralOsaka,
}

impl Pool {
    /// The transaction's gas limit: above the cap by [`RESERVOIR`], or well below it, or Osaka's
    /// own ceiling (EIP-7825 holds a transaction to 2^24 gas).
    const fn gas_limit(self) -> u64 {
        match self {
            Self::AboveCap => TX_GAS_LIMIT_CAP + RESERVOIR,
            Self::BelowCap => 30_000_000,
            Self::NeutralOsaka => 10_000_000,
        }
    }

    /// The intrinsic regular gas of the call to `A`: `TX_BASE_COST` 12,000 and 3,000 for the
    /// recipient's access under EIP-2780; 21,000 before it.
    const fn intrinsic(self) -> u64 {
        match self {
            Self::AboveCap | Self::BelowCap => 12_000 + 3_000,
            Self::NeutralOsaka => 21_000,
        }
    }
}

/* ---------- the figures by hand ---------- */

/// What `A` spends on its own: five `PUSH0`, a `PUSH20`, a `PUSH4`, a `CALL` to a cold account
/// carrying no value (100 warm plus 2,500 cold), and a `POP`.
const A_REGULAR: u64 = 5 * 2 + 3 + 3 + (100 + 2_500) + 2;

/// What `B` spends before its `CREATE` splits the gas: a `PUSH32` and a `PUSH8` and the `MSTORE`
/// that puts the init code in memory, with the first word of memory; three `PUSH8`; EIP-3860's 2
/// gas for the one word of init code; and `CREATE`'s 32,000.
const B_BEFORE_THE_SPLIT: u64 = 3 + 3 + (3 + 3) + 3 * 3 + 2 + 32_000;

/// What `B` spends after the creation returns: the `POP` of its answer.
const B_AFTER: u64 = 2;

/// What the init code spends: `PUSH2`, `PUSH0`, and the `RETURN`'s expansion of memory to
/// [`DEPLOYED_WORDS`] words, `3 w + w² / 512`.
const INIT_EXECUTION: u64 = 3 + 2 + 3 * DEPLOYED_WORDS + DEPLOYED_WORDS * DEPLOYED_WORDS / 512;

/// The hashing cost of the deposit: 6 gas per 32-byte word [S5.13].
const HASH: u64 = 6 * DEPLOYED_WORDS;

/// Osaka's regular deposit cost: 200 gas per byte.
const OSAKA_DEPOSIT: u64 = 200 * CODE_LEN;

/// The state gas of the deposited code at the cost per state byte in effect: 1,530 per byte at
/// the spec's price.
fn code_state_gas() -> u64 {
    entry(GasId::code_deposit_state_gas()) * CODE_LEN
}

/// The history of the deposited code at the cost per history byte in effect: 88 per byte at the
/// spec's price.
fn code_history_gas() -> u64 {
    entry(GasId::code_deposit_history_gas()) * CODE_LEN
}

/// The state gas of the account the creation adds, charged to `B` by its `CREATE`: 183,600 at
/// the spec's price.
fn created_account_state_gas() -> u64 {
    entry(GasId::create_state_gas())
}

/// The regular gas the creation needs to deposit its code with nothing left: its init code, then
/// the charges of the deposit that regular gas pays in `pool`.
fn deposits_at(pool: Pool) -> u64 {
    match pool {
        Pool::AboveCap => INIT_EXECUTION + HASH,
        Pool::BelowCap => INIT_EXECUTION + HASH + code_state_gas() + code_history_gas(),
        Pool::NeutralOsaka => INIT_EXECUTION + OSAKA_DEPOSIT,
    }
}

/// The least gas `B` can hold at its `CREATE`'s split for the creation to be forwarded exactly
/// `forward`: EIP-150 forwards all but a 64th, `r - r / 64`, which `64 (f / 63) + f mod 63`
/// inverts.
fn holding_for(forward: u64) -> u64 {
    let holding = 64 * (forward / 63) + forward % 63;
    assert_eq!(holding - holding / 64, forward, "the split inverted");
    holding
}

/// The gas `A` calls `B` with for the creation to be forwarded exactly `forward` in `pool`: what
/// `B` must hold at the split, plus what it spends before it. Below the cap the created account's
/// state gas spills onto `B`'s regular gas before the split too; above it the reservoir pays it,
/// and Osaka charges none.
fn call_gas(pool: Pool, forward: u64) -> u64 {
    let spilled = match pool {
        Pool::BelowCap => created_account_state_gas(),
        Pool::AboveCap | Pool::NeutralOsaka => 0,
    };
    holding_for(forward) + B_BEFORE_THE_SPLIT + spilled
}

/// The regular gas a deposited creation spent as regular gas: its init code and the deposit's
/// regular charges. The state gas and history that spilled onto its regular budget below the cap
/// stay on their own ledgers.
fn regular_deposit(pool: Pool) -> u64 {
    match pool {
        Pool::AboveCap | Pool::BelowCap => INIT_EXECUTION + HASH,
        Pool::NeutralOsaka => INIT_EXECUTION + OSAKA_DEPOSIT,
    }
}

/// The regular ledger of a run whose creation spent `spent` of its forward as regular gas: the
/// whole forward when it halted (what was charged and given back comes back as regular gas, and
/// the halt consumes it), or [`regular_deposit`] when it deposited with nothing left.
fn regular(pool: Pool, spent: u64) -> u64 {
    pool.intrinsic() + A_REGULAR + B_BEFORE_THE_SPLIT + B_AFTER + spent
}

/* ---------- the world ---------- */

/// The creation's init code: `PUSH2 CODE_LEN; PUSH0; RETURN`, five bytes, deploying [`CODE_LEN`]
/// zero bytes.
fn init_code() -> Bytes {
    BytecodeBuilder::default().push_number(CODE_LEN as u16).append_many([PUSH0, RETURN]).build()
}

/// `A` calling `B` with `gas`, and `B` creating [`init_code`].
fn world(gas: u64) -> MemoryDatabase {
    let init = init_code();
    let creator = BytecodeBuilder::default()
        .mstore(0, &init)
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build();
    let caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .push_number(u32::try_from(gas).expect("the forward fits a PUSH4"))
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(A, caller)
        .account_code(B, creator)
}

fn evm(
    pool: Pool,
    forward: u64,
    limits: EvmTxRuntimeLimits,
) -> MegaEvm<MemoryDatabase, NoOpInspector> {
    let ctx = context(world(call_gas(pool, forward)));
    match pool {
        Pool::AboveCap | Pool::BelowCap => MegaEvm::new(ctx.with_tx_runtime_limits(limits)),
        Pool::NeutralOsaka => {
            let cfg = neutral_cfg(EthSpecId::OSAKA).expect("a neutral fork");
            let mut evm = MegaEvm::new(ctx.with_neutral_cfg(cfg).with_tx_runtime_limits(limits));
            neutralize_evm(&mut evm, EthSpecId::OSAKA).expect("a neutral fork");
            evm
        }
    }
}

/// Runs the transaction with the creation forwarded `forward`, under `limits`.
fn run(pool: Pool, forward: u64, limits: EvmTxRuntimeLimits) -> MegaTransactionOutcome {
    evm(pool, forward, limits)
        .execute_transaction(call(CALLER, A, U256::ZERO, pool.gas_limit()))
        .expect("the transaction is valid")
}

/// The code at the creation's address, if any.
fn deployed_code(outcome: &MegaTransactionOutcome) -> Option<Bytes> {
    outcome.state.get(&B.create(0)).filter(|account| !account.info.is_empty_code_hash()).map(
        |account| account.info.code.clone().expect("the account's code is loaded").original_bytes(),
    )
}

/// Whether the creation's code is at its address.
fn deployed(outcome: &MegaTransactionOutcome) -> bool {
    deployed_code(outcome).is_some()
}

/// The creation's frame once `return_create` is done with it.
#[derive(Clone, Debug, Default)]
struct CreationEnd {
    /// How the creation ended, as its caller sees it.
    result: Option<InstructionResult>,
    /// Whether the creation deposited its code.
    deposited: bool,
    /// Whether `return_create` charged the code's state gas: the only state charge made on the
    /// creation's own frame. A charge made and then undone by a later out-of-gas still shows.
    charged_state_gas: bool,
    /// The regular gas the creation had left.
    remaining: u64,
    /// What the creation reverted with, if it reverted.
    reverted_with: Option<Bytes>,
}

impl CreationEnd {
    fn of(result: &InterpreterResult) -> Self {
        Self {
            result: Some(result.result),
            deposited: result.is_ok(),
            charged_state_gas: result.gas.state_gas_spent() > 0,
            remaining: result.gas.remaining(),
            reverted_with: result.is_revert().then(|| result.output.clone()),
        }
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for CreationEnd {
    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        *self = Self::of(&outcome.result);
    }
}

/// How the creation ends when forwarded `forward` and no limit is set.
fn creation_end(pool: Pool, forward: u64) -> CreationEnd {
    creation_end_under(pool, forward, EvmTxRuntimeLimits::no_limits())
}

/// How the creation ends when forwarded `forward`, under `limits`.
fn creation_end_under(pool: Pool, forward: u64, limits: EvmTxRuntimeLimits) -> CreationEnd {
    let mut evm = evm(pool, forward, limits).with_inspector(CreationEnd::default());
    evm.execute_transaction(call(CALLER, A, U256::ZERO, pool.gas_limit())).unwrap();
    evm.inspector().clone()
}

/* ---------- what each outcome looks like, by hand ---------- */

/// The history a transaction pays in `pool` for `bytes`: the cost per history byte in effect, or
/// nothing on the neutral configuration, which prices no history.
fn history_in(pool: Pool, bytes: u64) -> u64 {
    match pool {
        Pool::AboveCap | Pool::BelowCap => history(bytes),
        Pool::NeutralOsaka => 0,
    }
}

/// Holds a run to the ledgers of a creation that spent `spent` of its forward as regular gas, and
/// to the receipt and the reservoir those ledgers imply.
#[track_caller]
fn assert_ledgers(
    name: &str,
    pool: Pool,
    spent: u64,
    outcome: &MegaTransactionOutcome,
    state: u64,
    history_bytes: u64,
) {
    let history = history_in(pool, history_bytes);
    assert_eq!(outcome.gas.regular, regular(pool, spent), "{name}: the regular ledger");
    assert_eq!(outcome.gas.state, state, "{name}: the state ledger");
    assert_eq!(outcome.gas.history, history, "{name}: the history ledger");
    assert_eq!(
        outcome.gas.gas_used,
        regular(pool, spent) + state + history,
        "{name}: the receipt is the three ledgers, with nothing refunded and no floor binding",
    );
    let reservoir = match pool {
        Pool::AboveCap => RESERVOIR - state - history,
        Pool::BelowCap | Pool::NeutralOsaka => 0,
    };
    assert_eq!(outcome.gas.reservoir_remaining, reservoir, "{name}: the reservoir");
}

/// A creation forwarded `forward` in `pool` runs out of gas at its deposit and leaves no code:
/// its caller goes on, the creator's nonce record is all the transaction keeps beyond its body,
/// and every ledger is what the halt leaves — the forward consumed, no state gas, the body and
/// one record of history [S5.14].
///
/// `left` is what the creation held when the charge it cannot pay was attempted — one gas less
/// than that charge, or nothing — which the creation's frame still shows at its end, and which
/// prices the charge; `state_charged` says whether the deposit's state gas was charged before
/// the halt, which tells a creation short of its history from one short of its state gas
/// [S5.13].
#[track_caller]
fn assert_runs_out(name: &str, pool: Pool, forward: u64, left: u64, state_charged: bool) {
    let end = creation_end(pool, forward);
    assert_eq!(
        end.result,
        Some(InstructionResult::OutOfGas),
        "{name}: the creation halts: {end:?}"
    );
    assert!(!end.deposited, "{name}: {end:?}");
    assert_eq!(end.remaining, left, "{name}: what it held at the charge it cannot pay: {end:?}");
    assert_eq!(end.charged_state_gas, state_charged, "{name}: {end:?}");

    let outcome = run(pool, forward, EvmTxRuntimeLimits::no_limits());
    assert!(outcome.result.is_success(), "{name}: the caller survives: {:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None, "{name}");
    assert!(!deployed(&outcome), "{name}: no code is left");
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: TX_BODY_SIZE + WRITE_RECORD_SIZE, write_records: 1 },
        "{name}: the creator's nonce record outlives the creation, nothing else does",
    );
    assert_ledgers(name, pool, forward, &outcome, 0, TX_BODY_SIZE + WRITE_RECORD_SIZE);
}

/// A creation forwarded `forward` in `pool` deposits its code with nothing left: the transaction
/// keeps the creation's two records and the code, the state ledger holds the created account and
/// the code, and the history ledger the body, the two records and the code.
#[track_caller]
fn assert_deposits(name: &str, pool: Pool, forward: u64) {
    let end = creation_end(pool, forward);
    assert_eq!(end.result, Some(InstructionResult::Return), "{name}: {end:?}");
    assert!(end.deposited, "{name}: {end:?}");
    assert_eq!(end.remaining, 0, "{name}: exactly enough: {end:?}");
    let state = match pool {
        Pool::AboveCap | Pool::BelowCap => created_account_state_gas() + code_state_gas(),
        Pool::NeutralOsaka => 0,
    };
    assert_eq!(end.charged_state_gas, state > 0, "{name}: {end:?}");

    let outcome = run(pool, forward, EvmTxRuntimeLimits::no_limits());
    assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None, "{name}");
    assert_eq!(
        deployed_code(&outcome).map(|code| code.len() as u64),
        Some(CODE_LEN),
        "{name}: the code is at its address",
    );
    let bytes = TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + CODE_LEN;
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: bytes, write_records: 2 },
        "{name}: the creator's nonce, the created account, and the code",
    );
    assert_ledgers(name, pool, regular_deposit(pool), &outcome, state, bytes);
}

/* ---------- the four charges, one gas short of each ---------- */

/// Above the execution cap the reservoir pays the code's state gas and history, so the hash is
/// the only regular charge of the deposit: one gas short of it the creation runs out of gas with
/// no state gas charged, and one gas more deposits the code with nothing left. The boundary is
/// the init code's cost plus 6 gas per word of code, by hand (`independent`) [S5.13] [S5.14].
#[test]
fn test_a_creation_one_gas_short_of_the_hash_runs_out_of_gas_above_the_cap() {
    let enough = deposits_at(Pool::AboveCap);
    assert_runs_out("one gas short of the hash", Pool::AboveCap, enough - 1, HASH - 1, false);
    assert_deposits("the hash paid exactly", Pool::AboveCap, enough);
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(enough, 324 + 600, "the init code and the hash of 100 words");
}

/// Below the execution cap the code's state gas is paid out of regular gas right after the hash:
/// one gas short of it the creation runs out of gas with the hash paid and no state gas charged,
/// and one gas more charges the state gas and runs out on the history instead. The boundary is
/// the init code, the hash and 1,530 per byte of code, by hand (`constants`: the cost per state
/// byte in effect) [S5.13] [S5.14].
#[test]
fn test_a_creation_one_gas_short_of_its_state_gas_runs_out_of_gas_with_the_hash_paid() {
    // A deposit that adds no state gas has no state gas to be short of.
    if state_is_free() {
        return;
    }
    let state_paid = INIT_EXECUTION + HASH + code_state_gas();
    assert_runs_out(
        "one gas short of the state gas",
        Pool::BelowCap,
        state_paid - 1,
        code_state_gas() - 1,
        false,
    );
    assert_runs_out("the state gas paid exactly", Pool::BelowCap, state_paid, 0, true);
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(code_state_gas(), 3_200 * 1_530, "the state gas of 3,200 bytes");
}

/// Below the execution cap the history is the last charge of the deposit: one gas short of it
/// the creation runs out of gas with its state gas charged, and one gas more deposits the code
/// with nothing left. The boundary is the init code, the hash, the state gas and 88 per byte of
/// code, by hand (`constants`: the two byte prices in effect) [S5.13] [S5.14].
#[test]
fn test_a_creation_one_gas_short_of_its_history_runs_out_of_gas_with_the_state_gas_paid() {
    // A deposit that pays no history has no history to be short of, and one that adds no state
    // gas has no state charge to show.
    if history_is_free() || state_is_free() {
        return;
    }
    let enough = deposits_at(Pool::BelowCap);
    assert_runs_out(
        "one gas short of the history",
        Pool::BelowCap,
        enough - 1,
        code_history_gas() - 1,
        true,
    );
    assert_deposits("the history paid exactly", Pool::BelowCap, enough);
    if runs_at_measurement_prices() {
        return;
    }
    assert_eq!(code_history_gas(), 3_200 * 88, "the history of 3,200 bytes");
    assert_eq!(enough, 324 + 600 + 4_896_000 + 281_600);
}

/* ---------- the limits stand aside ---------- */

/// Under a state-gas limit one gas short of what the creation holds with its code, the creation
/// forwarded exactly enough is stopped, and the one forwarded one gas less runs out of gas as it
/// does without the limit: the transaction succeeds with no code deployed and the same gas.
///
/// The stop latches the transaction, so the whole transaction reverts whichever frame crossed.
/// The creation's own end tells the deposit's check from a later one: the deposit is held where
/// it is made when the creation itself reverts with the stop.
fn assert_state_gas_limit_stands_aside(pool: Pool) {
    let enough = deposits_at(pool);
    let creation = created_account_state_gas() + code_state_gas();
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(creation - 1);

    let stopped = run(pool, enough, limits);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::StateGrowth,
        limit: creation - 1,
        used: creation,
        frame_local: false,
    };
    assert_eq!(stopped.limit_exceeded, Some(stop), "{pool:?}: exactly enough, the check fires");
    assert!(!deployed(&stopped), "{pool:?}");
    let end = creation_end_under(pool, enough, limits);
    assert!(!end.deposited, "{pool:?}: the deposit is refused: {end:?}");
    assert_eq!(
        end.reverted_with,
        Some(stop.revert_data()),
        "{pool:?}: the creation that made the deposit is the one the stop ends",
    );

    let short = run(pool, enough - 1, EvmTxRuntimeLimits::no_limits());
    let ran_out = run(pool, enough - 1, limits);
    assert!(ran_out.result.is_success(), "{pool:?}: {:?}", ran_out.result);
    assert_eq!(ran_out.limit_exceeded, None, "{pool:?}: one gas short, it stands aside");
    assert!(!deployed(&ran_out), "{pool:?}");
    assert_eq!(ran_out.gas, short.gas, "{pool:?}: as without the limit");
    assert_eq!(ran_out.usage, short.usage, "{pool:?}: and it keeps what it keeps without it");
}

/// Under a frame cap the creation's code crosses, the creation forwarded exactly enough is
/// stopped alone — it reverts with the stop's revert data, and its caller goes on — and the one
/// forwarded one gas less runs out of gas as it does without the cap.
fn assert_data_size_limit_stands_aside(pool: Pool) {
    let enough = deposits_at(pool);
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(FRAME_CAP);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: 0,
        used: 0,
        frame_local: true,
    };

    let stopped = run(pool, enough, limits);
    assert!(stopped.result.is_success(), "{pool:?}: {:?}", stopped.result);
    assert_eq!(stopped.limit_exceeded, None, "{pool:?}: a frame budget latches nothing");
    assert!(!deployed(&stopped), "{pool:?}");
    let end = creation_end_under(pool, enough, limits);
    assert!(!end.deposited, "{pool:?}: {end:?}");
    assert_eq!(
        end.reverted_with.as_ref().map(|data| data.len()),
        Some(stop.revert_data().len()),
        "{pool:?}: exactly enough, the check fires and the creation reverts with the stop: {end:?}",
    );

    let short = run(pool, enough - 1, EvmTxRuntimeLimits::no_limits());
    let ran_out = run(pool, enough - 1, limits);
    assert!(ran_out.result.is_success(), "{pool:?}: {:?}", ran_out.result);
    assert_eq!(
        creation_end_under(pool, enough - 1, limits).result,
        Some(InstructionResult::OutOfGas),
        "{pool:?}: one gas short, it stands aside",
    );
    assert!(!deployed(&ran_out), "{pool:?}");
    assert_eq!(ran_out.gas, short.gas, "{pool:?}: as without the limit");
    assert_eq!(ran_out.usage, short.usage, "{pool:?}: and it keeps what it keeps without it");
}

/// Above the execution cap the reservoir pays the code's state gas and history: a creation that
/// cannot pay the hash of its code runs out of gas on it, whatever the state-gas limit.
#[test]
fn test_a_creation_that_cannot_pay_the_hash_runs_out_of_gas_under_the_state_gas_limit() {
    // A deposit that adds no state gas has no state-gas limit to cross.
    if state_is_free() {
        return;
    }
    let pool = Pool::AboveCap;
    assert_runs_out("one gas short of the hash", pool, deposits_at(pool) - 1, HASH - 1, false);
    assert_state_gas_limit_stands_aside(pool);
}

/// Below the execution cap the code's state gas and then its history are paid out of regular gas:
/// a creation that can pay its state gas but not its history runs out of gas on the history,
/// whatever the state-gas limit. The limit holds the deposit once all of it is paid, not at the
/// state gas: a creation that could pay the state gas alone would otherwise be stopped by a limit
/// it never reaches.
#[test]
fn test_a_creation_that_cannot_pay_its_history_runs_out_of_gas_under_the_state_gas_limit() {
    // A deposit that pays no history has no history to run out of gas on, and one that adds no
    // state gas has no state-gas limit to cross.
    if history_is_free() || state_is_free() {
        return;
    }
    let pool = Pool::BelowCap;
    assert_runs_out(
        "one gas short of the history",
        pool,
        deposits_at(pool) - 1,
        code_history_gas() - 1,
        true,
    );
    assert_state_gas_limit_stands_aside(pool);
}

/// The data-size limit counts deployed code behind the same check: a creation that cannot pay the
/// hash of its code, or its history, runs out of gas whatever the limit.
#[test]
fn test_a_creation_that_cannot_pay_for_its_code_runs_out_of_gas_under_the_data_size_limit() {
    assert_data_size_limit_stands_aside(Pool::AboveCap);
    assert_data_size_limit_stands_aside(Pool::BelowCap);
}

/// Without EIP-8037 the deposit's only charge is the regular deposit cost, 200 gas per byte: a
/// creation one gas short of it runs out of gas, one gas more deposits the code with nothing
/// left, and a creation that cannot pay it runs out of gas whatever the data-size limit.
#[test]
fn test_a_creation_that_cannot_pay_the_deposit_cost_runs_out_of_gas_under_the_data_size_limit() {
    let pool = Pool::NeutralOsaka;
    let enough = deposits_at(pool);
    assert_runs_out(
        "one gas short of the deposit cost",
        pool,
        enough - 1,
        OSAKA_DEPOSIT - 1,
        false,
    );
    assert_deposits("the deposit cost paid exactly", pool, enough);
    assert_data_size_limit_stands_aside(pool);
}
