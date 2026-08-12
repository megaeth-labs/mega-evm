//! REX7 splits an exceptionally halted frame into the work it performed and the budget it
//! destroyed.
//!
//! The two halves are accounted differently, and both halves have to be right:
//!
//! - **Executed** — everything the frame ran before it failed. It settles through the ordinary
//!   enforcing path, so it shrinks the parent frame's budget, the transaction's compute budget, the
//!   reading `MegaLimitControl.remainingComputeGas` returns, and the base a detention cap is built
//!   on. A parent frame keeps executing after it absorbs a failed child; if the child's work left
//!   enforcement, the code that follows could spend the same headroom a second time.
//! - **Destroyed** — the budget the frame never gets to spend, and never hands back. It is reported
//!   and block-accounted but never enforced: halting on it would turn an ordinary EVM halt into a
//!   resource-limit failure with the gas rescued, which is the receipt change the exceptional-halt
//!   carve-out forbids.
//!
//! Two boundaries decide what belongs to which half, and both are exercised here:
//!
//! - the **storage gas** a checkpoint body charged before aborting is neither — it is storage gas,
//!   and the body never reached the recording that would have subtracted it;
//! - the classification is only final **after action processing**, because revm's create-return can
//!   still turn a successful constructor into a code-deposit out-of-gas, an EIP-3541 reject or a
//!   runtime code-size reject.
//!
//! [`exceptional_halt`](crate::exceptional_halt) covers the reported totals; this module covers
//! which side of the enforcing boundary each part lands on.

