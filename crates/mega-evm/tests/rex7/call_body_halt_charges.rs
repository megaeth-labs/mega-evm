//! REX7: what a CALL-family body already charged when it halts stays in the open segment.
//!
//! revm's CALL body charges before it can fail. It expands memory for the argument and return
//! ranges, then takes the value-transfer fee, and only afterwards loads the target account and
//! charges the gas it forwards — either of which can run out of gas. A body that halts there has
//! really spent the earlier charges, and it never reaches the recording window that would have
//! settled them.
//!
//! The CALL-family wrapper is the one that carries such a failure to its tail rather than
//! returning at the inner call, because the detention cap has to be applied on every path out. Its
//! tail must not re-open the settlement window on that path: the frame-exit settlement is what
//! records the charges, and re-opening the window at the current counter would drop them from
//! every lane at once — neither enforced as work nor booked as destroyed, so the transaction's
//! reported total would no longer cover the envelope it burnt.
//!
//! Each test is a differential: the same halt, at the same instruction, with the same gas left
//! over, reached once with the charge under test and once without it and with the child budget
//! reduced by exactly that charge. The difference between the two transactions' enforced compute
//! gas is what the charge contributed, and it must be the charge itself.

use crate::common::{transact_default, Outcome, CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaSpecId,
};
use revm::bytecode::opcode::{CALL, CALLCODE, DELEGATECALL, POP, STATICCALL, STOP};

/// The account the inner frame's CALL targets. Never touched before that CALL, so the cold-access
/// surcharge is what the inner frame runs out of gas on.
const TARGET: Address = address!("0000000000000000000000000000000000350001");

/// Gas one `PUSH` costs, whatever its width.
const PUSH_GAS: u64 = 3;
/// The CALL-family static entry the interpreter charges before the handler is entered.
const CALL_STATIC_GAS: u64 = 100;
/// The EVM's value-transfer surcharge, charged first inside the body.
const VALUE_TRANSFER_GAS: u64 = 9_000;
/// Bytes of return range the memory-expansion shapes ask for: two words.
const RETURN_RANGE_BYTES: u64 = 64;
/// Memory gas two words cost from an untouched memory: `3 * 2 + 2 * 2 / 512`.
const RETURN_RANGE_MEMORY_GAS: u64 = 6;
/// Gas the inner frame still holds when it reaches the charge it cannot afford. Any value below
/// the cold-account surcharge puts the halt on that charge.
const SLACK_GAS: u64 = 100;

/// Appends a CALL-family opcode targeting `target` with `gas` forwarded.
///
/// `value` is `None` for the schemes that take no value operand. `ret_size` is the return range
/// the opcode asks for, which is what makes the body expand memory before it charges anything
/// else.
fn append_call(
    builder: BytecodeBuilder,
    opcode: u8,
    target: Address,
    gas: u64,
    value: Option<u64>,
    ret_size: u64,
) -> BytecodeBuilder {
    let builder =
        builder.push_number(ret_size).push_number(0_u64).push_number(0_u64).push_number(0_u64);
    let builder = match value {
        Some(value) => builder.push_number(value),
        None => builder,
    };
    builder.push_address(target).push_number(gas).append(opcode)
}

/// How many stack operands a CALL-family opcode takes, which is what its pushes cost.
fn operand_count(opcode: u8) -> u64 {
    match opcode {
        CALL | CALLCODE => 7,
        _ => 6,
    }
}

/// The inner frame: one CALL-family opcode into [`TARGET`] that cannot afford to finish.
fn inner_code(opcode: u8, value: Option<u64>, ret_size: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), opcode, TARGET, 0, value, ret_size)
        .append(POP)
        .append(STOP)
        .build()
}

/// The transaction's target: a plain CALL into [`CALLEE`] forwarding exactly `budget`, whose
/// result is discarded so the outer frame ends normally whatever the inner frame did.
fn outer_code(budget: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), CALL, CALLEE, budget, Some(0), 0)
        .append(POP)
        .append(STOP)
        .build()
}

fn db(opcode: u8, value: Option<u64>, ret_size: u64, budget: u64) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, outer_code(budget))
        .account_code(CALLEE, inner_code(opcode, value, ret_size))
        .account_balance(CALLEE, U256::from(ONE_ETH))
        .account_code(TARGET, BytecodeBuilder::default().append(STOP).build())
}

