//! What `MegaLimitControl.remainingComputeGas()` answers: the compute the calling frame could
//! still spend, the lesser of its own regular gas — with the gas the call forwarded counted back,
//! because the answer hands it back — and what gas detention's limit leaves the transaction once
//! it read volatile data.
//!
//! Most cases read the caller's gas with `GAS` right after the call returns: the answer gives the
//! forward back untouched, so the caller's own gas at the call is that reading plus the `POP` of
//! the call's status and the `GAS` itself, four gas.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, TX_GAS_LIMIT_CAP},
    system::{IMegaLimitControl, IOracle, LIMIT_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::*,
    context::{BlockEnv, TxEnv},
    interpreter::{interpreter::EthInterpreter, CallInputs, CallOutcome, Interpreter},
    Inspector,
};

use crate::common::{system_db, CALLER};

const CONTRACT: Address = address!("0x0000000000000000000000000000000000300001");
const CONTRACT2: Address = address!("0x0000000000000000000000000000000000300002");
const CONTRACT3: Address = address!("0x0000000000000000000000000000000000300003");

/// The cap a read of the block environment sets.
const CAP: u64 = BLOCK_ENV_ACCESS_COMPUTE_GAS;

/// Below the execution cap: no reservoir.
const BELOW: u64 = 100_000_000;
/// Above it: a reservoir of 100,000,000.
const ABOVE: u64 = TX_GAS_LIMIT_CAP + 100_000_000;
const TIERS: [u64; 2] = [BELOW, ABOVE];

/// What the POP of the call's status and the `GAS` after it cost.
const READING: u64 = 4;

const SELECTOR: [u8; 4] = IMegaLimitControl::remainingComputeGasCall::SELECTOR;

/// A block whose beneficiary is `beneficiary`.
fn block(beneficiary: Address) -> BlockEnv {
    BlockEnv { number: U256::from(1), gas_limit: 10_000_000_000, beneficiary, ..Default::default() }
}

/// A transaction from [`CALLER`] to `to` with `data`, carrying `gas_limit`.
fn tx(to: Address, data: &[u8], gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data: Bytes::copy_from_slice(data),
        gas_limit,
        ..Default::default()
    }))
}

/// Runs `tx` over `db` in a block whose beneficiary is `beneficiary`, under the default limits,
/// which detain.
fn execute_in(
    db: MemoryDatabase,
    beneficiary: Address,
    tx: MegaTransaction,
) -> MegaTransactionOutcome {
    let ctx = MegaContext::new(db, MegaSpecId::SATIN)
        .with_block(block(beneficiary))
        .with_chain(zero_fee_l1_block_info());
    let outcome = MegaEvm::new(ctx).execute_transaction(tx).expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    outcome
}

/// Runs `tx` over `db`, the beneficiary being nobody the tests use.
fn execute(db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    execute_in(db, Address::ZERO, tx)
}

/// The words the transaction returned.
fn words(outcome: &MegaTransactionOutcome) -> Vec<u64> {
    let output = outcome.result.output().cloned().unwrap_or_default();
    output.chunks(32).map(|word| U256::from_be_slice(word).to::<u64>()).collect()
}

/// Appends a query through `scheme`, forwarding `gas` (all it has when `None`), that keeps the
/// answer at `0x20 * slot` and, next to it, what `GAS` reads once the call returned.
fn query(code: BytecodeBuilder, scheme: u8, gas: Option<u64>, slot: u64) -> BytecodeBuilder {
    let code = code
        .mstore(0x200, SELECTOR)
        .push_number(32_u8) // retSize
        .push_number(0x20 * slot) // retOffset
        .push_number(4_u8) // argsSize
        .push_number(0x200_u16); // argsOffset
    let code = if scheme == CALL { code.push_number(0_u8) } else { code };
    let code = code.push_address(LIMIT_CONTROL_ADDRESS);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    code.append_many([scheme, POP, GAS]).push_number(0x20 * (slot + 1)).append(MSTORE)
}