use crate::common::{
    transact, transact_default, transact_tx, transact_with_bucket_capacity,
    transact_with_gas_limit, Outcome, CALLEE, CALLER, CONTRACT, DEFAULT_TX_GAS_LIMIT, ONE_ETH,
};
use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    alloy_op_evm::OpTxError,
    constants::mini_rex::{CODEDEPOSIT_STORAGE_GAS, LOG_DATA_STORAGE_GAS, MAX_CONTRACT_SIZE},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EVMError, EvmTxRuntimeLimits, IKeylessDeploy, IMegaLimitControl, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _, KEYLESS_DEPLOY_ADDRESS,
    LIMIT_CONTROL_ADDRESS,
};
use revm::{
    bytecode::opcode::{
        ADD, CALL, CREATE, LOG0, MLOAD, MSTORE8, POP, RETURN, SSTORE, STATICCALL, STOP, TIMESTAMP,
    },
    context::{result::ResultAndState, tx::TxEnvBuilder, CfgEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
};
use std::{convert::Infallible, vec::Vec};

/// Relayer that sends the keyless-deploy transactions.
const KEYLESS_RELAYER: Address = address!("0000000000000000000000000000000000340004");

/// Storage slot the caller writes its `remainingComputeGas` readings to.
const BEFORE_SLOT: u64 = 0xb0;
/// Second `remainingComputeGas` reading slot.
const AFTER_SLOT: u64 = 0xb1;

/// Plain-opcode pairs the failing child runs before it underflows. Chosen large enough that the
/// work it performs dominates every other term in these fixtures.
const CHILD_PAIRS: usize = 1_000;
/// Compute gas one `PUSH1 1; POP` pair costs: `PUSH1` is 3, `POP` is 2.
const PAIR_GAS: u64 = 5;

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// `pairs` PUSH1/POP pairs — plain opcodes that settle only at the next checkpoint.
fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// A callee that performs [`CHILD_PAIRS`] pairs of real work and then ends its frame with a stack
/// underflow — an exceptional halt that is not a gas shortage, so the interpreter keeps its
/// counter and nothing about the failure is a resource-limit exceed.
fn working_then_underflowing_callee() -> Bytes {
    plain_filler(BytecodeBuilder::default(), CHILD_PAIRS).append(ADD).append(STOP).build()
}

/// A CALL into [`CALLEE`] forwarding `gas`, with the success flag popped so the caller survives
/// whatever the callee did.
fn call_callee(builder: BytecodeBuilder, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// A STATICCALL into `MegaLimitControl.remainingComputeGas()` whose returned word is written to
/// `slot`. The selector is written to memory first; the reading comes back into offset 0 as well.
fn store_remaining_compute_gas(builder: BytecodeBuilder, slot: u64) -> BytecodeBuilder {
    builder
        .mstore(0, IMegaLimitControl::remainingComputeGasCall::SELECTOR)
        .push_number(32u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(4u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(1_000_000u64)
        .append(STATICCALL)
        .append(POP)
        .push_number(0u64)
        .append(MLOAD)
        .push_u256(U256::from(slot))
        .append(SSTORE)
}

/// The caller shape every blocker-A case shares: work, an exceptional child, then the same amount
/// of work again. Whether the transaction survives the second half is what the child's executed
/// work decides.
fn work_call_work(child_gas: u64, tail_pairs: usize) -> Bytes {
    let builder = plain_filler(BytecodeBuilder::default(), 10);
    let builder = call_callee(builder, child_gas);
    plain_filler(builder, tail_pairs).append(STOP).build()
}

fn caller_db(caller_code: Bytes) -> MemoryDatabase {
    base_db(caller_code).account_code(CALLEE, working_then_underflowing_callee())
}

/// The work an exceptionally halted child performed still binds the transaction's compute limit.
///
/// This is the shape a fail-open shows up in: the child runs [`CHILD_PAIRS`] pairs of plain
/// opcodes and then underflows, the caller absorbs the failure and runs the same amount of work
/// again. Per-opcode accounting charges the child's work as it happens, so REX6's total is the
/// calibration point — set the limit one below it and the transaction must not finish. REX7 has to
/// stop as well: the child's executed work is real work, whatever the frame did afterwards.
#[test]
fn test_executed_work_of_an_exceptional_child_still_binds_the_tx_limit() {
    let code = work_call_work(1_000_000, CHILD_PAIRS);

    // Calibrate against REX6 running the same program with nothing in its way.
    let unconstrained = transact_default(MegaSpecId::REX6, caller_db(code.clone()));
    assert!(
        unconstrained.is_success(),
        "the calibration run must succeed: {:?}",
        unconstrained.result
    );
    let limit = unconstrained.compute_gas - 1;
    assert!(
        limit > u64::try_from(CHILD_PAIRS).unwrap() * PAIR_GAS,
        "the fixture must do more work than the child alone, or the limit proves nothing",
    );

    let limits = |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit);
    let r6 = transact(MegaSpecId::REX6, caller_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, caller_db(code), limits(MegaSpecId::REX7));

    assert!(!r6.is_success(), "REX6 must stop at the limit: {:?}", r6.result);
    assert!(
        !r7.is_success(),
        "REX7 must stop at the same limit — the child's executed work is enforced even though its \
         frame ended in an exceptional halt; got {:?} with compute={}",
        r7.result,
        r7.compute_gas,
    );
}

/// The same shape from the caller's own point of view: `MegaLimitControl.remainingComputeGas()`
/// reports the minimum of the caller's per-frame budget and the transaction-level remaining, so
/// one reading pins both. The drop across the failing child must cover the work the child did.
#[test]
fn test_exceptional_child_shrinks_the_callers_remaining_compute_budget() {
    let builder = store_remaining_compute_gas(BytecodeBuilder::default(), BEFORE_SLOT);
    let builder = call_callee(builder, 1_000_000);
    let code = store_remaining_compute_gas(builder, AFTER_SLOT).append(STOP).build();

    let child_work = u64::try_from(CHILD_PAIRS).unwrap() * PAIR_GAS;
    let mut drops = Vec::new();
    for spec in [MegaSpecId::REX6, MegaSpecId::REX7] {
        let outcome = transact_default(spec, caller_db(code.clone()));
        assert!(
            outcome.is_success(),
            "{spec:?}: the caller must survive its child: {:?}",
            outcome.result,
        );
        let before: u64 = outcome
            .storage_value(CONTRACT, U256::from(BEFORE_SLOT))
            .try_into()
            .expect("a compute-gas reading fits in u64");
        let after: u64 = outcome
            .storage_value(CONTRACT, U256::from(AFTER_SLOT))
            .try_into()
            .expect("a compute-gas reading fits in u64");
        assert!(before > after, "{spec:?}: the reading must fall across the child");
        let drop = before - after;
        assert!(
            drop >= child_work,
            "{spec:?}: the caller's remaining budget must fall by at least the child's executed \
             work; drop={drop} child work={child_work}",
        );
        drops.push(drop);
    }
    // Both models see the same child work; the only slack is which opcodes each attributes to the
    // failing frame, so the two readings must agree to within one opcode's static gas.
    let (r6, r7) = (drops[0], drops[1]);
    assert!(
        r7.abs_diff(r6) <= 32,
        "the two models must charge the caller the same for a failed child; REX6={r6} REX7={r7}",
    );
}

/// A detention cap is built relative to the usage already enforced at the access point
/// (`usage + cap`), so a fail-open on an exceptional child does not just widen the compute limit —
/// it widens every cap installed afterwards. Reading the post-transaction detained limit back is a
/// direct check on the base the cap was built from.
#[test]
fn test_detention_cap_after_an_exceptional_child_counts_its_executed_work() {
    let builder = plain_filler(BytecodeBuilder::default(), 10);
    let builder = call_callee(builder, 1_000_000);
    let code = builder.append(TIMESTAMP).append(POP).append(STOP).build();

    let limits = |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = 1_000_000;
        limits
    };
    let r6 = transact(MegaSpecId::REX6, caller_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, caller_db(code), limits(MegaSpecId::REX7));

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);
    assert!(
        r7.detained_compute_gas_limit.abs_diff(r6.detained_compute_gas_limit) <= 32,
        "the cap must be built on the same enforced usage under both models; REX6={} REX7={}",
        r6.detained_compute_gas_limit,
        r7.detained_compute_gas_limit,
    );
}

/// The transaction-wide identity the storage-exclusion cases assert: every EVM gas a transaction
/// spends is either compute gas or `MegaETH` storage gas, so
/// `compute_gas == gas_used − storage gas`. Measuring the transaction-intrinsic part from a bare
/// `STOP` keeps it exact rather than pinned to a constant.
fn intrinsic_storage_gas(spec: MegaSpecId) -> u64 {
    let outcome = transact_with_gas_limit(
        spec,
        base_db(BytecodeBuilder::default().append(STOP).build()),
        EvmTxRuntimeLimits::from_spec(spec),
        1_000_000,
    );
    outcome.gas_used - outcome.compute_gas
}

/// A checkpoint body charges its storage gas before running the raw opcode, and subtracts it back
/// out when it records its own compute window. A body that halts in between never reaches that
/// subtraction — so the charge has to leave the open segment as it is made, or the frame-exit
/// settlement reports storage gas as compute gas.
///
/// `LOG0` in a static frame is the shape that isolates it: the storage surcharge is a flat
/// per-byte rate that is already paid when revm rejects the state change.
#[test]
fn test_aborted_log_checkpoint_does_not_report_its_storage_charge_as_compute() {
    const LOG_BYTES: u64 = 32;
    let callee = BytecodeBuilder::default()
        .push_number(LOG_BYTES) // len
        .push_number(0u64) // offset
        .append(LOG0)
        .append(STOP)
        .build();
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(CALLEE)
        .push_number(77_777u64)
        .append(STATICCALL)
        .append(POP)
        .append(STOP)
        .build();

    let db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let r7 = transact_default(MegaSpecId::REX7, db());
    assert!(
        r7.is_success(),
        "the caller must survive the static-context rejection: {:?}",
        r7.result,
    );

    let log_storage_gas = LOG_DATA_STORAGE_GAS * LOG_BYTES;
    assert_eq!(
        r7.compute_gas,
        r7.gas_used - intrinsic_storage_gas(MegaSpecId::REX7) - log_storage_gas,
        "the LOG storage surcharge is storage gas on the halting path too; compute={} gas_used={}",
        r7.compute_gas,
        r7.gas_used,
    );
}

/// The same exclusion for the other storage-charging checkpoint family that can abort after
/// charging: `SSTORE`. Its surcharge is SALT-scaled, so it is only non-zero above the minimum
/// bucket size — the elevated capacity is what makes this case exist at all.
///
/// The surcharge is measured from a control run that performs the same write outside a static
/// frame, so the assertion states the amount rather than assuming a constant: the aborted body
/// pays exactly that much storage gas, and none of it may reach the compute total.
#[test]
fn test_aborted_sstore_checkpoint_does_not_report_its_storage_charge_as_compute() {
    /// Twice the minimum bucket size, so the SALT multiplier makes the `SSTORE` set charge
    /// non-zero.
    const BUCKET_CAPACITY: u64 = 2 * mega_evm::MIN_BUCKET_SIZE as u64;

    let callee = BytecodeBuilder::default().sstore(U256::from(9), U256::from(0x77)).build();
    let caller = |call_opcode: u8| {
        let mut builder = BytecodeBuilder::default()
            .push_number(0u64) // retSize
            .push_number(0u64) // retOffset
            .push_number(0u64) // argsSize
            .push_number(0u64); // argsOffset
        if call_opcode == CALL {
            builder = builder.push_number(0u64); // value
        }
        builder
            .push_address(CALLEE)
            .push_number(10_000_000u64)
            .append(call_opcode)
            .append(POP)
            .append(STOP)
            .build()
    };

    let run = |call_opcode| {
        transact_with_bucket_capacity(
            MegaSpecId::REX7,
            base_db(caller(call_opcode)).account_code(CALLEE, callee.clone()),
            EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
            BUCKET_CAPACITY,
        )
    };
    let intrinsic_storage = intrinsic_storage_gas(MegaSpecId::REX7);

    // Control: the same write in a frame that is allowed to make it. Its non-intrinsic storage gas
    // is the surcharge the aborted run below also pays, before revm rejects the state change.
    let committed = run(CALL);
    assert!(committed.is_success(), "the control write must succeed: {:?}", committed.result);
    let surcharge = committed.gas_used - committed.compute_gas - intrinsic_storage;
    assert!(
        surcharge > 0,
        "the elevated bucket capacity must make the SSTORE set charge non-zero; gas_used={} \
         compute={}",
        committed.gas_used,
        committed.compute_gas,
    );

    let aborted = run(STATICCALL);
    assert!(
        aborted.is_success(),
        "the caller must survive the static-context rejection: {:?}",
        aborted.result,
    );
    assert_eq!(
        aborted.compute_gas,
        aborted.gas_used - intrinsic_storage - surcharge,
        "the SSTORE surcharge stays storage gas when the body it paid for never runs; compute={} \
         gas_used={} surcharge={surcharge}",
        aborted.compute_gas,
        aborted.gas_used,
    );
}

/// Init code returning `len` bytes of runtime code whose first byte is `first`.
fn deploying_initcode(first: u8, len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(u64::from(first))
        .push_number(0u64)
        .append(MSTORE8)
        .push_number(len)
        .push_number(0u64)
        .append(RETURN)
        .build()
}

/// A contract whose body CREATEs `initcode` with all the gas it has, then stops.
fn creator_code(initcode: &Bytes) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, initcode.as_ref())
        .push_number(initcode.len() as u64) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

