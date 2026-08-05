//! `ModExp` pricing for zero-base/zero-modulus inputs across the whole spec arc.
//!
//! `MINI_REX` adopted the Osaka / EIP-7883 `ModExp` schedule through the revm implementation
//! current at the time, which short-circuited zero-base/zero-modulus inputs to the 500-gas
//! minimum before computing the formula cost. EIP-7883 as written has no such special case:
//! its multiplication complexity floors at 16, so a zero-base/zero-modulus call with a
//! 64-byte exponent length prices in the thousands. Later revm versions charge that formula
//! cost, as the EIP specifies.
//!
//! Every spec keeps the historical short-circuit, so replaying a block reproduces the gas it
//! was executed with. Charging the formula cost instead is a consensus change that has not
//! been made, and these tests pin that: they fail the moment any spec starts pricing these
//! inputs by the formula, so the change cannot land unnoticed.
//!
//! EQUIVALENCE uses the inherited Berlin schedule, whose formula collapses to the flat
//! minimum for these inputs under either implementation ordering, so it is structurally
//! unaffected either way.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::OpTx,
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CALL, SSTORE},
    context::{result::ResultAndState, tx::TxEnvBuilder, TxEnv},
};

const CALLER: Address = address!("0000000000000000000000000000000000700000");
const CONTRACT: Address = address!("0000000000000000000000000000000000700001");
const MODEXP: Address = mega_evm::modexp::ADDRESS;

/// The flat charge of the zero-base/zero-modulus short-circuit.
const LEGACY_MIN_GAS: u64 = mega_evm::modexp::MIN_GAS;

/// Every spec that installs the Osaka `ModExp` schedule — all of them except EQUIVALENCE,
/// which stays on the inherited Berlin schedule.
const OSAKA_SCHEDULE_SPECS: [MegaSpecId; 9] = [
    MegaSpecId::MINI_REX,
    MegaSpecId::REX,
    MegaSpecId::REX1,
    MegaSpecId::REX2,
    MegaSpecId::REX3,
    MegaSpecId::REX4,
    MegaSpecId::REX5,
    MegaSpecId::REX6,
    MegaSpecId::REX7,
];

/// Wrapper contract: writes a 96-byte `ModExp` header (`base_len=0`, `exp_len`, `mod_len=0`,
/// no body) into memory, CALLs the precompile with `forwarded_gas`, stores the CALL result
/// at storage slot 0, and STOPs — so the outer transaction always succeeds and the
/// precompile outcome is observable in both gas and state.
fn zero_base_mod_wrapper(exp_len: u64, forwarded_gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, U256::ZERO.to_be_bytes::<32>()) // base_len = 0
        .mstore(32, U256::from(exp_len).to_be_bytes::<32>()) // exp_len
        .mstore(64, U256::ZERO.to_be_bytes::<32>()) // mod_len = 0
        .push_number(0_u64) // retSize
        .push_number(96_u64) // retOffset
        .push_number(96_u64) // argsSize (header only, no body)
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(MODEXP)
        .push_number(forwarded_gas) // gas
        .append(CALL)
        .push_number(0_u64) // slot 0
        .append(SSTORE) // store the CALL result
        .stop()
        .build()
}

/// Runs one transaction calling the wrapper under `spec` and returns the result.
fn transact(spec: MegaSpecId, code: Bytes) -> ResultAndState<MegaHaltReason> {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);
    let mut context = MegaContext::new(&mut db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    // 30M: MINI_REX charges a large first-write SSTORE storage-gas surcharge on the
    // wrapper's result slot, which a 1M budget does not cover.
    let tx: TxEnv =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(30_000_000).build_fill();
    let mut tx = OpTx(OpTransaction::new(tx));
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not error")
}

/// The CALL result the wrapper stored at slot 0 (1 = precompile succeeded, 0 = halted).
fn stored_call_result(result: &ResultAndState<MegaHaltReason>) -> U256 {
    result
        .state
        .get(&CONTRACT)
        .and_then(|account| account.storage.get(&U256::ZERO))
        .map(|slot| slot.present_value)
        .unwrap_or_default()
}

/// Every Osaka-schedule spec charges the flat 500-gas short-circuit for zero-base/zero-modulus
/// inputs: the total gas of a generously funded call equals that of one funded with exactly
/// `MIN_GAS + 1`, which is only possible if the charge is flat (a formula charge would halt
/// the underfunded variant, burning the full forwarded amount).
#[test]
fn test_every_spec_charges_flat_min_gas_for_zero_base_mod() {
    for spec in OSAKA_SCHEDULE_SPECS {
        let ample = transact(spec, zero_base_mod_wrapper(64, 10_000));
        let tight = transact(spec, zero_base_mod_wrapper(64, LEGACY_MIN_GAS + 1));

        assert!(ample.result.is_success(), "{spec:?}: outer tx should succeed: {:?}", ample.result);
        assert_eq!(
            stored_call_result(&ample),
            U256::from(1),
            "{spec:?}: a generously funded zero-base/zero-mod ModExp call must succeed"
        );
        assert_eq!(
            stored_call_result(&tight),
            U256::from(1),
            "{spec:?}: a call funded just above MIN_GAS must succeed under the flat schedule"
        );
        assert_eq!(
            ample.result.tx_gas_used(),
            tight.result.tx_gas_used(),
            "{spec:?}: the charge must be flat — a formula-based charge would OOG the tight \
             variant and the totals would diverge"
        );
    }
}

