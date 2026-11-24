//! The contracts of `mega-evm`'s `tests/satin/reservoir_drain.rs` under Rex6, on the released
//! engine the legacy leg links: one gas pool, so EIP-150's forward bounds everything a callee
//! spends, its storage gas included.
//!
//! `A` reads `GAS`, calls `C` with a forward of 1,000,000, reads `GAS` again, writes `K` fresh
//! slots and returns the two readings and the call's flag. `C` is `STOP`, `INVALID` (which burns
//! its forward), or the loop of `LOG0`s of 512 bytes that the Satin test runs, byte for byte: it
//! stops when an iteration's `GAS` drop exceeds the log's regular cost (4,478 with the closing
//! `GAS`), or when its gas falls below 52,481. Under one pool the drop always includes the log's
//! storage gas, so that callee logs once; the greedy one, which never stops on the drop, logs
//! until its forward runs low.
//!
//! `K` is the most slots `A` completes after the `INVALID` callee, found by running it. `A`'s
//! work completes after every callee, because none of them spends more than the forward.
//!
//! The gas limit is 20,000,000, where the gas binds `A`'s work before Rex6's limit of 1,000 new
//! slots per transaction. Without the work, each callee spends the same at 20,000,000 and at
//! 400,000,000: a larger gas limit gives it nothing more. The tests print their tables
//! (`--nocapture`).

#![cfg(feature = "legacy")]

use mega_evm_legacy::{
    alloy_primitives::{address, Address, Bytes, TxKind, U256},
    revm::{
        bytecode::opcode::{
            ADD, CALL, DUP1, DUP2, GAS, GT, INVALID, ISZERO, JUMP, JUMPDEST, JUMPI, LOG0, LT,
            MSTORE, POP, PUSH0, PUSH1, PUSH2, PUSH20, PUSH4, RETURN, SSTORE, STOP, SUB, SWAP1,
        },
        context::{result::ExecutionResult, BlockEnv, TxEnv},
    },
    test_utils::MemoryDatabase,
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};

const SENDER: Address = address!("0000000000000000000000000000000000710000");
const A: Address = address!("0000000000000000000000000000000000710001");
const C: Address = address!("0000000000000000000000000000000000710002");

const FORWARD: u32 = 1_000_000;

// The Satin test's numbers for its `LOG0` callee, which that test holds to these values.

/// The regular gas of one log with its operands and the closing `GAS`.
const STOP_ABOVE: u32 = 4_478;
/// The gas the callee keeps before another iteration.
const RESERVE: u32 = 52_481;

/// A callee: its name in the tables and its code.
#[derive(Clone, Debug)]
struct Callee {
    name: &'static str,
    code: Bytes,
}

impl Callee {
    fn new(name: &'static str, code: Bytes) -> Self {
        Self { name, code }
    }

    fn burns() -> Self {
        Self::new("INVALID", Bytes::from_static(&[INVALID]))
    }
}

/// The `LOG0` callee: 512 bytes of data from memory expanded first, in a loop that reads `GAS`
/// around each log.
fn logging(stop_on_spill: bool) -> Bytes {
    let stop_above = if stop_on_spill { STOP_ABOVE } else { u32::MAX };
    let mut code = vec![PUSH0, PUSH2, 0x01, 0xe0, MSTORE, PUSH0];
    let top = code.len();
    code.extend([JUMPDEST, GAS, PUSH2, 0x02, 0x00, PUSH0, LOG0, GAS, DUP1, PUSH4]);
    code.extend(RESERVE.to_be_bytes());
    code.extend([GT, PUSH2, 0, 0, JUMPI]);
    let out_on_reserve = code.len() - 3;
    code.extend([SWAP1, SUB, PUSH4]);
    code.extend(stop_above.to_be_bytes());
    code.extend([LT, PUSH2, 0, 0, JUMPI]);
    let out_on_drop = code.len() - 3;
    code.extend([PUSH1, 1, ADD, PUSH2]);
    code.extend((top as u16).to_be_bytes());
    code.push(JUMP);
    let out = (code.len() as u16).to_be_bytes();
    code.extend([JUMPDEST, STOP]);
    code[out_on_reserve..out_on_reserve + 2].copy_from_slice(&out);
    code[out_on_drop..out_on_drop + 2].copy_from_slice(&out);
    code.into()
}