/// Runs a REX7 transaction into [`CONTRACT`] with an explicit gas limit and, optionally, a lowered
/// `limit_contract_code_size`.
///
/// The shared helpers in [`crate::common`] all take the context's default configuration, which
/// pins the contract-size limit to `MegaETH`'s 512 KiB. Reaching revm's size reject needs a
/// smaller one, so this builds the context itself.
fn transact_create_reject(
    mut db: MemoryDatabase,
    gas_limit: u64,
    code_size_limit: Option<usize>,
) -> Outcome {
    let mut cfg = CfgEnv::default();
    cfg.spec = MegaSpecId::REX7;
    cfg.limit_contract_code_size = code_size_limit.or(Some(MAX_CONTRACT_SIZE));
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX7)
        .with_cfg(cfg)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(gas_limit).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let mut evm = MegaEvm::new(context);
    let result =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    let (usage, detained_compute_gas_limit) = {
        let additional_limit = EvmTr::ctx_ref(&evm).additional_limit.borrow();
        (additional_limit.get_usage(), additional_limit.detained_compute_gas_limit())
    };
    let gas_used = result.result.tx_gas_used();
    Outcome {
        result: result.result,
        compute_gas: usage.compute_gas,
        data_size: usage.data_size,
        kv_updates: usage.kv_updates,
        state_growth: usage.state_growth,
        gas_used,
        detained_compute_gas_limit,
        state: result.state,
    }
}