/// Appends a return of the first `words` words of memory.
fn returning(code: BytecodeBuilder, words: u64) -> Bytes {
    code.push_number(0x20 * words).append_many([PUSH0, RETURN]).build()
}

/// Appends `rounds` pushes and pops, five gas each.
fn burn(mut code: BytecodeBuilder, rounds: u32) -> BytecodeBuilder {
    for _ in 0..rounds {
        code = code.push_number(1_u8).append(POP);
    }
    code
}

/// The answer to a query `code` makes, and what `GAS` read after it, running as [`CONTRACT`].
fn answer_of(code: Bytes, gas_limit: u64) -> (u64, u64) {
    let outcome = execute(system_db().account_code(CONTRACT, code), tx(CONTRACT, &[], gas_limit));
    let words = words(&outcome);
    (words[0], words[1])
}

/* ---------- the legacy engine's rows ---------- */

/// A transaction that calls the contract itself hears the regular gas its own frame was given:
/// its gas limit less all it paid before the frame below the execution cap, where nothing but
/// regular gas pays; the cap less its intrinsic regular gas above it, where the reservoir paid the
/// rest.
#[test]
fn test_direct_tx_remaining_compute_gas() {
    for gas_limit in TIERS {
        let outcome = execute(system_db(), tx(LIMIT_CONTROL_ADDRESS, &SELECTOR, gas_limit));
        let answer = IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(
            outcome.result.output().unwrap(),
        )
        .unwrap();
        if gas_limit < TX_GAS_LIMIT_CAP {
            assert_eq!(answer, gas_limit - outcome.result.tx_gas_used(), "below the cap");
        } else {
            assert_eq!(answer, TX_GAS_LIMIT_CAP - outcome.gas.regular, "above the cap");
            assert!(outcome.gas.history > 0, "the body's history came out of the reservoir");
        }
    }
}

/// A `STATICCALL` hears the caller's own gas, the forward counted back.
#[test]
fn test_remaining_compute_gas_staticcall() {
    for gas_limit in TIERS {
        let (answer, gas) = answer_of(
            returning(query(BytecodeBuilder::default(), STATICCALL, None, 0), 2),
            gas_limit,
        );
        assert_eq!(answer, gas + READING, "at {gas_limit}");
        assert!(answer <= TX_GAS_LIMIT_CAP, "regular gas never exceeds the execution cap");
    }
}

/// Compute spent before the query comes off the answer, gas for gas.
#[test]
fn test_remaining_compute_gas_decreases_after_compute_work() {
    let (base, _) =
        answer_of(returning(query(BytecodeBuilder::default(), CALL, None, 0), 2), BELOW);
    let (heavy, _) = answer_of(
        returning(query(burn(BytecodeBuilder::default(), 20_000), CALL, None, 0), 2),
        BELOW,
    );
    assert_eq!(base - heavy, 20_000 * 5);
}

/// A second query hears less than the first, by what the caller spent between the two: the
/// query itself costs the caller only its call's overhead.
#[test]
fn test_remaining_compute_gas_sequential_queries_decrease() {
    let code = query(query(BytecodeBuilder::default(), CALL, None, 0), CALL, None, 2);
    let words = words(&execute(
        system_db().account_code(CONTRACT, returning(code, 4)),
        tx(CONTRACT, &[], BELOW),
    ));
    let [first, first_gas, second, second_gas] = words[..] else { panic!("{words:?}") };
    assert_eq!(first, first_gas + READING);
    assert_eq!(second, second_gas + READING);
    assert!(second < first);
    assert!(first - second < 1_000, "the second query's overhead: {}", first - second);
}

/// The exact value: the frame's own gas, see [`test_direct_tx_remaining_compute_gas`]; here with
/// the query's calldata padded, which the transaction pays for before its frame.
#[test]
fn test_remaining_compute_gas_exact_value_matches_tracker() {
    let data: Vec<u8> = SELECTOR.iter().copied().chain([0xff; 64]).collect();
    let outcome = execute(system_db(), tx(LIMIT_CONTROL_ADDRESS, &data, 1_000_000));
    let answer = IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(
        outcome.result.output().unwrap(),
    )
    .unwrap();
    assert_eq!(answer, 1_000_000 - outcome.result.tx_gas_used());
}