/// Runs one arm of a differential and checks the properties both arms share: the inner frame
/// halted on gas, so its whole budget was spent as work and nothing was destroyed.
fn run_arm(opcode: u8, value: Option<u64>, ret_size: u64, budget: u64) -> Outcome {
    let outcome = transact_default(MegaSpecId::REX7, db(opcode, value, ret_size, budget));
    assert!(outcome.is_success(), "the outer frame absorbs the inner halt and stops normally");
    assert_eq!(
        outcome.destroyed, 0,
        "an out-of-gas frame's counter is zeroed by the interpreter, so it destroys nothing \
         the frame-exit delta cannot already see as work",
    );
    assert_eq!(outcome.booked_destroyed, 0, "and no site books a destroyed remainder for it");
    outcome
}

/// The budget an inner frame needs to reach the cold-account surcharge with [`SLACK_GAS`] left,
/// having paid `extra` inside the body first.
fn budget_for(opcode: u8, extra: u64) -> u64 {
    operand_count(opcode) * PUSH_GAS + CALL_STATIC_GAS + extra + SLACK_GAS
}

/// Asserts that `extra` gas charged inside a halting body reaches the enforced lane.
///
/// Both arms halt on the same charge with the same gas left over, and differ only by `extra`:
/// the charge itself, and the child budget that funds it.
fn assert_body_charge_is_enforced(
    opcode: u8,
    with: (Option<u64>, u64),
    without: (Option<u64>, u64),
    extra: u64,
) {
    let (with_value, with_ret_size) = with;
    let (without_value, without_ret_size) = without;
    let charged = run_arm(opcode, with_value, with_ret_size, budget_for(opcode, extra));
    let control = run_arm(opcode, without_value, without_ret_size, budget_for(opcode, 0));
    assert_eq!(
        charged.total_gas_spent - control.total_gas_spent,
        extra,
        "the two arms must differ by the charge alone",
    );
    assert_eq!(
        charged.enforced() - control.enforced(),
        extra,
        "a charge the body took before halting is work the transaction performed, so it must \
         reach the lane every compute limit is evaluated against",
    );
}

/// `CALLCODE` with a value transfer: the 9,000 surcharge is charged, and then the cold-account
/// load the frame cannot afford halts it.
#[test]
fn test_rex7_value_callcode_halt_enforces_the_transfer_fee() {
    assert_body_charge_is_enforced(CALLCODE, (Some(1), 0), (Some(0), 0), VALUE_TRANSFER_GAS);
}

/// The same shape through `CALL`, which resolves a different target account and so reaches the
/// surcharge by a different route.
#[test]
fn test_rex7_value_call_halt_enforces_the_transfer_fee() {
    assert_body_charge_is_enforced(CALL, (Some(1), 0), (Some(0), 0), VALUE_TRANSFER_GAS);
}

/// `STATICCALL` asking for a return range: the memory expansion is charged ahead of everything
/// else in the body, including the load that halts the frame.
#[test]
fn test_rex7_staticcall_halt_enforces_the_return_range_memory() {
    assert_body_charge_is_enforced(
        STATICCALL,
        (None, RETURN_RANGE_BYTES),
        (None, 0),
        RETURN_RANGE_MEMORY_GAS,
    );
}

/// `DELEGATECALL` covers the fourth instantiation of the shared wrapper.
#[test]
fn test_rex7_delegatecall_halt_enforces_the_return_range_memory() {
    assert_body_charge_is_enforced(
        DELEGATECALL,
        (None, RETURN_RANGE_BYTES),
        (None, 0),
        RETURN_RANGE_MEMORY_GAS,
    );
}

/// The receipt does not move. Checkpoint accounting changes how a halting frame's budget is
/// reported — REX7 settles it as compute gas, REX6 never records it at all — but not what the EVM
/// charged, so the gas the transaction burns is the same under both.
#[test]
fn test_rex6_and_rex7_burn_the_same_gas_on_a_halting_call_body() {
    for (opcode, value, ret_size, extra) in [
        (CALLCODE, Some(1), 0, VALUE_TRANSFER_GAS),
        (CALL, Some(1), 0, VALUE_TRANSFER_GAS),
        (STATICCALL, None, RETURN_RANGE_BYTES, RETURN_RANGE_MEMORY_GAS),
        (DELEGATECALL, None, RETURN_RANGE_BYTES, RETURN_RANGE_MEMORY_GAS),
    ] {
        let budget = budget_for(opcode, extra);
        let rex6 = transact_default(MegaSpecId::REX6, db(opcode, value, ret_size, budget));
        let rex7 = transact_default(MegaSpecId::REX7, db(opcode, value, ret_size, budget));
        assert_eq!(
            rex6.gas_used, rex7.gas_used,
            "opcode 0x{opcode:02x}: the receipt must not depend on how compute gas is settled",
        );
        assert_eq!(
            rex6.total_gas_spent, rex7.total_gas_spent,
            "opcode 0x{opcode:02x}: nor may the envelope the receipt is built from",
        );
    }
}