/// Runtime length the CREATE cases deploy — small enough that the per-byte code-deposit storage
/// charge stays affordable at the gas limits below.
const RUNTIME_LEN: u64 = 100;

/// revm's per-byte code-deposit gas (`revm::interpreter::gas::CODEDEPOSIT`).
const CANONICAL_CODE_DEPOSIT_GAS: u64 = 200;

/// The transaction-wide gas identity for the CREATE fixtures: the receipt's EVM gas is compute gas
/// plus `MegaETH` storage gas, and the only storage gas beyond the transaction intrinsic is the
/// per-byte code-deposit charge the execution layer takes before revm's create-return runs.
///
/// It holds whether or not the deposit is ultimately rejected — which is the point. A reject
/// destroys the CREATE frame's whole remainder, and that destroyed budget is EVM gas the receipt
/// charges, so it has to appear in the compute total like any other exceptionally halted frame's.
fn assert_create_gas_identity(label: &str, outcome: &Outcome, intrinsic_storage: u64) {
    let code_deposit_storage = CODEDEPOSIT_STORAGE_GAS * RUNTIME_LEN;
    assert!(
        outcome.is_success(),
        "{label}: the creator must survive the CREATE: {:?}",
        outcome.result,
    );
    assert!(
        outcome.gas_used > intrinsic_storage + code_deposit_storage,
        "{label}: the fixture must reach the create-return, not run out paying the code-deposit \
         storage charge; gas_used={}",
        outcome.gas_used,
    );
    assert_eq!(
        outcome.compute_gas,
        outcome.gas_used - intrinsic_storage - code_deposit_storage,
        "{label}: every EVM gas the transaction spent must be compute gas or storage gas; \
         compute={} gas_used={} code-deposit storage={code_deposit_storage}",
        outcome.compute_gas,
        outcome.gas_used,
    );
}