/// A caller that forwards the query little still hears its own gas, not the forward: the answer
/// is about the caller.
#[test]
fn test_remaining_compute_gas_inner_call_returns_frame_remaining() {
    let (answer, gas) =
        answer_of(returning(query(BytecodeBuilder::default(), CALL, Some(100_000), 0), 2), BELOW);
    assert_eq!(answer, gas + READING);
    assert!(answer > 99_000_000, "{answer}");
}

/// A query from a frame two levels down hears that frame's gas, not its caller's.
#[test]
fn test_remaining_compute_gas_two_level_nesting_returns_inner_frame() {
    let inner = returning(query(BytecodeBuilder::default(), CALL, None, 0), 2);
    let outer = burn(BytecodeBuilder::default(), 50_000)
        .push_number(64_u8) // retSize
        .push_number(0_u8) // retOffset
        .append_many([PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT2)
        .push_number(50_000_000_u32)
        .append_many([CALL, POP]);
    let outer = returning(outer, 2);
    let db = system_db().account_code(CONTRACT, outer).account_code(CONTRACT2, inner);
    let words = words(&execute(db, tx(CONTRACT, &[], BELOW)));
    assert_eq!(words[0], words[1] + READING);
    assert!(words[0] < 50_000_000, "at most what the frame was given: {}", words[0]);
}

/// What a child spent is gone from its caller's answer even when the child reverted: compute is
/// not undone.
#[test]
fn test_remaining_compute_gas_persistent_after_inner_revert() {
    let child = burn(BytecodeBuilder::default(), 20_000).revert().build();
    let caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT2)
        .push_number(50_000_000_u32)
        .append_many([CALL, POP]);
    let caller = returning(query(caller, CALL, None, 0), 2);
    let db = system_db().account_code(CONTRACT, caller).account_code(CONTRACT2, child);
    let after_revert = words(&execute(db, tx(CONTRACT, &[], BELOW)));
    let (base, _) =
        answer_of(returning(query(BytecodeBuilder::default(), CALL, None, 0), 2), BELOW);
    assert_eq!(after_revert[0], after_revert[1] + READING);
    assert!(base - after_revert[0] > 20_000 * 5, "the child's compute stays spent");
}

/// After a read of the block environment the answer is what the cap leaves: the cap less what the
/// caller spent since the read, which `GAS` right after the read and right after the query
/// measure. The caller's own gas is far above it.
#[test]
fn test_remaining_compute_gas_clamped_by_detention_limit() {
    for gas_limit in TIERS {
        let code = BytecodeBuilder::default()
            .append_many([TIMESTAMP, POP, GAS])
            .append_many([PUSH0, MSTORE]);
        // The read's gas at 0, the answer at 0x20, the gas after it at 0x40.
        let code = returning(query(code, CALL, None, 1), 3);
        let words =
            words(&execute(system_db().account_code(CONTRACT, code), tx(CONTRACT, &[], gas_limit)));
        let [at_read, answer, after] = words[..] else { panic!("{words:?}") };
        // `GAS` after the read reads the `POP` and its own two gas less than the read left; the
        // caller held `after + READING` at the query.
        let spent_since_the_read = (at_read + 2 + 2) - (after + READING);
        assert_eq!(answer, CAP - spent_since_the_read, "at {gas_limit}");
        assert!(after + READING > CAP, "the caller's own gas is not what binds");
    }
}

/* ---------- the four boundaries: below and above the cap, with and without a read ---------- */

