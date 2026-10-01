//! The properties a single transaction must hold, one test each.
//!
//! Every property draws its cases from the same generators, runs the case on a fresh EVM and
//! states what the spec says of the outcome. A property that does not hold by design is stated
//! as what does hold, and says so.

use std::{collections::BTreeMap, sync::Mutex};

use alloy_primitives::{Address, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::is_system_originated,
    test_utils::{GasInspector, MemoryDatabase},
    transaction_body_bytes, LimitCheck, MegaContext, MegaTransaction, MegaTransactionOutcome,
    WRITE_RECORD_SIZE,
};
use revm::{
    context::{result::ExecutionResult, Transaction},
    interpreter::{interpreter::EthInterpreter, Interpreter},
    state::Bytecode,
    Database, Inspector,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use crate::{
    gen::{
        case::{case, Case, Envs},
        tx::{tx, Delegate, Shape},
        Flavor, Who, SYSTEM_ADDRESS,
    },
    harness::{check, print_tally, prop_check, prop_eq},
    render::{render, render_outcome},
};

/// The number of cases each property runs in the bounded mode.
const CASES: u32 = 768;

/// Every case executes without a panic: the engine answers with an outcome or refuses the
/// transaction with an error, whatever the input. In a debug build every `debug_assert` on the
/// path runs. The tally of outcome classes is printed, so a run shows what the generators reach.
#[test]
fn test_property_no_panic() {
    let tally: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());
    check(
        "no_panic",
        CASES,
        || case(Flavor::Satin),
        |case| {
            let class = match case.execute() {
                Ok(outcome) => {
                    let stop = outcome
                        .limit_exceeded
                        .map(|check| match check {
                            LimitCheck::ExceedsLimit { kind, .. } => {
                                format!(" stopped on {kind:?}")
                            }
                            LimitCheck::WithinLimit => String::new(),
                        })
                        .unwrap_or_default();
                    match &outcome.result {
                        ExecutionResult::Success { .. } => "success".to_string(),
                        ExecutionResult::Revert { .. } => format!("revert{stop}"),
                        ExecutionResult::Halt { reason, .. } => format!("halt {reason:?}"),
                    }
                }
                // The error's own name, without its fields: `Transaction(Base(NonceTooHigh`.
                Err(error) => {
                    format!("refused {}", error.split([' ', '{', ')']).next().unwrap_or(""))
                }
            };
            *tally.lock().unwrap().entry(class).or_default() += 1;
            Ok(())
        },
    );
    print_tally("no_panic", tally.into_inner().unwrap());
}

/// The same input twice gives byte-identical outcomes: the result, its gas, logs and output, the
/// gas by ledger, the usage, the stop and the state, or the same refusal.
#[test]
fn test_property_determinism() {
    check(
        "determinism",
        CASES,
        || case(Flavor::Satin),
        |case| {
            let first = render(&case.execute());
            let second = render(&case.execute());
            prop_eq!(first, second, "two runs of the same case differ");
            Ok(())
        },
    );
}

/// A transaction's outcome does not depend on what its EVM ran before: on an EVM that already ran
/// another transaction over the same pre-state it is byte-identical to its outcome on a fresh
/// one, whether that transaction succeeded, reverted, halted, was stopped by a limit or was
/// refused with an error, and running it once more gives the same again.
///
/// Nothing is committed between the runs, so the state is the same each time; what could differ
/// is what the engine keeps for one transaction and must reset for the next: the limits layer and
/// its latch, gas detention, the SALT multipliers it read, the keyless frame. A transaction that
/// ends in an error leaves through no settlement, so the reset is all that stands between it and
/// the next. The tally of how the previous transaction ended is printed.
#[test]
fn test_property_a_reused_evm_runs_a_transaction_as_a_fresh_one_does() {
    let tally: Mutex<BTreeMap<&'static str, u32>> = Mutex::new(BTreeMap::new());
    check(
        "reused_evm",
        CASES,
        || (case(Flavor::Satin), tx(Flavor::Satin)),
        |(case, previous)| {
            let fresh = render(&case.execute());
            let mut evm = case.evm();
            let previous = previous.build(&case.world, &case.main.assemble());
            let ended = match Case::execute_on(&mut evm, previous) {
                // A SALT bucket that cannot be read fails the transaction where the charge is
                // made, in the middle of its run; every other error refuses it before it runs.
                Err(error) if error.contains("the bucket cannot be read") => {
                    "the previous transaction ended in an error while it ran"
                }
                Err(_) => "the previous transaction was refused with an error",
                Ok(outcome) if outcome.limit_exceeded.is_some() => {
                    "the previous transaction was stopped by a limit"
                }
                Ok(outcome) if outcome.result.is_success() => "the previous transaction succeeded",
                Ok(_) => "the previous transaction reverted or halted",
            };
            *tally.lock().unwrap().entry(ended).or_default() += 1;
            let reused = render(&Case::execute_on(&mut evm, case.transaction()));
            prop_eq!(fresh, reused, "the outcome on a reused EVM differs: {ended}");
            let again = render(&Case::execute_on(&mut evm, case.transaction()));
            prop_eq!(fresh, again, "the outcome of a second run on the same EVM differs");
            Ok(())
        },
    );
    print_tally("reused_evm", tally.into_inner().unwrap());
}