/// revm's create-return rejects a successful constructor's runtime code **after** action
/// processing. EIP-3541 and the code-size limit are the two rejects that need no gas pressure at
/// all: the constructor returned normally, the frame's result was `Return` when the frame-exit
/// settlement ran, and only the create-return turned it into a halt that destroys the frame's
/// whole remainder.
///
/// The code-size case runs against a lowered `limit_contract_code_size`. `MegaETH`'s own per-byte
/// code-deposit storage charge is 10,000 gas, so a runtime code long enough to pass the 512 KiB
/// consensus limit would need billions of gas to reach the reject and would run out paying that
/// charge first — reaching revm's size check at all needs a configured limit, not a longer
/// contract.
#[test]
fn test_create_rejected_after_action_processing_settles_its_destroyed_remainder() {
    let intrinsic_storage = intrinsic_storage_gas(MegaSpecId::REX7);
    let deployed = transact_create_reject(
        base_db(creator_code(&deploying_initcode(0x00, RUNTIME_LEN))),
        DEFAULT_TX_GAS_LIMIT,
        None,
    );
    assert_create_gas_identity("successful deposit", &deployed, intrinsic_storage);

    for (label, first, code_size_limit) in [
        // Runtime code starting with 0xEF: EIP-3541 rejects the deposit.
        ("EIP-3541", 0xefu8, None),
        // Runtime code past a configured contract-size limit.
        ("code size", 0x00, Some(RUNTIME_LEN as usize - 1)),
    ] {
        let rejected = transact_create_reject(
            base_db(creator_code(&deploying_initcode(first, RUNTIME_LEN))),
            DEFAULT_TX_GAS_LIMIT,
            code_size_limit,
        );
        assert_create_gas_identity(label, &rejected, intrinsic_storage);
        assert!(
            rejected.gas_used > deployed.gas_used,
            "{label}: the reject must destroy the CREATE frame's remainder, so it costs strictly \
             more than the deposit it replaced; rejected={} deployed={}",
            rejected.gas_used,
            deployed.gas_used,
        );
    }
}

