//! The transaction outcome: the result, the gas by ledger, the usage counted and the stop.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockGasCounters, EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaEvm,
    MegaLimitExceeded, MegaTransactionOutcome, WRITE_RECORD_SIZE,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const CONTRACT: Address = address!("0000000000000000000000000000000000600001");

fn writer() -> MemoryDatabase {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .stop()
        .build();
    MemoryDatabase::default().account_code(CONTRACT, code)
}

fn execute(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(call(CALLER, CONTRACT, U256::ZERO, gas_limit))
        .unwrap()
}

/// The ledgers split the raw spend, the receipt reports revm's gas used, and the reservoir the
/// sender gets back is reported.
#[test]
fn test_outcome_reports_the_ledgers() {
    let outcome = execute(writer(), EvmTxRuntimeLimits::no_limits(), 1_000_000_000);
    assert!(outcome.result.is_success());
    let gas = outcome.gas;
    let result_gas = outcome.result.gas();
    assert_eq!(gas.regular + gas.state + gas.history, result_gas.total_gas_spent());
    assert_eq!(gas.gas_used, result_gas.tx_gas_used());
    assert_eq!(gas.reservoir_remaining, 1_000_000_000 - TX_GAS_LIMIT_CAP);
    assert_eq!(gas.history, 0, "nothing prices history bytes yet");
    assert_eq!(outcome.usage, LimitUsage { data_size: 2 * WRITE_RECORD_SIZE, write_records: 2 });
    assert_eq!(outcome.limit_exceeded, None);

    let mut block = BlockGasCounters::default();
    block.record(&gas);
    block.record(&gas);
    assert_eq!(block.execution, 2 * gas.block_execution_gas());
    assert_eq!(block.state, 2 * gas.state);
}

/// A stopped transaction reports the stop.
#[test]
fn test_outcome_reports_the_stop() {
    let outcome =
        execute(writer(), EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(40), 1_000_000);
    assert!(!outcome.result.is_success());
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 40,
            used: 80,
            frame_local: false
        })
    );
    assert_eq!(outcome.usage, LimitUsage::ZERO, "the stopped transaction keeps nothing");
}

/// A contract reverting with `MegaLimitExceeded`'s bytes on its own is not a stop.
#[test]
fn test_spoofed_revert_data_is_not_a_stop() {
    let data: Bytes = MegaLimitExceeded { kind: 0, limit: 1 }.abi_encode().into();
    let code = BytecodeBuilder::default().revert_with_data(&data).build();
    let outcome = execute(
        MemoryDatabase::default().account_code(CONTRACT, code),
        EvmTxRuntimeLimits::no_limits(),
        1_000_000,
    );
    assert_eq!(outcome.result.output(), Some(&data));
    assert_eq!(outcome.limit_exceeded, None);
}

/// Counts the steps it sees.
#[derive(Default)]
struct Steps(usize);

impl<CTX> revm::Inspector<CTX, revm::interpreter::interpreter::EthInterpreter> for Steps {
    fn step(
        &mut self,
        _interp: &mut revm::interpreter::Interpreter<
            revm::interpreter::interpreter::EthInterpreter,
        >,
        _context: &mut CTX,
    ) {
        self.0 += 1;
    }
}

/// `execute_transaction` runs the inspector when one is enabled, and not otherwise.
#[test]
fn test_convenience_execution_methods_work() {
    let mut evm = MegaEvm::new(context(writer())).with_inspector(Steps::default());
    let inspected = evm.execute_transaction(call(CALLER, CONTRACT, U256::ZERO, 1_000_000)).unwrap();
    assert!(inspected.result.is_success());
    let steps = evm.inspector().0;
    assert!(steps > 0, "the enabled inspector ran");
    alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    let executed = evm.execute_transaction(call(CALLER, CONTRACT, U256::ZERO, 1_000_000)).unwrap();
    assert!(executed.result.is_success());
    assert_eq!(evm.inspector().0, steps, "the disabled inspector did not run");
    assert_eq!(inspected.gas, executed.gas);
}