/// An inspector that only records changes nothing: without one, under the gas inspector of the
/// test utilities and under revm-inspectors' tracer, the outcome is byte-identical.
#[test]
fn test_property_inspector_transparency() {
    check(
        "inspector_transparency",
        CASES,
        || case(Flavor::Satin),
        |case| {
            let plain = render(&case.execute());
            let (recorded, _) = case.execute_inspected(GasInspector::new());
            prop_eq!(plain, render(&recorded), "the gas inspector changed the outcome");
            let (traced, _) =
                case.execute_inspected(TracingInspector::new(TracingInspectorConfig::all()));
            prop_eq!(plain, render(&traced), "the tracing inspector changed the outcome");
            Ok(())
        },
    );
}

/// Whether the transaction is exempt from history gas and the per-transaction limits: a deposit,
/// a transaction the protocol sent, or one the engine promotes to a deposit.
fn is_exempt(tx: &MegaTransaction) -> bool {
    tx.0.deposit.source_hash != alloy_primitives::B256::ZERO ||
        is_system_originated(tx, SYSTEM_ADDRESS)
}

/// The account `address` had before the transaction, if any: an empty one is none, as the
/// engine sees it.
fn pre_account(db: &mut MemoryDatabase, address: Address) -> Option<revm::state::AccountInfo> {
    db.basic(address).unwrap_or_default().filter(|info| !info.is_empty())
}