/// The third post-action reject is the canonical code-deposit charge itself running out of gas —
/// the one that exists only inside a narrow gas window: too little gas and the frame fails earlier,
/// paying `MegaETH`'s own per-byte code-deposit storage charge; too much and the deposit goes
/// through.
///
/// Sweeping across that window covers it without pinning the boundary. What every point has to
/// satisfy is that no EVM gas goes missing: the receipt's gas is compute gas plus storage gas, and
/// the only storage gas a run can carry beyond the transaction intrinsic is the per-byte
/// code-deposit charge — all of it, or none of it, depending on whether the frame could afford it.
/// A destroyed CREATE remainder that never reached the compute total would show up here as a third
/// value.
#[test]
fn test_create_code_deposit_out_of_gas_settles_its_destroyed_remainder() {
    let intrinsic_storage = intrinsic_storage_gas(MegaSpecId::REX7);
    let code_deposit_storage = CODEDEPOSIT_STORAGE_GAS * RUNTIME_LEN;
    let initcode = deploying_initcode(0x00, RUNTIME_LEN);
    let run = |gas_limit| transact_create_reject(base_db(creator_code(&initcode)), gas_limit, None);

    let deployed = run(DEFAULT_TX_GAS_LIMIT);
    assert_create_gas_identity("successful deposit", &deployed, intrinsic_storage);

    // A successful deposit costs a fixed amount, and the frame that pays it keeps back the 2% the
    // creator retained — so the window where the canonical charge alone is unaffordable sits just
    // above that fixed cost, sized by the charge itself.
    let canonical_deposit = CANONICAL_CODE_DEPOSIT_GAS * RUNTIME_LEN;
    let mut code_deposit_oog_points = 0;
    for step in 0..=canonical_deposit / 1_000 {
        let gas_limit = deployed.gas_used + step * 1_000;
        let outcome = run(gas_limit);
        assert!(
            outcome.is_success(),
            "gas_limit={gas_limit}: the creator must survive the CREATE: {:?}",
            outcome.result,
        );
        let storage = outcome
            .gas_used
            .checked_sub(outcome.compute_gas)
            .and_then(|total| total.checked_sub(intrinsic_storage))
            .unwrap_or_else(|| {
                panic!(
                    "gas_limit={gas_limit}: compute gas exceeds the receipt's non-intrinsic gas; \
                     compute={} gas_used={}",
                    outcome.compute_gas, outcome.gas_used,
                )
            });
        assert!(
            storage == 0 || storage == code_deposit_storage,
            "gas_limit={gas_limit}: the only storage gas past the intrinsic is the per-byte \
             code-deposit charge, taken in full or not at all — anything else is EVM gas missing \
             from the compute total; storage={storage} compute={} gas_used={}",
            outcome.compute_gas,
            outcome.gas_used,
        );
        // The charge was affordable but the deposit still did not happen: revm's create-return
        // rejected it, after action processing, for the canonical code-deposit gas.
        if storage == code_deposit_storage && outcome.gas_used != deployed.gas_used {
            code_deposit_oog_points += 1;
        }
    }
    assert!(
        code_deposit_oog_points > 0,
        "the sweep must contain at least one canonical code-deposit out-of-gas; a successful \
         deposit costs the same {} gas at every limit above the window",
        deployed.gas_used,
    );
}

