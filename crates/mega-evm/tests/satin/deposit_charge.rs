//! Deployed code is held to the limits only where `return_create` charges its state gas.
//!
//! `return_create` charges a creation's deposit in this order: the regular deposit cost, the
//! regular cost of hashing the code under EIP-8037, then the code's state gas — the reservoir
//! first, then regular gas — and last the code's history. A creation that cannot pay one of them
//! runs out of gas there, alone, and the chain keeps none of its code. The state-gas limit and the
//! data-size limit both hold the deposit before `return_create` commits it, and only once the
//! state gas is paid: a creation that cannot pay a charge up to it runs out of gas whatever the
//! limit, and its caller goes on. The history after it is held like a record's history after the
//! state gas of its slot: the limit comes first.
//!
//! Each case finds its boundary on the engine: the least gas `A` can call the creator `B` with for
//! `return_create` to charge the code's state gas — or, without EIP-8037, to deposit the code —
//! when no limit is set. The creation then has no regular gas left after it, so it is exactly
//! enough. With that gas a limit the deposit crosses stops the creation; one gas short the
//! creation runs out of gas, under the limit exactly as without it.
//!
//! Which charge the creation runs out of gas on follows from where the gas comes from. Above the
//! execution cap the reservoir pays the code's state gas and history, so the last regular charge
//! is the hash. Below it there is no reservoir, and the state gas is paid out of regular gas after
//! the hash. The regular deposit cost is zero under EIP-8037, so it binds only on a configuration
//! without it: the neutral one of Osaka, where no state gas is charged and the data-size limit is
//! the one that holds the code.

use alloy_primitives::{address, Address, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{neutral_cfg, neutralize_evm, BytecodeBuilder, MemoryDatabase},
    EthSpecId, EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaContext, MegaEvm,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, MLOAD, MSTORE, POP, PUSH0, RETURN, RETURNDATASIZE, SSTORE},
    context_interface::cfg::GasId,
    inspector::NoOpInspector,
    interpreter::{interpreter::EthInterpreter, CreateInputs, CreateOutcome},
    Database, Inspector,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000a00000");
/// Calls `B` with a chosen gas, keeps what `B` answers in slot 1, then writes slot 0.
const A: Address = address!("0000000000000000000000000000000000a00001");
/// Creates the code with everything it has, and answers with the size of what the creation
/// returned: a stop's revert data, or nothing.
const B: Address = address!("0000000000000000000000000000000000a00002");

/// The bytes the creation deploys.
const CODE_LEN: u16 = 1_024;

/// Below the execution cap: there is no reservoir, and every state charge is paid out of regular
/// gas.
const BELOW_CAP: u64 = 10_000_000;
/// Above the execution cap: the reservoir pays every state and history charge.
const ABOVE_CAP: u64 = TX_GAS_LIMIT_CAP + 100_000_000;

/// A frame cap below what the creation keeps with its code, and above what any other frame keeps.
const FRAME_CAP: u64 = 1_000;

/// Where the transaction runs.
#[derive(Clone, Copy, Debug)]
enum Setup {
    /// Satin's own configuration, with this gas limit.
    Satin(u64),
    /// The neutral configuration of Osaka, which has no EIP-8037 and charges the regular deposit
    /// cost.
    NeutralOsaka,
}

/// `A` calling `B` with `gas`.
///
/// The creation's init code touches memory far out before it returns the code, which costs it a
/// few thousand gas: `B` keeps a sixty-fourth of what it forwards, and that has to be enough for
/// it to answer however little the creation gives back.
fn world(gas: u64) -> MemoryDatabase {
    let init = BytecodeBuilder::default()
        .push_number(0x8000_u16)
        .append(MLOAD)
        .append(POP)
        .push_number(CODE_LEN)
        .push_number(0_u8)
        .append(RETURN)
        .build();
    let creator = BytecodeBuilder::default()
        .mstore(0, &init)
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(CREATE)
        .append(POP)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(MSTORE)
        .push_number(32_u8)
        .append(PUSH0)
        .append(RETURN)
        .build();
    let caller = BytecodeBuilder::default()
        .push_number(32_u8)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .push_number(gas)
        .append(CALL)
        .append(POP)
        .append(PUSH0)
        .append(MLOAD)
        .push_number(1_u8)
        .append(SSTORE)
        .sstore(U256::ZERO, U256::from(1))
        .stop()
        .build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_code(A, caller)
        .account_code(B, creator)
}