/// The gas ledgers, as the spec defines them:
///
/// - the three ledgers add up to the raw spend: the regular ledger is defined as the spend less the
///   two others, so what is checked is that the spend covers them;
/// - the receipt's figure is the raw spend less the refund, at least the floor, at most the gas
///   limit;
/// - the refund is within its cap, a fifth of the raw spend;
/// - the reservoir is the gas above the execution cap, and it pays the state and history gas before
///   the regular budget does and nothing else: what is gone from it is exactly the state and
///   history ledgers, or all of it when they exceed it. Two rules bill it otherwise and are left
///   out: the EIP-7623 floor, when it is the receipt's figure, and op-revm's failed deposit, which
///   bills the whole gas limit;
/// - an exempt transaction pays no history and reports no history bytes; any other reports at least
///   its body and pays at most what its bytes cost, the allowances paying the rest;
/// - value is conserved: the balances change by what a deposit minted, less what a self-destruction
///   burned;
/// - the sender's nonce moves by one, or by more when its own code creates.
#[test]
fn test_property_gas_ledgers() {
    check(
        "gas_ledgers",
        CASES,
        || case(Flavor::Satin),
        |case| {
            let tx = case.transaction();
            let Ok(outcome) = case.execute() else { return Ok(()) };
            let result_gas = outcome.result.gas();
            let gas = outcome.gas;
            let total = result_gas.total_gas_spent();
            let gas_limit = tx.gas_limit();
            let failed_deposit = matches!(
                &outcome.result,
                ExecutionResult::Halt { reason: mega_evm::MegaHaltReason::FailedDeposit, .. }
            );
            if failed_deposit {
                // op-revm's rule: a deposit that halts or is refused after Regolith bills its whole
                // gas limit, the reservoir included, and everything it did is discarded.
                prop_eq!(total, gas_limit, "a failed deposit bills its whole gas limit");
                prop_eq!(gas.reservoir_remaining, 0, "a failed deposit keeps no reservoir");
                prop_eq!(
                    (gas.state, gas.history),
                    (0, 0),
                    "a failed deposit reports no state or history gas"
                );
            }

            // The regular ledger is by definition the spend less the two others, so the three
            // add up unless that subtraction saturates: this is the check that it does not.
            prop_check!(
                total >= gas.state + gas.history,
                "the spend covers the state and history ledgers, so the three add up"
            );
            prop_eq!(
                gas.gas_used,
                total.saturating_sub(result_gas.inner_refunded()).max(gas.floor),
                "the receipt figure is the spend less the refund, at least the floor"
            );
            prop_check!(gas.gas_used <= gas_limit, "the receipt figure is within the gas limit");
            prop_check!(result_gas.inner_refunded() * 5 <= total, "the refund is within a fifth");

            let reservoir = gas_limit.saturating_sub(gas_limit.min(TX_GAS_LIMIT_CAP));
            prop_check!(
                gas.reservoir_remaining <= reservoir,
                "no more comes back than was in the reservoir"
            );
            if !failed_deposit && gas.gas_used != gas.floor {
                prop_eq!(
                    reservoir - gas.reservoir_remaining,
                    reservoir.min(gas.state + gas.history),
                    "the reservoir pays the state and history gas first, and nothing else"
                );
            }

            if is_exempt(&tx) {
                prop_eq!(
                    (gas.history, gas.history_bytes),
                    (0, 0),
                    "an exempt transaction pays no history"
                );
            } else {
                prop_check!(
                    gas.history_bytes >= transaction_body_bytes(&tx),
                    "the body is in the history bytes"
                );
                let worth =
                    mega_evm::history_gas(gas.history_bytes).expect("the bytes have a price");
                prop_check!(gas.history <= worth, "the history gas is at most what the bytes cost");
            }

            let mut pre = case.database();
            let mut delta = alloy_primitives::I256::ZERO;
            for (address, account) in &outcome.state {
                let before = pre_account(&mut pre, *address).map_or(U256::ZERO, |a| a.balance);
                let after = account.info.balance;
                let signed = |v: U256| alloy_primitives::I256::try_from(v).expect("a balance fits");
                delta += signed(after) - signed(before);
            }
            let mint = alloy_primitives::I256::try_from(U256::from(tx.0.deposit.mint.unwrap_or(0)))
                .expect("a mint fits");
            if case.destroys() {
                prop_check!(
                    delta <= mint,
                    "value is conserved but for what a destruction burned: {delta} vs {mint}"
                );
            } else {
                prop_eq!(delta, mint, "value is conserved");
            }

            let sender = tx.caller();
            let before = pre_account(&mut pre, sender).map_or(0, |a| a.nonce);
            let after = outcome.state.get(&sender).map_or(before, |a| a.info.nonce);
            // A sender that runs code, through a delegation or as an applied authority, may create
            // and so bump its nonce further.
            let runs_code = case.world.delegation.is_some_and(|d| d.delegator == Who::Caller) ||
                matches!(&case.tx.shape, Shape::Eip7702 { auths, .. } if auths.iter().any(|a| a.authority == Who::Caller));
            if runs_code {
                prop_check!(
                    after > before,
                    "the sender's nonce moves by at least one: {before} -> {after}"
                );
            } else {
                prop_eq!(after, before + 1, "the sender's nonce moves by one");
            }
            Ok(())
        },
    );
}