/// A transaction that is the beneficiary is detained from its start, before its frame: calling
/// the contract directly, it hears the cap, below and above the execution cap alike. The same
/// transaction from an ordinary sender hears its frame's own gas.
#[test]
fn test_a_detained_transactions_own_call_hears_the_cap() {
    for gas_limit in TIERS {
        let detained =
            execute_in(system_db(), CALLER, tx(LIMIT_CONTROL_ADDRESS, &SELECTOR, gas_limit));
        let undetained = execute(system_db(), tx(LIMIT_CONTROL_ADDRESS, &SELECTOR, gas_limit));
        let answer = |outcome: &MegaTransactionOutcome| {
            IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(
                outcome.result.output().unwrap(),
            )
            .unwrap()
        };
        assert_eq!(answer(&detained), CAP, "at {gas_limit}");
        assert!(answer(&undetained) > CAP, "at {gas_limit}: its own gas");
    }

    // A detained transaction whose frame has less than the cap hears its own gas.
    let outcome = execute_in(system_db(), CALLER, tx(LIMIT_CONTROL_ADDRESS, &SELECTOR, 10_000_000));
    let answer = IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(
        outcome.result.output().unwrap(),
    )
    .unwrap();
    assert_eq!(answer, 10_000_000 - outcome.result.tx_gas_used());
}

/// A contract's query, without a read and after one, below and above the execution cap: its own
/// gas without the read, capped at the execution cap; what the cap leaves after it. The two tiers
/// hear the same answer after the read, and answers that differ by the reservoir's share of the
/// intrinsic cost without it.
#[test]
fn test_a_contracts_query_below_and_above_the_execution_cap() {
    let plain =
        returning(query(BytecodeBuilder::default().append_many([PUSH0, POP]), CALL, None, 0), 2);
    let read = returning(
        query(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), CALL, None, 0),
        2,
    );
    let mut after_read = vec![];
    for gas_limit in TIERS {
        let (answer, gas) = answer_of(plain.clone(), gas_limit);
        assert_eq!(answer, gas + READING, "without a read, at {gas_limit}");
        assert!(answer < gas_limit.min(TX_GAS_LIMIT_CAP));

        let (answer, gas) = answer_of(read.clone(), gas_limit);
        assert!(answer < CAP && gas + READING > answer, "after the read, at {gas_limit}");
        after_read.push(answer);
    }
    assert_eq!(after_read[0], after_read[1], "the cap leaves the same compute in both tiers");
}

/// A detained caller whose own gas is below what the cap leaves hears its own gas: the answer is
/// the lesser of the two.
#[test]
fn test_a_detained_caller_with_less_than_the_allowance_hears_its_own_gas() {
    let inner = returning(query(BytecodeBuilder::default(), CALL, None, 0), 2);
    let outer = BytecodeBuilder::default()
        .append_many([TIMESTAMP, POP])
        .push_number(64_u8) // retSize
        .push_number(0_u8) // retOffset
        .append_many([PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT2)
        .push_number(1_000_000_u32)
        .append_many([CALL, POP]);
    let db = system_db().account_code(CONTRACT, returning(outer, 2)).account_code(CONTRACT2, inner);
    let words = words(&execute(db, tx(CONTRACT, &[], BELOW)));
    assert_eq!(words[0], words[1] + READING, "the child's own gas");
    assert!(words[0] < 1_000_000);
}

/// A read a child made binds its caller's answer once the child returned: the limit is the
/// transaction's.
#[test]
fn test_a_childs_read_binds_its_callers_answer() {
    let child = BytecodeBuilder::default().append_many([TIMESTAMP, POP, STOP]).build();
    let caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CONTRACT2)
        .push_number(1_000_000_u32)
        .append_many([CALL, POP]);
    let caller = returning(query(caller, CALL, None, 0), 2);
    let db = system_db().account_code(CONTRACT, caller).account_code(CONTRACT2, child);
    let words = words(&execute(db, tx(CONTRACT, &[], BELOW)));
    assert!(words[0] < CAP && words[0] > CAP - 10_000, "what the cap leaves: {}", words[0]);
    assert!(words[1] + READING > CAP, "not the caller's own gas");
}

/* ---------- the answer is what the caller can spend when it resumes ---------- */