/// Rex7 prices these inputs exactly as Rex6 does. Rex7 is the unstable spec, so it is where an
/// EIP-7883 adoption would land first; this pins that it has not happened.
#[test]
fn test_rex7_prices_zero_base_mod_identically_to_rex6() {
    let rex6 = transact(MegaSpecId::REX6, zero_base_mod_wrapper(64, 50_000));
    let rex7 = transact(MegaSpecId::REX7, zero_base_mod_wrapper(64, 50_000));

    assert_eq!(stored_call_result(&rex6), U256::from(1), "REX6 call must succeed");
    assert_eq!(stored_call_result(&rex7), U256::from(1), "REX7 call must succeed");
    assert_eq!(
        rex7.result.tx_gas_used(),
        rex6.result.tx_gas_used(),
        "REX7 must charge what REX6 charges — adopting the EIP-7883 formula cost is a \
         deliberate spec change, not a side effect"
    );
}

/// Records what adopting the EIP-7883 formula would cost, so the size of the deviation stays
/// visible: the formula prices a 64-byte exponent length well above the flat minimum the
/// specs charge.
#[test]
fn test_eip7883_formula_cost_exceeds_the_charged_flat_minimum() {
    let formula_cost = revm::precompile::modexp::osaka_gas_calc(0, 64, 0, &U256::ZERO);
    assert!(
        formula_cost > LEGACY_MIN_GAS,
        "the EIP-7883 formula cost ({formula_cost}) must exceed the flat charge \
         ({LEGACY_MIN_GAS}) — otherwise the short-circuit would be unobservable"
    );
}

/// The sharpest input shape: a call forwarding more than the flat minimum but less than the
/// EIP-7883 formula cost. Under the short-circuit it succeeds on every spec; under the
/// formula it would halt and burn the entire forwarded amount.
#[test]
fn test_underfunded_zero_base_mod_call_succeeds_on_every_spec() {
    let forwarded = 1_000;
    assert!(
        forwarded > LEGACY_MIN_GAS &&
            forwarded < revm::precompile::modexp::osaka_gas_calc(0, 64, 0, &U256::ZERO),
        "the forwarded amount must sit inside the flat-vs-formula window to be a real probe"
    );

    for spec in OSAKA_SCHEDULE_SPECS {
        let result = transact(spec, zero_base_mod_wrapper(64, forwarded));
        assert_eq!(
            stored_call_result(&result),
            U256::from(1),
            "{spec:?}: {forwarded} forwarded gas covers the flat charge, so the call must \
             succeed"
        );
    }
}

/// EQUIVALENCE (inherited Berlin schedule) is structurally unaffected: the Berlin formula
/// collapses to its flat 200-gas minimum for zero-base/zero-modulus inputs under either
/// implementation ordering.
#[test]
fn test_equivalence_zero_base_mod_keeps_flat_berlin_charge() {
    const BERLIN_MIN_GAS: u64 = 200;
    let ample = transact(MegaSpecId::EQUIVALENCE, zero_base_mod_wrapper(64, 10_000));
    let tight = transact(MegaSpecId::EQUIVALENCE, zero_base_mod_wrapper(64, BERLIN_MIN_GAS + 1));

    assert_eq!(stored_call_result(&ample), U256::from(1), "generously funded call must succeed");
    assert_eq!(stored_call_result(&tight), U256::from(1), "tightly funded call must succeed");
    assert_eq!(
        ample.result.tx_gas_used(),
        tight.result.tx_gas_used(),
        "the Berlin charge for zero-base/zero-mod inputs must be flat"
    );
}

/// The EIP-7823 input-size limit is checked before the short-circuit, matching the historical
/// implementation's order: an oversized exponent length fails even when base and modulus
/// lengths are zero.
#[test]
fn test_eip7823_limit_precedes_short_circuit() {
    let result = transact(MegaSpecId::MINI_REX, zero_base_mod_wrapper(2_000, 10_000));
    assert!(result.result.is_success(), "outer tx should succeed");
    assert_eq!(
        stored_call_result(&result),
        U256::from(0),
        "an exponent length above the EIP-7823 limit must fail the precompile even with \
         zero base and modulus lengths"
    );
}