/// After every instruction the KV count never weighs more than the data size, `KV × 40 ≤ data
/// size`; at the end the same holds, the history bytes are within the data size, and every
/// transaction keeps at least its body and the Oracle hints it forwarded.
#[test]
fn test_property_kv_weighs_no_more_than_data_size() {
    #[derive(Default)]
    struct Probe {
        steps: u64,
        over: Option<(mega_evm::LimitUsage, u64)>,
    }
    impl Inspector<MegaContext<MemoryDatabase, Envs>, EthInterpreter> for Probe {
        fn step_end(
            &mut self,
            _: &mut Interpreter<EthInterpreter>,
            context: &mut MegaContext<MemoryDatabase, Envs>,
        ) {
            self.steps += 1;
            let usage = context.additional_limit().usage();
            if usage.write_records * WRITE_RECORD_SIZE > usage.data_size {
                self.over.get_or_insert((usage, self.steps));
            }
        }
    }
    check(
        "kv_weighs_no_more_than_data_size",
        CASES,
        || case(Flavor::Satin),
        |case| {
            let tx = case.transaction();
            let (execution, evm) = case.execute_inspected(Probe::default());
            prop_eq!(
                evm.inspector().over,
                None,
                "KV x 40 exceeded the data size after an instruction"
            );
            let Ok(outcome) = execution else { return Ok(()) };
            let usage = outcome.usage;
            prop_check!(
                usage.write_records * WRITE_RECORD_SIZE <= usage.data_size,
                "KV x 40 exceeded the data size at the end: {usage:?}"
            );
            prop_check!(
                outcome.gas.history_bytes <= usage.data_size,
                "the history bytes are within the data size: {:?} vs {usage:?}",
                outcome.gas
            );
            // What left the machine stays counted, whatever became of the transaction, an exempt
            // one and a failed deposit included: the body, which is in the block, and the hints
            // forwarded to the oracle service.
            let hints = forwarded_hint_bytes(&evm);
            prop_check!(
                usage.data_size >= transaction_body_bytes(&tx) + hints,
                "the body and the {hints} bytes of hints forwarded are counted: {usage:?}"
            );
            Ok(())
        },
    );
}

/// What the Oracle hints `evm`'s transaction forwarded weigh as data size. A forwarded hint's
/// payload is its call's whole input: the selector, the topic, the offset and length of the data,
/// and the data padded to a word.
fn forwarded_hint_bytes<I>(evm: &mega_evm::MegaEvm<MemoryDatabase, I, Envs>) -> u64 {
    evm.ctx()
        .external_envs()
        .oracle_env
        .recorded_hints()
        .iter()
        .map(|hint| 4 + 3 * 32 + (hint.data.len() as u64).div_ceil(32) * 32)
        .sum()
}

/// A stopped transaction keeps only what the spec says survives: the stop is a revert carrying
/// `MegaLimitExceeded`, with no log; no storage write stands; no account changes but the sender's
/// nonce and fee, the fee recipients' credit, an applied EIP-7702 authority's nonce and
/// delegation, and a deposit's mint; the records kept are the applied authorities', and the data
/// size is the body, those records and the payloads of the Oracle hints it forwarded, which have
/// left the machine and are counted whatever the frame that sent them did, and nothing of a hint
/// that crossed the limit, which was not forwarded (a hint that was admitted and did not decode
/// is counted too, so where a program can send one the hints forwarded are a lower bound); the
/// state gas kept is the applied
/// authorizations', the sender's own delegation included, and that of the caller account a deposit
/// created, which is the body's account and no record. An authority that is also a fee recipient
/// is credited the fee, as any fee recipient is.
#[test]
fn test_property_a_stop_keeps_only_what_survives() {
    check(
        "a_stop_keeps_only_what_survives",
        CASES,
        || case(Flavor::Satin),
        |case| {
            let tx = case.transaction();
            let (execution, evm) = case.execute_with_evm();
            let Ok(outcome) = execution else { return Ok(()) };
            let Some(stop) = outcome.limit_exceeded else { return Ok(()) };
            check_survivors(case, &tx, &outcome, stop, forwarded_hint_bytes(&evm))
        },
    );
}