/// `A`: `GAS`, `CALL(FORWARD, C)`, `GAS`, `work` fresh slots, and the two readings and the flag
/// returned.
fn caller(work: u32) -> Bytes {
    let mut code = vec![GAS, PUSH0, PUSH0, PUSH0, PUSH0, PUSH0, PUSH20];
    code.extend(C);
    code.push(PUSH4);
    code.extend(FORWARD.to_be_bytes());
    code.extend([CALL, GAS, PUSH4]);
    code.extend(work.to_be_bytes());
    let top = code.len() as u16;
    code.extend([JUMPDEST, DUP1, ISZERO, PUSH2, 0, 0, JUMPI]);
    let out_at = code.len() - 3;
    code.extend([PUSH1, 1, DUP2, SSTORE, PUSH1, 1, SWAP1, SUB, PUSH2]);
    code.extend(top.to_be_bytes());
    code.push(JUMP);
    let out = (code.len() as u16).to_be_bytes();
    code[out_at..out_at + 2].copy_from_slice(&out);
    code.extend([JUMPDEST, POP, PUSH1, 0x40, MSTORE, PUSH1, 0x20, MSTORE, PUSH0, MSTORE]);
    code.extend([PUSH1, 0x60, PUSH0, RETURN]);
    code.into()
}

/// One run's figures.
#[derive(Clone, Debug)]
struct Run {
    callee: &'static str,
    gas_limit: u64,
    work: u32,
    status: String,
    gas_used: u64,
    logs: usize,
    /// `A`'s `GAS` before and after the call, when `A` returned.
    gas_around_call: Option<(u64, u64)>,
    /// The call's flag, when `A` returned.
    call_succeeded: Option<bool>,
}

impl Run {
    fn completed(&self) -> bool {
        self.gas_around_call.is_some()
    }
}

fn run(gas_limit: u64, callee: Callee, work: u32) -> Run {
    let db = MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10u64.pow(18)))
        .account_code(A, caller(work))
        .account_code(C, callee.code);
    let block = BlockEnv { number: U256::from(1), gas_limit: 10_000_000_000, ..Default::default() };
    let mut context = MegaContext::new(db, MegaSpecId::REX6).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut tx = MegaTransaction::new(TxEnv {
        caller: SENDER,
        kind: TxKind::Call(A),
        gas_limit,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = MegaEvm::new(context).execute_transaction(tx).expect("the transaction is valid");
    let (status, words) = match &outcome.result {
        ExecutionResult::Success { output, .. } => {
            let words: Vec<u64> =
                output.data().chunks(32).map(|w| U256::from_be_slice(w).to::<u64>()).collect();
            ("success".to_string(), Some(words))
        }
        ExecutionResult::Revert { .. } => ("revert".to_string(), None),
        ExecutionResult::Halt { reason, .. } => (format!("halt: {reason:?}"), None),
    };
    Run {
        callee: callee.name,
        gas_limit,
        work,
        status,
        gas_used: outcome.result.gas_used(),
        logs: outcome.result.logs().len(),
        gas_around_call: words.as_ref().map(|words| (words[0], words[2])),
        call_succeeded: words.as_ref().map(|words| words[1] == 1),
    }
}

fn callees() -> [Callee; 3] {
    [
        Callee::new("STOP", Bytes::from_static(&[STOP])),
        Callee::new("LOG0 512 B", logging(true)),
        Callee::new("LOG0 512 B, greedy", logging(false)),
    ]
}

/// The most slots `A` completes after a callee that burns its forward.
fn work_after_a_burned_forward(gas_limit: u64) -> u32 {
    let (mut fits, mut fails) = (0, 1_000);
    let too_many = run(gas_limit, Callee::burns(), fails);
    assert!(too_many.status.contains("OutOfGas"), "the gas binds before 1,000 slots: {too_many:?}");
    while fails - fits > 1 {
        let mid = (fits + fails) / 2;
        if run(gas_limit, Callee::burns(), mid).completed() {
            fits = mid;
        } else {
            fails = mid;
        }
    }
    fits
}

fn print_table(title: &str, rows: &[Run], baseline_after: u64) {
    println!("\n### {title}\n");
    println!(
        "| gas limit | C | logs | C spend | C spend / forward | A GAS before | A GAS after | \
         A work (slots) | status | gas used |"
    );
    println!("|{}", "---:|".repeat(10));
    for row in rows {
        let shown = |figure: Option<String>| figure.unwrap_or_else(|| "-".into());
        let before = shown(row.gas_around_call.map(|(before, _)| before.to_string()));
        let after = shown(row.gas_around_call.map(|(_, after)| after.to_string()));
        let spend = row.gas_around_call.map(|(_, after)| baseline_after - after);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.gas_limit,
            row.callee,
            row.logs,
            shown(spend.map(|s| s.to_string())),
            shown(spend.map(|s| format!("{:.2}", s as f64 / f64::from(FORWARD)))),
            before,
            after,
            row.work,
            row.status,
            row.gas_used,
        );
    }
}

