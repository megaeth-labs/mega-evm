//! The dispatch itself: what admits a call, what an admitted call costs, and what an observer
//! sees of it.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::{
        IMegaAccessControl, IMegaLimitControl, IOracle, ACCESS_CONTROL_ADDRESS,
        LIMIT_CONTROL_ADDRESS, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
    },
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, InjectedDbError},
    MegaEvm, MegaTransactionError,
};
use revm::{
    bytecode::opcode::{CALL, POP, STOP},
    context::{result::EVMError, ContextTr},
    inspector::Inspector,
    interpreter::{interpreter_types::InterpreterTypes, CallInputs, CallOutcome},
};

use crate::common::{call_tx, context, output, run, system_db, with_contract, CONTRACT};

const REMAINING_COMPUTE_GAS: [u8; 4] = IMegaLimitControl::remainingComputeGasCall::SELECTOR;
const IS_DISABLED: [u8; 4] = IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR;

/// An account that exists and runs no code, so a call to it costs what the `CALL` opcode costs
/// and nothing more.
const PLAIN_TARGET: Address = address!("0x0000000000000000000000000000000000c0ffee");

/// Code that calls `to` with `data`, forwarding `gas`, and stops.
fn call_with_gas(to: Address, data: &[u8], gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0x0, data)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(to)
        .push_number(gas)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// What a transaction running `code` spends.
fn spent(code: Bytes) -> u64 {
    let db = with_contract(code).account_code(PLAIN_TARGET, Bytes::new());
    let result = run(db, call_tx(CONTRACT, [], U256::ZERO));
    assert!(result.result.is_success(), "{:?}", result.result);
    result.result.gas().total_gas_spent()
}

/// An intercepted call costs what the `CALL` opcode that made it costs: the dispatch itself
/// records nothing.
#[test]
fn test_an_intercepted_call_costs_what_a_plain_call_costs() {
    let intercepted = call_with_gas(LIMIT_CONTROL_ADDRESS, &REMAINING_COMPUTE_GAS, 1_000_000);
    let plain = call_with_gas(PLAIN_TARGET, &REMAINING_COMPUTE_GAS, 1_000_000);
    assert_eq!(
        intercepted.len(),
        plain.len(),
        "the two programs differ only in the address they call",
    );
    assert_eq!(spent(intercepted), spent(plain));
}

/// How much gas the caller forwards to an intercepted call does not change what it costs: the
/// forwarded gas comes back in full.
#[test]
fn test_the_forwarded_gas_of_an_intercepted_call_comes_back() {
    let low = call_with_gas(LIMIT_CONTROL_ADDRESS, &REMAINING_COMPUTE_GAS, 1_000_000);
    let high = call_with_gas(LIMIT_CONTROL_ADDRESS, &REMAINING_COMPUTE_GAS, 9_000_000);
    assert_eq!(low.len(), high.len(), "the two programs differ only in the gas they forward");
    assert_eq!(spent(low), spent(high));
}

/// Admission is the four selector bytes: the exact selector and the selector followed by
/// anything are both intercepted, and an input too short to hold one is not.
#[test]
fn test_admission_is_the_selector_alone() {
    let intercepted = |data: Vec<u8>| {
        let result = run(system_db(), call_tx(ACCESS_CONTROL_ADDRESS, &data, U256::ZERO));
        // The interceptor answers `false`; the deployed bytecode reverts.
        result.result.is_success()
    };
    assert!(intercepted(IS_DISABLED.to_vec()), "the exact selector");
    assert!(intercepted(IS_DISABLED.iter().copied().chain([0xff]).collect()), "one byte after it");
    assert!(
        intercepted(IS_DISABLED.iter().copied().chain([0_u8; 32]).collect()),
        "a padded argument after it",
    );
    assert!(!intercepted(IS_DISABLED[..3].to_vec()), "three bytes are not a selector");
    assert!(!intercepted(Vec::new()), "no input is not a selector");
    assert!(!intercepted(vec![0xde, 0xad, 0xbe, 0xef]), "another selector");
}

/// The dispatch reaches a system contract only at its own address: the same selectors sent
/// anywhere else run whatever code is there.
#[test]
fn test_the_address_admits_the_dispatch() {
    let db = system_db().account_code(PLAIN_TARGET, Bytes::new());
    let result = run(db, call_tx(PLAIN_TARGET, IS_DISABLED, U256::ZERO));
    assert!(result.result.is_success());
    assert!(
        result.result.output().is_none_or(|output| output.is_empty()),
        "an account without code answers nothing, so nothing was intercepted",
    );
}

/// Records which addresses an inspector sees called, and in which order they end.
#[derive(Default)]
struct CallTracker {
    calls: Vec<Address>,
    call_ends: Vec<Address>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for CallTracker {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls.push(inputs.target_address);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends.push(inputs.target_address);
    }
}

/// An intercepted call is a call like any other to an observer: the inspector sees it start and
/// end, paired with the frame that made it.
#[test]
fn test_an_inspector_sees_an_intercepted_call() {
    let code = call_with_gas(LIMIT_CONTROL_ADDRESS, &REMAINING_COMPUTE_GAS, 1_000_000);
    let mut evm = MegaEvm::new(context(with_contract(code))).with_inspector(CallTracker::default());
    let result =
        evm.transact_raw(call_tx(CONTRACT, [], U256::ZERO)).expect("the transaction is valid");

    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(evm.inspector().calls, vec![CONTRACT, LIMIT_CONTROL_ADDRESS]);
    assert_eq!(evm.inspector().call_ends, vec![LIMIT_CONTROL_ADDRESS, CONTRACT]);
}