/// What [`test_property_a_stop_keeps_only_what_survives`] checks of a stopped transaction;
/// `hint_bytes` is what the Oracle hints it forwarded weigh.
pub(crate) fn check_survivors(
    case: &Case,
    tx: &MegaTransaction,
    outcome: &MegaTransactionOutcome,
    stop: LimitCheck,
    hint_bytes: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let rendered = render_outcome(outcome);
    let LimitCheck::ExceedsLimit { frame_local, .. } = stop else {
        return Err(crate::harness::fail(format!(
            "a latched check that is within the limit\n{rendered}"
        )));
    };
    prop_check!(!frame_local, "a frame budget latched the transaction\n{rendered}");
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => {
            prop_eq!(*output, stop.revert_data(), "the stop's revert data\n{rendered}");
        }
        other => {
            return Err(crate::harness::fail(format!(
                "a stop that is not a revert: {other:?}\n{rendered}"
            )))
        }
    }
    prop_check!(outcome.result.logs().is_empty(), "a stop keeps no log\n{rendered}");

    let sender = tx.caller();
    let mut pre = case.database();
    let is_deposit = tx.0.deposit.source_hash != alloy_primitives::B256::ZERO;
    let fee_recipients = [
        case.world.beneficiary_address(),
        op_revm::constants::BASE_FEE_RECIPIENT,
        op_revm::constants::L1_FEE_RECIPIENT,
        op_revm::constants::OPERATOR_FEE_RECIPIENT,
    ];
    // The authorizations that apply, by revm's rules, and what the state says of them: either
    // every one applied, or the latch took them all back before the first frame.
    let simulated = applied_authorizations(case);
    let mut expected: BTreeMap<Address, (u64, Delegate)> = BTreeMap::new();
    for (authority, delegate) in &simulated {
        let entry = expected.entry(*authority).or_insert((0, *delegate));
        entry.0 += 1;
        entry.1 = *delegate;
    }
    let bumped = |address: Address, state_nonce: u64| {
        let before = pre_account(&mut case.database(), address).map_or(0, |a| a.nonce);
        state_nonce > before + u64::from(address == sender)
    };
    let taken_back = !expected.is_empty() &&
        expected
            .keys()
            .all(|a| outcome.state.get(a).is_none_or(|acc| !bumped(*a, acc.info.nonce)));
    if taken_back {
        expected.clear();
    }
    let applied = expected.keys().filter(|a| **a != sender).count() as u64;
    let mut created_caller = 0u64;
    for (address, account) in &outcome.state {
        let before = pre_account(&mut pre, *address);
        let before_nonce = before.as_ref().map_or(0, |a| a.nonce);
        let before_balance = before.as_ref().map_or(U256::ZERO, |a| a.balance);
        let before_code =
            before.as_ref().map_or(alloy_primitives::KECCAK256_EMPTY, |a| a.code_hash);
        for (key, slot) in &account.storage {
            prop_eq!(
                slot.present_value,
                slot.original_value,
                "a stop keeps no storage write: {address} slot {key}\n{rendered}"
            );
        }
        let is_sender = *address == sender;
        if let Some((bumps, delegate)) = expected.get(address) {
            prop_eq!(
                account.info.nonce,
                before_nonce + u64::from(is_sender) + bumps,
                "an applied authority's nonce moves once per authorization: {address}\n{rendered}"
            );
            let expected_code = match delegate {
                Delegate::Zero => alloy_primitives::KECCAK256_EMPTY,
                other => Bytecode::new_eip7702(match other {
                    Delegate::Contract => Who::Contract.address(),
                    Delegate::A => Who::A.address(),
                    Delegate::B => Who::B.address(),
                    Delegate::Beneficiary => Who::Beneficiary.address(),
                    Delegate::Zero => unreachable!(),
                })
                .hash_slow(),
            };
            prop_eq!(
                account.info.code_hash,
                expected_code,
                "an applied authority delegates: {address}\n{rendered}"
            );
            if is_sender {
                prop_check!(
                    account.info.balance <= before_balance,
                    "the sender pays and receives nothing\n{rendered}"
                );
            } else if fee_recipients.contains(address) {
                // An authority may be the block beneficiary, which the transaction's fee is
                // credited to whatever stopped it.
                prop_check!(
                    account.info.balance >= before_balance,
                    "an authority that is a fee recipient is credited: {address}\n{rendered}"
                );
            } else {
                prop_eq!(
                    account.info.balance,
                    before_balance,
                    "an authority's balance stands: {address}\n{rendered}"
                );
            }
            continue;
        }
        if is_sender {
            prop_eq!(
                account.info.nonce,
                before_nonce + 1,
                "the sender's nonce moves by one\n{rendered}"
            );
            prop_eq!(account.info.code_hash, before_code, "the sender's code stands\n{rendered}");
            if is_deposit {
                let mint = U256::from(tx.0.deposit.mint.unwrap_or(0));
                prop_eq!(
                    account.info.balance,
                    before_balance + mint,
                    "a deposit keeps its mint\n{rendered}"
                );
                if before.is_none() {
                    created_caller = 1;
                }
            } else {
                prop_check!(
                    account.info.balance <= before_balance,
                    "the sender pays and receives nothing\n{rendered}"
                );
            }
            continue;
        }
        if fee_recipients.contains(address) {
            prop_eq!(
                account.info.nonce,
                before_nonce,
                "a fee recipient's nonce stands: {address}\n{rendered}"
            );
            prop_eq!(
                account.info.code_hash,
                before_code,
                "a fee recipient's code stands: {address}\n{rendered}"
            );
            prop_check!(
                account.info.balance >= before_balance,
                "a fee recipient is credited: {address}\n{rendered}"
            );
            continue;
        }
        prop_eq!(
            (account.info.nonce, account.info.balance, account.info.code_hash),
            (before_nonce, before_balance, before_code),
            "an account a stop touched stands as before: {address}\n{rendered}"
        );
    }

    let usage = outcome.usage;
    prop_eq!(
        usage.write_records,
        applied,
        "the records kept are the distinct applied authorities' but the sender's\n{rendered}"
    );
    // The hints counted: what the data size holds beside the body and the records. Every hint
    // forwarded is in it. A hint that was admitted and did not decode is in it too, and reached
    // nobody: only a raw call of the Oracle, whose calldata is whatever memory holds, sends one.
    let priced = transaction_body_bytes(tx) + usage.write_records * WRITE_RECORD_SIZE;
    let Some(hints_counted) = usage.data_size.checked_sub(priced) else {
        return Err(crate::harness::fail(format!(
            "the data size kept is below the body and the records\n{rendered}"
        )));
    };
    prop_check!(
        hints_counted >= hint_bytes,
        "a forwarded hint stays counted: {hints_counted} < {hint_bytes}\n{rendered}"
    );
    if !case.may_send_a_hint_that_does_not_decode() {
        prop_eq!(
            hints_counted,
            hint_bytes,
            "the data size kept is the body, the records and the hints forwarded\n{rendered}"
        );
    }
    if !is_exempt(tx) {
        prop_eq!(
            outcome.gas.history_bytes,
            priced,
            "the history bytes are the data size kept but the hints\n{rendered}"
        );
        let worth = mega_evm::history_gas(priced).expect("the bytes have a price");
        prop_eq!(
            outcome.gas.history,
            worth,
            "the history gas is the body's and the records'\n{rendered}"
        );
    }
    // Every applied authorization keeps the state gas of its delegation, the sender's own
    // included: a sender that delegates itself makes no record, its account being the body's, and
    // pays the state gas of the designator all the same.
    if expected.is_empty() && created_caller == 0 {
        prop_eq!(outcome.gas.state, 0, "a stop keeps no state gas\n{rendered}");
    }
    Ok(())
}

