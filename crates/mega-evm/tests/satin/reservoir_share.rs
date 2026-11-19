//! The reservoir a frame starts with: the transaction's own frame and a system call's outermost
//! frame take the whole of it, and every child of `CALL`, `CALLCODE`, `DELEGATECALL`,
//! `STATICCALL`, `CREATE` or `CREATE2` takes `remaining − remaining × 2 / 100` of what its caller
//! has left after every charge it made for the start, the history of the start's records
//! included, whatever regular gas the caller asks to forward. The caller keeps the rest while the
//! child runs, and gets it back on top of what the child left when the child settles.
//!
//! An inspector reads the reservoir each start hands its frame off the frame's input, which is
//! where revm puts the share it took out of the caller.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, PUSH0, PUSH1, PUSH20, STOP},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
    },
    Database, Inspector,
};

use crate::common::{
    body_history, call, context, history, reservoir_share, runs_at_measurement_prices,
};

const SENDER: Address = address!("0000000000000000000000000000000000720000");
const A: Address = address!("0000000000000000000000000000000000720001");
const B: Address = address!("0000000000000000000000000000000000720002");
const C: Address = address!("0000000000000000000000000000000000720003");

/// The gas a transaction carries above the execution cap: its reservoir before the body is paid.
const ABOVE_THE_CAP: u64 = 10_000_000;

/// Records the reservoir every frame start hands its frame, in the order the starts are made.
#[derive(Default)]
struct Shares {
    reservoirs: Vec<u64>,
    gas_limits: Vec<u64>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Shares {
    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.reservoirs.push(inputs.reservoir);
        self.gas_limits.push(inputs.gas_limit);
        None
    }

    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.reservoirs.push(inputs.reservoir());
        self.gas_limits.push(inputs.gas_limit());
        None
    }
}

/// `A` calls `B`, which calls `C`, each with all its gas and no value.
fn chain() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10_u64.pow(18)))
        .account_code(A, BytecodeBuilder::default().call(B, U256::ZERO).stop().build())
        .account_code(B, BytecodeBuilder::default().call(C, U256::ZERO).stop().build())
        .account_code(C, Bytes::from_static(&[STOP]))
}

/// The reservoir the transaction's own frame starts with: what the gas limit carries above the
/// execution cap, less the history of the body, which the reservoir pays first.
fn first_frame_reservoir() -> u64 {
    ABOVE_THE_CAP - body_history(0)
}

/// The transaction's own frame takes the whole reservoir; a child takes all but its caller's 2 per
/// cent, and a grandchild all but 2 per cent of what the child has left. Nothing spends any of it,
/// so it all comes back, and the transaction is reimbursed the whole.
#[test]
fn test_a_child_takes_the_reservoir_less_its_callers_two_percent_at_every_depth() {
    let mut evm = MegaEvm::new(context(chain())).with_inspector(Shares::default());
    let outcome = evm
        .execute_transaction(call(SENDER, A, U256::ZERO, TX_GAS_LIMIT_CAP + ABOVE_THE_CAP))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    let whole = first_frame_reservoir();
    let child = reservoir_share(whole);
    let grandchild = reservoir_share(child);
    assert_eq!(evm.inspector().reservoirs, [whole, child, grandchild]);
    assert!(whole * 2 / 100 > 0, "the caller keeps a part of this reservoir");
    assert_eq!(outcome.gas.reservoir_remaining, whole, "every share came back");
}

/// `A` calls `B` asking for one unit of regular gas, carrying `value`.
fn asking_for_one(value: u8) -> MemoryDatabase {
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH1, value, PUSH20])
        .append_many(B.into_array())
        .append_many([PUSH1, 1, CALL])
        .stop()
        .build();
    MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10_u64.pow(18)))
        .account_balance(A, U256::from(1))
        .account_code(A, code)
        .account_code(B, Bytes::from_static(&[STOP]))
}