fn evm(
    setup: Setup,
    gas: u64,
    limits: EvmTxRuntimeLimits,
) -> MegaEvm<MemoryDatabase, NoOpInspector> {
    let ctx = context(world(gas));
    match setup {
        Setup::Satin(_) => MegaEvm::new(ctx.with_tx_runtime_limits(limits)),
        Setup::NeutralOsaka => {
            let cfg = neutral_cfg(EthSpecId::OSAKA).expect("a neutral fork");
            let mut evm = MegaEvm::new(ctx.with_neutral_cfg(cfg).with_tx_runtime_limits(limits));
            neutralize_evm(&mut evm, EthSpecId::OSAKA).expect("a neutral fork");
            evm
        }
    }
}

const fn gas_limit(setup: Setup) -> u64 {
    match setup {
        Setup::Satin(gas_limit) => gas_limit,
        Setup::NeutralOsaka => BELOW_CAP,
    }
}

/// Runs the transaction with `A` calling `B` with `gas`, under `limits`.
fn run(setup: Setup, gas: u64, limits: EvmTxRuntimeLimits) -> MegaTransactionOutcome {
    evm(setup, gas, limits)
        .execute_transaction(call(CALLER, A, U256::ZERO, gas_limit(setup)))
        .expect("the transaction is valid")
}

/// Whether the creation's code is at its address.
fn deployed(outcome: &MegaTransactionOutcome) -> bool {
    outcome.state.get(&B.create(0)).is_some_and(|account| !account.info.is_empty_code_hash())
}

/// The size of what `B`'s creation returned, as `A` kept it.
fn creation_answer(outcome: &MegaTransactionOutcome) -> U256 {
    outcome.state[&A].storage.get(&U256::from(1)).map(|slot| slot.present_value).unwrap_or_default()
}

/// The creation's frame once `return_create` is done with it.
#[derive(Clone, Copy, Debug, Default)]
struct CreationEnd {
    /// Whether the creation deposited its code.
    deposited: bool,
    /// Whether `return_create` charged the code's state gas: the only state charge made on the
    /// creation's own frame. A charge made and then undone by a later out-of-gas still shows.
    charged_state_gas: bool,
    /// The regular gas the creation had left.
    remaining: u64,
}

impl CreationEnd {
    /// Whether `return_create` got as far as the charge the limits hold the deposit before: the
    /// code's state gas, or, without EIP-8037, the deposit itself.
    const fn reached(&self) -> bool {
        self.deposited || self.charged_state_gas
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for CreationEnd {
    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let gas = outcome.result.gas;
        *self = Self {
            deposited: outcome.result.is_ok(),
            charged_state_gas: gas.state_gas_spent() > 0,
            remaining: gas.remaining(),
        };
    }
}

/// How the creation ends when `A` calls `B` with `gas` and no limit is set.
fn creation_end(setup: Setup, gas: u64) -> CreationEnd {
    let mut evm =
        evm(setup, gas, EvmTxRuntimeLimits::no_limits()).with_inspector(CreationEnd::default());
    evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit(setup))).unwrap();
    *evm.inspector()
}

/// The least gas `A` can call `B` with for `return_create` to reach the charge the limits hold the
/// deposit before, with no limit set; and the unlimited outcome one gas short of it.
///
/// At the boundary the creation has no regular gas left after that charge, so it is exactly
/// enough; one gas short the creation runs out of gas and deploys nothing.
fn boundary(setup: Setup) -> (u64, MegaTransactionOutcome) {
    let (mut short, mut enough) = (0, 5_000_000);
    assert!(
        !creation_end(setup, short).reached() && creation_end(setup, enough).reached(),
        "{setup:?}: the search brackets the boundary"
    );
    while enough - short > 1 {
        let middle = short + (enough - short) / 2;
        if creation_end(setup, middle).reached() {
            enough = middle;
        } else {
            short = middle;
        }
    }
    let end = creation_end(setup, enough);
    assert_eq!(end.remaining, 0, "{setup:?}: exactly enough: {end:?}");

    let outcome = run(setup, short, EvmTxRuntimeLimits::no_limits());
    assert!(outcome.result.is_success(), "{setup:?}: {:?}", outcome.result);
    assert!(!deployed(&outcome), "{setup:?}: one gas short, the creation runs out of gas");
    assert_eq!(creation_answer(&outcome), U256::ZERO, "{setup:?}: an out-of-gas returns nothing");
    (enough, outcome)
}

/// The state gas the creation holds once its code's state gas is charged: the created account
/// and the code.
fn creation_state_gas() -> u64 {
    crate::salt::entry(GasId::create_state_gas()) +
        u64::from(CODE_LEN) * crate::salt::entry(GasId::code_deposit_state_gas())
}