/// The EIP-7702 authorizations of `case` that revm applies, in order, each with its delegate: the
/// signature recovers, the chain id is this chain's, the nonce is the authority's at that point
/// (the sender's after its own bump), and the authority has no code but a delegation.
fn applied_authorizations(case: &Case) -> Vec<(Address, Delegate)> {
    let Shape::Eip7702 { auths, .. } = &case.tx.shape else { return Vec::new() };
    let world = &case.world;
    let sender = Who::Caller.address();
    let mut nonces: BTreeMap<Address, u64> = BTreeMap::new();
    nonces.insert(sender, world.nonce_of(sender) + 1);
    let has_plain_code = |address: Address| {
        let program = [(Who::Contract, &case.main), (Who::A, &case.a), (Who::B, &case.b)]
            .into_iter()
            .find(|(who, _)| who.address() == address)
            .map(|(_, program)| program);
        let delegates = world.delegation.is_some_and(|d| d.delegator.address() == address);
        !delegates && program.is_some_and(|p| !p.assemble().is_empty())
    };
    let mut applied = Vec::new();
    for auth in auths {
        if !auth.recovers || !auth.chain_ok {
            continue;
        }
        let authority = auth.authority.address();
        let nonce = world.nonce_of(authority) + u64::from(!auth.nonce_ok);
        if nonce != nonces.get(&authority).copied().unwrap_or_else(|| world.nonce_of(authority)) {
            continue;
        }
        if has_plain_code(authority) {
            continue;
        }
        nonces.insert(authority, nonce + 1);
        applied.push((authority, auth.delegate));
    }
    applied
}