/// The regular gas a call asks to forward does not bound the reservoir it hands its frame: a call
/// asking for one unit of gas still hands it the share. A call that moves value hands it the
/// share of what the history of the start's records left the caller — the two accounts the
/// transfer changes — and the stipend joins the forward.
#[test]
fn test_the_regular_gas_a_call_asks_for_does_not_bound_the_reservoir_it_forwards() {
    let whole = first_frame_reservoir();
    for (value, gas_limit, records) in [(0, 1, 0), (1, 1 + 2_300, 2)] {
        let mut evm =
            MegaEvm::new(context(asking_for_one(value))).with_inspector(Shares::default());
        let outcome = evm
            .execute_transaction(call(SENDER, A, U256::ZERO, TX_GAS_LIMIT_CAP + ABOVE_THE_CAP))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "value {value}: {:?}", outcome.result);
        let left = whole - history(WRITE_RECORD_SIZE * records);
        let shares = evm.inspector();
        assert_eq!(shares.reservoirs, [whole, reservoir_share(left)], "value {value}");
        assert_eq!(shares.gas_limits[1], gas_limit, "value {value}");
    }
}

/// A system call's outermost frame takes the whole reservoir the split leaves it, and the frame it
/// starts takes the share of it: the protocol's own work is exempt from the limits, not from the
/// split.
#[test]
fn test_a_system_calls_outermost_frame_takes_the_whole_reservoir_and_its_child_the_share() {
    use revm::{handler::system_call::SYSTEM_CALL_GAS_LIMIT, InspectSystemCallEvm};
    let mut evm = MegaEvm::new(context(chain())).with_inspector(Shares::default());
    let result = InspectSystemCallEvm::inspect_one_system_call_with_caller(
        &mut evm,
        SENDER,
        A,
        Bytes::new(),
    )
    .expect("the system call runs");
    assert!(result.is_success(), "{result:?}");
    let whole = SYSTEM_CALL_GAS_LIMIT - mega_evm::SYSTEM_CALL_REGULAR_GAS_LIMIT;
    let child = reservoir_share(whole);
    assert_eq!(evm.inspector().reservoirs, [whole, child, reservoir_share(child)]);
    assert_eq!(evm.ctx().additional_limit().reservoir_remaining(), whole, "it all came back");
}

/// The figures above at the spec's numbers: a 10,000,000 margin pays the body's 27,280 of history,
/// and the frames take 9,972,720, 9,773,266 and 9,577,801.
#[test]
fn test_the_shares_at_the_specs_numbers() {
    if runs_at_measurement_prices() {
        return;
    }
    let whole = first_frame_reservoir();
    assert_eq!(whole, 9_972_720);
    assert_eq!(reservoir_share(whole), 9_773_266);
    assert_eq!(reservoir_share(reservoir_share(whole)), 9_577_801);
}

/// The neutral configuration runs the fixture fork's schedule, which forwards the whole reservoir
/// to every child, as Ethereum's Amsterdam does; `no_limits()` on Satin's own configuration takes
/// out the limits and leaves the share as it is.
#[test]
fn test_the_neutral_configuration_forwards_the_whole_reservoir_and_no_limits_keeps_the_share() {
    use mega_evm::{
        test_utils::{neutral_cfg, neutralize_evm, zero_fee_l1_block_info},
        EthSpecId, EvmTxRuntimeLimits, MegaSpecId,
    };
    let gas_limit = TX_GAS_LIMIT_CAP + ABOVE_THE_CAP;

    let ctx = MegaContext::new(chain(), MegaSpecId::SATIN)
        .with_neutral_cfg(neutral_cfg(EthSpecId::AMSTERDAM).expect("a neutral fork"))
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits())
        .with_block(crate::common::block())
        .with_chain(zero_fee_l1_block_info());
    let mut evm = MegaEvm::new(ctx).with_inspector(Shares::default());
    neutralize_evm(&mut evm, EthSpecId::AMSTERDAM).expect("a neutral fork");
    let outcome = evm
        .execute_transaction(call(SENDER, A, U256::ZERO, gas_limit))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let [first, child, grandchild] = evm.inspector().reservoirs[..] else {
        panic!("three frames: {:?}", evm.inspector().reservoirs)
    };
    assert_eq!((child, grandchild), (first, first), "the whole reservoir, at every depth");

    let ctx = context(chain()).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    let mut evm = MegaEvm::new(ctx).with_inspector(Shares::default());
    evm.execute_transaction(call(SENDER, A, U256::ZERO, gas_limit))
        .expect("the transaction is valid");
    let whole = first_frame_reservoir();
    let child = reservoir_share(whole);
    assert_eq!(evm.inspector().reservoirs, [whole, child, reservoir_share(child)]);
}
