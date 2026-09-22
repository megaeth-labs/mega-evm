//! The compute figure: the regular gas a transaction spent, read off its `Gas` and nothing else.
//!
//! A transaction's raw spend splits into three ledgers — regular, state and history — and the
//! regular one is the compute every limit reads. It is `total − state − history`, from the result
//! revm's `Gas` settled into; there is no counter of its own beside it. The figure a block counts
//! is that ledger at least the EIP-7623 floor, with history taken out *before* the floor is
//! applied: the fork's own `block_regular_gas_used` is `max(total − state, floor)`, which carries
//! history, and taking history out of it afterwards could land below the floor.
//!
//! Every case runs below the execution cap, where the reservoir is empty and state and history
//! spill onto regular gas, and above it, where the reservoir pays them first.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, TX_GAS_LIMIT_CAP},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockGasCounters, MegaContext, MegaEvm, MegaGasUsage, MegaTransactionOutcome, TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{ADD, POP},
    interpreter::{interpreter::EthInterpreter, Interpreter},
    Inspector,
};

use crate::common::{call, call_with_data, context, execute, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000c00000");
const CALLEE: Address = address!("0000000000000000000000000000000000c00001");

/// The two gas limits every case runs at: below the execution cap and above it.
const GAS_LIMITS: [u64; 2] = [10_000_000, TX_GAS_LIMIT_CAP + 100_000_000];

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(1))
}

/// `count` rounds of `PUSH1 1; PUSH1 2; ADD; POP`, eleven gas each, then `STOP`.
fn arithmetic(count: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..count {
        code = code.push_number(1_u8).push_number(2_u8).append(ADD).append(POP);
    }
    code.stop().build()
}

/// The three ledgers split the raw spend, the regular one is what is neither state nor history,
/// and the figure a block counts is that ledger at least the floor.
fn assert_ledgers(name: &str, outcome: &MegaTransactionOutcome) {
    let gas = &outcome.gas;
    let result = outcome.result.gas();
    assert_eq!(
        gas.regular + gas.state + gas.history,
        result.total_gas_spent(),
        "{name}: the three ledgers split the raw spend",
    );
    assert_eq!(gas.block_execution_gas(), gas.regular.max(gas.floor), "{name}");
    let mut block = BlockGasCounters::default();
    block.record(gas);
    assert_eq!(block.execution, gas.block_execution_gas(), "{name}: the block counts that figure");
}

/* ---------- the floor ---------- */

/// A transaction that carries a kilobyte of calldata and runs nothing is bound by its floor: its
/// regular ledger is the intrinsic gas, far below the floor's sixty-four gas a byte. The block
/// counts the floor.
///
/// Its history — the body and the kilobyte, at the cost per history byte — is larger still, so
/// the order the two are taken out in decides the figure. History comes out first and the floor
/// applies to what is left: the block counts the floor. The fork's own figure keeps the history
/// in and lands far above the floor; taking the history out of that afterwards lands far below
/// it. Neither is what a block counts.
#[test]
fn test_a_floor_bound_transaction_counts_its_floor_and_history_comes_out_first() {
    if runs_at_measurement_prices() {
        return;
    }
    const CALLDATA: u64 = 1_000;
    for gas_limit in GAS_LIMITS {
        let tx = call_with_data(CALLER, CALLEE, Bytes::from(vec![0; CALLDATA as usize]), gas_limit);
        let outcome = execute(funded(), tx);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_ledgers("floor-bound", &outcome);

        let gas = outcome.gas;
        assert_eq!(gas.history, (TX_BODY_SIZE + CALLDATA) * COST_PER_HISTORY_BYTE);
        assert!(gas.floor > gas.regular, "{gas:?}: the floor binds the regular ledger");
        assert_eq!(gas.block_execution_gas(), gas.floor, "the block counts the floor");

        let fork_figure = outcome.result.gas().block_regular_gas_used();
        assert_eq!(fork_figure, gas.regular + gas.history, "the fork's figure carries history");
        assert!(fork_figure > gas.floor);
        assert!(
            fork_figure - gas.history < gas.floor,
            "taking history out after the floor would count less than the floor",
        );
        assert_eq!(gas.gas_used, gas.regular + gas.history, "the receipt is above its floor");
    }
}

/* ---------- an inspector that edits gas ---------- */

/// Charges `charge` and hands back `give_back` once, after the first instruction.
struct EditsGas {
    charge: u64,
    give_back: u64,
    done: bool,
}

impl EditsGas {
    const fn new(charge: u64, give_back: u64) -> Self {
        Self { charge, give_back, done: false }
    }
}

impl Inspector<MegaContext<MemoryDatabase>, EthInterpreter> for EditsGas {
    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<MemoryDatabase>,
    ) {
        if !self.done {
            self.done = true;
            assert!(interp.gas.record_regular_cost(self.charge));
            interp.gas.erase_cost(self.give_back);
        }
    }
}

/// An inspector that charges gas between two instructions, or hands some back, moves the receipt;
/// the compute figure moves with it, by the same amount, because both are read off the same
/// `Gas`. A counter kept beside `Gas` would not have seen the edit at all.
#[test]
fn test_gas_an_inspector_edits_moves_the_compute_figure_with_the_receipt() {
    let run = |inspector: EditsGas, gas_limit: u64| -> MegaGasUsage {
        let outcome = MegaEvm::new(context(funded().account_code(CALLEE, arithmetic(20))))
            .with_inspector(inspector)
            .execute_transaction(call(CALLER, CALLEE, U256::ZERO, gas_limit))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_ledgers("edited", &outcome);
        outcome.gas
    };

    for gas_limit in GAS_LIMITS {
        let baseline = run(EditsGas::new(0, 0), gas_limit);
        assert!(baseline.regular > baseline.floor + 86, "the floor is out of the way");

        for (charge, give_back) in [(126, 0), (0, 86)] {
            let edited = run(EditsGas::new(charge, give_back), gas_limit);
            let moved = |figure: fn(&MegaGasUsage) -> u64| {
                i128::from(figure(&edited)) - i128::from(figure(&baseline))
            };
            let expected = i128::from(charge) - i128::from(give_back);
            assert_eq!(moved(|gas| gas.gas_used), expected, "the receipt moves");
            assert_eq!(moved(|gas| gas.regular), expected, "the regular ledger moves with it");
            assert_eq!(moved(MegaGasUsage::block_execution_gas), expected, "and the block figure");
            assert_eq!((edited.state, edited.history), (baseline.state, baseline.history));
        }
    }
}