/// A database error while the system address is read during validation fails the transaction
/// with that error: a database blip must not let the transaction through as a deposit.
#[test]
fn test_a_database_error_reading_the_system_address_fails_the_transaction() {
    let mut db = ErrorInjectingDatabase::new(system_db());
    db.fail_on_account = Some(MEGA_SYSTEM_ADDRESS);

    let tx = {
        let mut tx = call_tx(
            ORACLE_CONTRACT_ADDRESS,
            IOracle::getSlotCall { slot: U256::ZERO }.abi_encode(),
            U256::ZERO,
        );
        tx.0.base.caller = MEGA_SYSTEM_ADDRESS;
        tx.0.base.chain_id =
            Some(revm::context::CfgEnv::<mega_evm::MegaSpecId>::default().chain_id);
        tx
    };
    let result: Result<_, EVMError<InjectedDbError, MegaTransactionError>> =
        MegaEvm::new(context(db)).transact_raw(tx);

    match result {
        Err(EVMError::Database(error)) => {
            assert!(format!("{error}").contains(&MEGA_SYSTEM_ADDRESS.to_string()), "{error}");
        }
        other => panic!("expected a database error, got: {other:?}"),
    }
}

/// The value policy is the interceptor's, not the account's: a value-bearing transaction to a
/// control contract is refused with `NonZeroTransfer()` before any value moves.
#[test]
fn test_a_value_bearing_transaction_is_refused_before_the_value_moves() {
    let db = system_db();
    let mut tx = call_tx(LIMIT_CONTROL_ADDRESS, REMAINING_COMPUTE_GAS, U256::from(1));
    tx.0.base.caller = crate::common::CALLER;
    let result = run(db, tx);

    let revm::context::result::ExecutionResult::Revert { output, .. } = &result.result else {
        panic!("the transaction did not revert: {:?}", result.result);
    };
    assert_eq!(output[..], IMegaLimitControl::NonZeroTransfer::SELECTOR);
    assert!(
        result
            .state
            .get(&LIMIT_CONTROL_ADDRESS)
            .is_none_or(|account| account.info.balance.is_zero()),
        "the refused value stayed with its sender",
    );
}

/// A `STATICCALL` to a system contract from inside a static frame is intercepted too: nothing
/// about the answer writes state.
#[test]
fn test_a_nested_static_call_is_intercepted() {
    use revm::bytecode::opcode::STATICCALL;

    // An outer contract that STATICCALLs an inner one, which STATICCALLs the system contract.
    let inner_code = BytecodeBuilder::default()
        .mstore(0x0, IS_DISABLED)
        .push_number(32_u64)
        .push_number(0x20_u64)
        .push_number(4_u64)
        .push_number(0_u64)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64)
        .append(STATICCALL)
        .append(POP)
        .push_number(32_u64)
        .push_number(0x20_u64)
        .append(revm::bytecode::opcode::RETURN)
        .build();
    let inner = address!("0x0000000000000000000000000000000000300002");
    let outer_code = BytecodeBuilder::default()
        .push_number(32_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(inner)
        .push_number(1_000_000_u64)
        .append(STATICCALL)
        .append(POP)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(revm::bytecode::opcode::RETURN)
        .build();

    let db = with_contract(outer_code).account_code(inner, inner_code);
    let result = run(db, call_tx(CONTRACT, [], U256::ZERO));
    assert_eq!(
        output(&result),
        Bytes::from(IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(
            &false
        )),
    );
}

/// A transaction that reaches no system contract is untouched by the dispatch.
#[test]
fn test_a_transaction_that_touches_no_system_contract_is_untouched() {
    let code = BytecodeBuilder::default().append(STOP).build();
    let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    assert!(result.result.is_success());
}

/// The two system contracts without an interceptor run their bytecode, whatever the call
/// carries and whichever scheme makes it: an unknown selector reverts in their dispatcher, where
/// a call to an account without code would succeed.
#[test]
fn test_the_contracts_without_an_interceptor_run_their_bytecode() {
    use mega_evm::system::{HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, SEQUENCER_REGISTRY_ADDRESS};
    use revm::bytecode::opcode::{CALLCODE, DELEGATECALL, STATICCALL};

    for address in [HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, SEQUENCER_REGISTRY_ADDRESS] {
        // As a transaction, with a selector neither contract knows and with none at all.
        for data in [vec![0xde, 0xad, 0xbe, 0xef], Vec::new()] {
            let result = run(system_db(), call_tx(address, &data, U256::ZERO));
            assert!(!result.result.is_success(), "{address} answered {data:?} without its code");
        }

        // Through every call scheme, including the two the scheme guard refuses.
        for scheme in [CALL, STATICCALL, CALLCODE, DELEGATECALL] {
            let code = crate::common::calls_with(scheme, address, &[0xde, 0xad, 0xbe, 0xef], 0);
            let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
            let outcome = output(&result);
            let (status, _) = crate::common::split_outcome(&outcome);
            assert!(!status, "{address} answered scheme {scheme:#x} without its code");
        }

        // A call that carries value is the bytecode's to refuse.
        let code = crate::common::calls_with(CALL, address, &[0xde, 0xad, 0xbe, 0xef], 1);
        let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
        let outcome = output(&result);
        let (status, _) = crate::common::split_outcome(&outcome);
        assert!(!status, "{address} took value without its code");
    }
}