/// Holds every callee to its forward: its spend, read as `A`'s `GAS` after the call against
/// `A`'s after a callee that returned at once, is at most the forward, and `A`'s work completes.
#[track_caller]
fn assert_within_the_forward(rows: &[Run]) -> u64 {
    assert!(rows.iter().all(Run::completed), "A's work completes after every callee: {rows:#?}");
    assert!(rows.iter().all(|row| row.call_succeeded == Some(true)), "every callee returns");
    let baseline_after = rows[0].gas_around_call.unwrap().1;
    for row in rows {
        let spend = baseline_after - row.gas_around_call.unwrap().1;
        assert!(spend <= u64::from(FORWARD), "{}: {spend} within the forward", row.callee);
    }
    baseline_after
}

#[test]
fn test_rex6_a_callee_spends_within_its_forward() {
    const GAS_LIMIT: u64 = 20_000_000;
    let work = work_after_a_burned_forward(GAS_LIMIT);
    let mut rows: Vec<_> = callees().into_iter().map(|c| run(GAS_LIMIT, c, work)).collect();
    let baseline_after = assert_within_the_forward(&rows);
    assert_eq!(rows[1].logs, 1, "the drop includes the log's storage gas from the first log on");
    assert!(rows[2].logs > 1, "the greedy callee logs until its forward runs low");

    // The two ends of the calibration, for the table.
    rows.extend([run(GAS_LIMIT, Callee::burns(), work), run(GAS_LIMIT, Callee::burns(), work + 1)]);
    print_table("6a. Rex6, gas limit 20M, forward 1M", &rows, baseline_after);
}

#[test]
fn test_rex6_a_callee_spends_the_same_at_a_larger_gas_limit() {
    let spends = |gas_limit| {
        let rows: Vec<_> = callees().into_iter().map(|c| run(gas_limit, c, 0)).collect();
        let baseline_after = assert_within_the_forward(&rows);
        let spends: Vec<_> =
            rows.iter().map(|row| baseline_after - row.gas_around_call.unwrap().1).collect();
        (rows, baseline_after, spends)
    };
    let (_, _, at_20m) = spends(20_000_000);
    let (rows, baseline_after, at_400m) = spends(400_000_000);
    assert_eq!(at_20m, at_400m, "each callee spends the same at either gas limit");
    print_table("6b. Rex6, gas limit 400M, forward 1M, no work", &rows, baseline_after);
}