/// Runs the blocker-A shape through `inspect_frame_run` instead of `frame_run`.
///
/// The two loops are hand-maintained copies of the same body, and the split is settled across both
/// of their hooks — the executed tail before action processing, the destroyed remainder after. A
/// drop of either on the inspected copy alone would silently re-open the fail-open for any node
/// running with a tracer attached.
fn transact_inspected(mut db: MemoryDatabase, limits: EvmTxRuntimeLimits, inspected: bool) -> u64 {
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX7).with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(DEFAULT_TX_GAS_LIMIT)
        .build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    // Both arms must produce the same `MegaEvm` type, so build the inspected one by toggling the
    // inspector flag rather than by changing the inspector type.
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    if !inspected {
        alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    }
    let result: Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> =
        alloy_evm::Evm::transact_raw(&mut evm, tx);
    result.expect("tx should not surface EVMError");
    let usage = EvmTr::ctx_ref(&evm).additional_limit.borrow().get_usage();
    usage.compute_gas
}

/// The inspected execution loop must split an exceptional frame exactly like the plain one.
#[test]
fn test_the_split_is_the_same_under_an_inspector() {
    let db = || caller_db(work_call_work(1_000_000, CHILD_PAIRS));
    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7);
    assert_eq!(
        transact_inspected(db(), limits, false),
        transact_inspected(db(), limits, true),
        "an attached inspector must not move the executed / destroyed split",
    );
}

/// Builds a deterministic pre-EIP-155 keyless deployment transaction.
fn keyless_tx_bytes(init_code: Bytes) -> Bytes {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: 100_000_000_000,
        gas_limit: 200_000,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };
    let word = U256::from_be_bytes(hex!(
        "3333333333333333333333333333333333333333333333333333333333333333"
    ));
    let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Bytes::from(buf)
}

/// The `KeylessDeploy` sandbox runs a whole nested transaction with its own tracker and merges the
/// usage back, so the executed / destroyed split has to survive that boundary. A sandbox whose
/// constructor halts exceptionally reports its destroyed remainder like any other frame; if the
/// merge dropped the classification, the parent would enforce it — and a constructor's ordinary EVM
/// halt would rewrite the outer transaction into a compute-limit exceed with the gas rescued.
///
/// The parent's compute limit is set well below the sandbox's gas override so the destroyed
/// remainder alone would be enough to trip it.
#[test]
fn test_sandbox_destroyed_remainder_stays_non_enforcing_across_the_merge() {
    // `ADD` on an empty stack: the constructor halts immediately, so almost the whole sandbox
    // envelope is destroyed rather than performed.
    let init_code = BytecodeBuilder::default().append(ADD).append(STOP).build();
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes(init_code),
        gasLimitOverride: U256::from(1_000_000u64),
    }
    .abi_encode();

    let build_tx = || {
        TxEnvBuilder::default()
            .caller(KEYLESS_RELAYER)
            .call(KEYLESS_DEPLOY_ADDRESS)
            .gas_limit(30_000_000)
            .chain_id(Some(1))
            .data(Bytes::from(call_data.clone()))
            .build_fill()
    };
    let limits = |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(300_000);
    let run = |spec| {
        let db =
            MemoryDatabase::default().account_balance(KEYLESS_RELAYER, U256::from(10 * ONE_ETH));
        transact_tx(spec, db, limits(spec), build_tx(), &crate::common::default_envs())
    };

    let r6 = run(MegaSpecId::REX6);
    let r7 = run(MegaSpecId::REX7);

    assert!(
        r6.is_success(),
        "REX6 returns the constructor failure through the keyless-deploy wire contract: {:?}",
        r6.result,
    );
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the outer transaction must keep the wire contract REX6 defines — the sandbox's destroyed \
         budget is reported, never enforced, on either side of the merge",
    );
    assert!(
        r7.compute_gas > 300_000,
        "the sandbox's destroyed remainder must still be reported past the limit; compute={}",
        r7.compute_gas,
    );
}