/// Records every answer a query returns, next to the spendable regular gas its caller resumes
/// with: what a regular charge of the caller could draw at its next instruction.
#[derive(Debug, Default)]
struct AnswerAndResume {
    pending: Option<u64>,
    pairs: Vec<(u64, u64)>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for AnswerAndResume {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if let Some(answer) = self.pending.take() {
            self.pairs.push((answer, interp.gas.tracker().spendable()));
        }
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == LIMIT_CONTROL_ADDRESS && outcome.result.is_ok() {
            self.pending = Some(U256::from_be_slice(&outcome.result.output).to::<u64>());
        }
    }
}

/// Runs `tx` over `db` in a block whose beneficiary is `beneficiary`, under the default limits,
/// and returns every answer with the spendable gas its caller resumed with.
fn answers_and_resumes(
    db: MemoryDatabase,
    beneficiary: Address,
    tx: MegaTransaction,
) -> Vec<(u64, u64)> {
    let ctx = MegaContext::new(db, MegaSpecId::SATIN)
        .with_block(block(beneficiary))
        .with_chain(zero_fee_l1_block_info());
    let mut evm = MegaEvm::new(ctx).with_inspector(AnswerAndResume::default());
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let answers = evm.inspector();
    assert_eq!(answers.pending, None, "every answer was followed by its caller's next step");
    answers.pairs.clone()
}

/// Appends a `CALL` of `target` forwarding `gas` and carrying `value`, discarding its status.
fn calls(code: BytecodeBuilder, target: Address, gas: u64, value: u64) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append_many([CALL, POP])
}

/// Code that reads the block's timestamp.
fn reads_timestamp(code: BytecodeBuilder) -> BytecodeBuilder {
    code.append_many([TIMESTAMP, POP])
}

/// Code that queries through `CALL`, forwarding all it has, and stops.
fn queries() -> Bytes {
    query(BytecodeBuilder::default(), CALL, None, 0).stop().build()
}