/// Under a state-gas limit one gas short of what the creation holds with its code, the creation
/// that reaches the charge is stopped, and the one that cannot runs out of gas as it does without
/// the limit: the transaction succeeds with no code deployed and the same gas.
fn assert_state_gas_limit_stands_aside(setup: Setup) {
    let (enough, short) = boundary(setup);
    let creation = creation_state_gas();
    assert!(short.gas.state > 0 && short.gas.state < creation, "{setup:?}: the slot alone fits");
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(creation - 1);

    let stopped = run(setup, enough, limits);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::StateGrowth,
        limit: creation - 1,
        used: creation,
        frame_local: false,
    };
    assert_eq!(stopped.limit_exceeded, Some(stop), "{setup:?}: exactly enough, the check fires");
    assert!(!deployed(&stopped), "{setup:?}");

    let ran_out = run(setup, enough - 1, limits);
    assert!(ran_out.result.is_success(), "{setup:?}: {:?}", ran_out.result);
    assert_eq!(ran_out.limit_exceeded, None, "{setup:?}: one gas short, it stands aside");
    assert!(!deployed(&ran_out), "{setup:?}");
    assert_eq!(creation_answer(&ran_out), U256::ZERO, "{setup:?}: the creation ran out of gas");
    assert_eq!(ran_out.gas, short.gas, "{setup:?}: as without the limit");
}

/// Under a frame cap the creation's code crosses, the creation that reaches the charge is stopped
/// alone — it answers with the stop's revert data — and the one that cannot runs out of gas as it
/// does without the cap.
fn assert_data_size_limit_stands_aside(setup: Setup) {
    let (enough, short) = boundary(setup);
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(FRAME_CAP);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: 0,
        used: 0,
        frame_local: true,
    };

    let stopped = run(setup, enough, limits);
    assert!(stopped.result.is_success(), "{setup:?}: {:?}", stopped.result);
    assert_eq!(
        creation_answer(&stopped),
        U256::from(stop.revert_data().len()),
        "{setup:?}: exactly enough, the check fires and the creation reverts with the stop",
    );
    assert!(!deployed(&stopped), "{setup:?}");

    let ran_out = run(setup, enough - 1, limits);
    assert!(ran_out.result.is_success(), "{setup:?}: {:?}", ran_out.result);
    assert_eq!(creation_answer(&ran_out), U256::ZERO, "{setup:?}: one gas short, it stands aside");
    assert!(!deployed(&ran_out), "{setup:?}");
    assert_eq!(ran_out.gas, short.gas, "{setup:?}: as without the limit");
    assert_eq!(ran_out.usage, short.usage, "{setup:?}: and it keeps what it keeps without it");
}

/// Above the execution cap the reservoir pays the code's state gas: a creation that cannot pay
/// the hash of its code runs out of gas on it, whatever the state-gas limit.
#[test]
fn test_a_creation_that_cannot_pay_the_hash_runs_out_of_gas_under_the_state_gas_limit() {
    let setup = Setup::Satin(ABOVE_CAP);
    let (enough, _) = boundary(setup);
    assert!(
        deployed(&run(setup, enough, EvmTxRuntimeLimits::no_limits())),
        "the hash is the last charge the frame's own gas pays"
    );
    assert_state_gas_limit_stands_aside(setup);
}

/// Below the execution cap the code's state gas is paid out of regular gas: a creation that can
/// pay the hash of its code but not its state gas runs out of gas on the state gas, whatever the
/// state-gas limit.
///
/// With exactly enough for the state gas the check fires, although without a limit the creation
/// would then run out of gas on the code's history: that is charged after the state gas, as a
/// record's history is after its slot's, and the limit holds what was charged before it.
#[test]
fn test_a_creation_that_cannot_pay_its_state_gas_runs_out_of_gas_under_the_state_gas_limit() {
    let setup = Setup::Satin(BELOW_CAP);
    let (enough, _) = boundary(setup);
    let end = creation_end(setup, enough);
    assert!(end.charged_state_gas && !end.deposited, "the history is what it cannot pay: {end:?}");
    assert_state_gas_limit_stands_aside(setup);
}

/// The data-size limit counts deployed code behind the same check: a creation that cannot pay the
/// hash of its code, or its state gas, runs out of gas whatever the limit.
#[test]
fn test_a_creation_that_cannot_pay_for_its_code_runs_out_of_gas_under_the_data_size_limit() {
    assert_data_size_limit_stands_aside(Setup::Satin(ABOVE_CAP));
    assert_data_size_limit_stands_aside(Setup::Satin(BELOW_CAP));
}

/// Without EIP-8037 the deposit's only charge is the regular deposit cost: a creation that cannot
/// pay it runs out of gas whatever the data-size limit.
#[test]
fn test_a_creation_that_cannot_pay_the_deposit_cost_runs_out_of_gas_under_the_data_size_limit() {
    let end = creation_end(Setup::NeutralOsaka, boundary(Setup::NeutralOsaka).0);
    assert!(end.deposited && !end.charged_state_gas, "no state gas without EIP-8037: {end:?}");
    assert_data_size_limit_stands_aside(Setup::NeutralOsaka);
}