/// The answer is exactly the spendable regular gas the caller resumes with — what it can still
/// spend — whether nothing detains it, its own read does, a child's read does before a child that
/// halted, it was started by a value call from a caller detained or not, its sender is the
/// beneficiary, or it read the Oracle's storage; below and above the execution cap.
#[test]
fn test_the_answer_is_what_the_caller_resumes_with() {
    let get_slot = IOracle::getSlotCall { slot: U256::ZERO }.abi_encode();
    let reads_oracle = || {
        BytecodeBuilder::default()
            .mstore(0x0, &get_slot)
            .append_many([PUSH0, PUSH0])
            .push_number(get_slot.len() as u64)
            .append_many([PUSH0, PUSH0])
            .push_address(ORACLE_CONTRACT_ADDRESS)
            .append_many([GAS, CALL, POP])
    };
    let halts = BytecodeBuilder::default().append(INVALID).build();

    for gas_limit in TIERS {
        let cases: [(&str, MemoryDatabase, Address, usize); 8] = [
            ("undetained", system_db().account_code(CONTRACT, queries()), Address::ZERO, 1),
            (
                "detained by its own read, a small forward and a STATICCALL",
                system_db().account_code(
                    CONTRACT,
                    query(
                        query(reads_timestamp(BytecodeBuilder::default()), CALL, Some(100), 0),
                        STATICCALL,
                        None,
                        2,
                    )
                    .stop()
                    .build(),
                ),
                Address::ZERO,
                2,
            ),
            (
                "detained by a child's read, after a child that halted",
                system_db()
                    .account_code(
                        CONTRACT,
                        query(
                            calls(
                                calls(BytecodeBuilder::default(), CONTRACT2, 1_000_000, 0),
                                CONTRACT3,
                                500_000,
                                0,
                            ),
                            CALL,
                            None,
                            0,
                        )
                        .stop()
                        .build(),
                    )
                    .account_code(
                        CONTRACT2,
                        reads_timestamp(BytecodeBuilder::default()).stop().build(),
                    )
                    .account_code(CONTRACT3, halts.clone()),
                Address::ZERO,
                1,
            ),
            (
                "a value-called callee",
                system_db()
                    .account_code(
                        CONTRACT,
                        calls(BytecodeBuilder::default(), CONTRACT2, 50_000, 1).stop().build(),
                    )
                    .account_balance(CONTRACT, U256::from(10))
                    .account_code(CONTRACT2, queries()),
                Address::ZERO,
                1,
            ),
            (
                "a value-called callee that read",
                system_db()
                    .account_code(
                        CONTRACT,
                        calls(BytecodeBuilder::default(), CONTRACT2, 50_000, 1).stop().build(),
                    )
                    .account_balance(CONTRACT, U256::from(10))
                    .account_code(
                        CONTRACT2,
                        query(reads_timestamp(BytecodeBuilder::default()), CALL, None, 0)
                            .stop()
                            .build(),
                    ),
                Address::ZERO,
                1,
            ),
            (
                "a value-called callee of a caller that read",
                system_db()
                    .account_code(
                        CONTRACT,
                        calls(reads_timestamp(BytecodeBuilder::default()), CONTRACT2, 50_000, 1)
                            .stop()
                            .build(),
                    )
                    .account_balance(CONTRACT, U256::from(10))
                    .account_code(CONTRACT2, queries()),
                Address::ZERO,
                1,
            ),
            ("a beneficiary sender", system_db().account_code(CONTRACT, queries()), CALLER, 1),
            (
                "after an Oracle read",
                system_db()
                    .account_code(CONTRACT, query(reads_oracle(), CALL, None, 0).stop().build()),
                Address::ZERO,
                1,
            ),
        ];
        for (case, db, beneficiary, queries) in cases {
            let pairs = answers_and_resumes(db, beneficiary, tx(CONTRACT, &[], gas_limit));
            assert_eq!(pairs.len(), queries, "{case}, at {gas_limit}");
            for (answer, spendable) in pairs {
                assert_eq!(answer, spendable, "{case}, at {gas_limit}");
            }
        }
    }
}

/// A caller that acts on the answer can spend it and no more: after a read, a loop that spends a
/// little less than the answer completes, and one that spends a little more is stopped at the
/// compute limit.
#[test]
fn test_a_caller_can_spend_the_answer_and_no_more() {
    // After the query, `answer / 26 + extra` rounds of a loop whose body costs 26 gas:
    // JUMPDEST 1, PUSH1 3, SWAP1 3, SUB 3, DUP1 3, PUSH1 3, JUMPI 10.
    for extra in [-4_i64, 4] {
        let code = query(reads_timestamp(BytecodeBuilder::default()), CALL, None, 0)
            .append_many([PUSH0, MLOAD])
            .push_number(26_u8)
            .append_many([SWAP1, DIV])
            .push_number(extra.unsigned_abs());
        let code = if extra < 0 { code.append_many([SWAP1, SUB]) } else { code.append(ADD) };
        let mut code = code.build_vec();
        let start = u8::try_from(code.len()).expect("the loop starts within a PUSH1");
        code.extend([JUMPDEST, PUSH1, 1, SWAP1, SUB, DUP1, PUSH1, start, JUMPI, STOP]);

        let ctx = MegaContext::new(
            system_db().account_code(CONTRACT, Bytes::from(code)),
            MegaSpecId::SATIN,
        )
        .with_block(block(Address::ZERO))
        .with_chain(zero_fee_l1_block_info())
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(100_000),
        );
        let outcome = MegaEvm::new(ctx)
            .execute_transaction(tx(CONTRACT, &[], BELOW))
            .expect("the transaction is valid");

        if extra < 0 {
            assert!(outcome.result.is_success(), "{:?}", outcome.result);
            assert_eq!(outcome.limit_exceeded, None);
        } else {
            assert!(
                matches!(
                    outcome.limit_exceeded,
                    Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, .. })
                ),
                "{:?}",
                outcome.limit_exceeded
            );
        }
    }
}
