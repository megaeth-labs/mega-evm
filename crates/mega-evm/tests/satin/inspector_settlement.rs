//! What a frame that kept nothing settles into its caller, whatever an inspector made of its
//! result.
//!
//! Two frames keep nothing in the journal: one that failed, whose checkpoint was reverted before
//! the inspector saw its result, and one the inspector answered in place of running, which never
//! started. An inspector may still hand the caller a success for either — Foundry's `expectRevert`
//! turns a failed call into a success, and a mocked call is answered without running — and the
//! caller sees the success, the output and the regular gas the inspector left. Nothing else of the
//! frame reaches it: no state gas or history gas for writes the journal does not hold, no refund,
//! no write record, and no upfront charge for an account that was never added.
//!
//! Every expectation here is the rule's: a caller that drops the flag cannot tell a rewritten
//! failure from the failure, and a frame answered without running adds no account and writes
//! nothing, so the transaction's state gas is zero and its history is its body's. Where a new
//! account is charged, its bucket is crowded (`m = 3`), so the charge that comes back is the one
//! that was made, priced at the account it was made for.

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    history_gas,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    transaction_body_bytes, untouched_call_gas, untouched_create_gas, LimitUsage, MegaContext,
    MegaEvm, MegaTransaction, MegaTransactionOutcome, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, POP, PUSH0, STOP},
    context_interface::cfg::GasId,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        InstructionResult, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

use crate::salt::{
    create_with, crowded_account, db, entry, minimal_envs, salt_context, tx_with_gas, value_call,
    SaltEnvs, CONTRACT, EMPTY,
};

/// A contract `CONTRACT` calls.
const CALLEE: Address = address!("0000000000000000000000000000000000c00010");
/// The BN254 pairing precompile, which an empty input with too little gas runs out of gas on.
const PAIRING: Address = address!("0000000000000000000000000000000000000008");
/// The BN254 addition precompile, which a point off the curve fails.
const ADDITION: Address = address!("0000000000000000000000000000000000000006");
/// The address an answered creation reports.
const ANSWERED: Address = address!("0000000000000000000000000000000000c000aa");

/// A gas limit below the execution cap, where state gas spills onto regular gas, and one above it,
/// where a reservoir pays it.
const GAS_LIMITS: [u64; 2] = [30_000_000, TX_GAS_LIMIT_CAP + 100_000_000];

/// Turns every failed call result into a success and leaves the rest of it as it is: the shape of
/// Foundry's `expectRevert`.
struct Revives;

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Revives {
    fn call_end(&mut self, _: &mut CTX, _: &CallInputs, outcome: &mut CallOutcome) {
        if !outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Return;
        }
    }
}

/// Answers every call to `target`, and every creation, itself with `result`, on the gas the frame
/// was forwarded, untouched: the frame never starts. An answered creation reports `address`.
struct Answers {
    target: Address,
    result: InstructionResult,
    address: Option<Address>,
}

impl Answers {
    const fn call(target: Address, result: InstructionResult) -> Self {
        Self { target, result, address: None }
    }

    const fn creation(result: InstructionResult, address: Option<Address>) -> Self {
        Self { target: Address::ZERO, result, address }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Answers {
    fn call(&mut self, _: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (inputs.target_address == self.target).then(|| {
            CallOutcome::new(
                InterpreterResult::new(self.result, Bytes::new(), untouched_call_gas(inputs)),
                inputs.return_memory_offset.clone(),
            )
        })
    }

    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        (self.target == Address::ZERO).then(|| {
            CreateOutcome::new(
                InterpreterResult::new(self.result, Bytes::new(), untouched_create_gas(inputs)),
                self.address,
            )
        })
    }
}

/// Runs `tx` over `db` reading `envs`, without an inspector.
fn plain(db: MemoryDatabase, envs: SaltEnvs, tx: MegaTransaction) -> MegaTransactionOutcome {
    MegaEvm::new(salt_context(db, envs)).execute_transaction(tx).expect("a valid transaction")
}

/// Runs `tx` over `db` reading `envs`, under `inspector`.
fn inspected<I>(
    db: MemoryDatabase,
    envs: SaltEnvs,
    tx: MegaTransaction,
    inspector: I,
) -> MegaTransactionOutcome
where
    I: Inspector<MegaContext<MemoryDatabase, SaltEnvs>, EthInterpreter>,
{
    MegaEvm::new(salt_context(db, envs))
        .with_inspector(inspector)
        .execute_transaction(tx)
        .expect("a valid transaction")
}

/// A call from `CALLER` to `to` carrying `value` and `data`, at `gas_limit`.
fn to(to: Address, value: u64, data: Bytes, gas_limit: u64) -> MegaTransaction {
    tx_with_gas(TxKind::Call(to), data, U256::from(value), gas_limit)
}

/// A transaction that keeps nothing but its body: no state gas, its body's history alone, and its
/// body's bytes alone on the data size.
fn assert_keeps_only_its_body(case: &str, tx: &MegaTransaction, outcome: &MegaTransactionOutcome) {
    let body = transaction_body_bytes(tx);
    assert_eq!(outcome.gas.state, 0, "{case}: no state gas");
    assert_eq!(outcome.gas.history, history_gas(body).unwrap(), "{case}: the body's history");
    assert_eq!(outcome.gas.history_bytes, body, "{case}: the body's bytes");
    assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 }, "{case}");
}

/// Whether `account` holds anything in `outcome`'s state.
fn holds_something(outcome: &MegaTransactionOutcome, account: Address) -> bool {
    outcome.state.get(&account).is_some_and(|account| !account.info.is_empty())
}

/// `CALLEE` fills a fresh slot, clears a slot it held (a refund), logs a word, then reverts;
/// `CONTRACT` calls it and drops the flag. Rewritten into a success, the call is the revert it was
/// to everything but the flag `CONTRACT` drops: the transaction is the one no inspector runs, on
/// every ledger. Kept, the same writes cost state gas, history and a refund — what the rewrite
/// would otherwise have merged.
#[test]
fn test_a_failed_call_rewritten_into_a_success_settles_as_the_failure() {
    let writes = || {
        BytecodeBuilder::default()
            .sstore(U256::from(1), U256::from(1))
            .sstore(U256::from(2), U256::ZERO)
            .log3_word()
    };
    let caller = BytecodeBuilder::default().call(CALLEE, U256::ZERO).append(POP).stop().build();
    let setup = |code: Bytes| {
        db(caller.clone()).account_code(CALLEE, code).account_storage(
            CALLEE,
            U256::from(2),
            U256::from(1),
        )
    };
    let reverting = setup(writes().revert().build());
    let stopping = setup(writes().stop().build());
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let kept = plain(stopping.clone(), minimal_envs(), tx.clone());
        assert!(kept.result.is_success(), "{:?}", kept.result);
        assert!(kept.gas.state > 0, "kept, the fresh slot costs state gas");
        assert_eq!(kept.usage.write_records, 2, "and both slots are records");

        let failed = plain(reverting.clone(), minimal_envs(), tx.clone());
        let rewritten = inspected(reverting.clone(), minimal_envs(), tx.clone(), Revives);
        assert!(failed.result.is_success(), "CONTRACT drops the flag: {:?}", failed.result);
        assert_keeps_only_its_body("the failed call", &tx, &failed);
        assert_eq!(rewritten.result, failed.result, "no log, and the same bill");
        assert_eq!(rewritten.gas, failed.gas, "gas limit {gas_limit}: every ledger");
        assert_eq!(rewritten.usage, failed.usage, "gas limit {gas_limit}: the counts");
        assert_eq!(rewritten.state, failed.state, "gas limit {gas_limit}: the state");
    }
}

/// `CONTRACT` sends a wei to the BN254 pairing precompile with no gas of its own: the stipend is
/// short of the pairing's price, so the call runs out of gas, and the account its value would add
/// — the upfront new-account charge its `CALL` made — goes with it. Rewritten into a success, the
/// call gives that charge back all the same: the transaction is the one no inspector runs. With the
/// gas to run, the same call adds the account and keeps the charge.
#[test]
fn test_a_failed_value_call_rewritten_into_a_success_gives_the_new_account_charge_back() {
    let caller = |gas: u64| {
        BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_number(1_u8)
            .push_address(PAIRING)
            .push_number(gas)
            .append(CALL)
            .append(POP)
            .stop()
            .build()
    };
    let envs = || crowded_account(minimal_envs(), PAIRING, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller(1_000_000)), envs(), tx.clone());
        assert!(holds_something(&ran, PAIRING), "the pairing ran and kept the wei");
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");

        let failed = plain(db(caller(0)), envs(), tx.clone());
        let rewritten = inspected(db(caller(0)), envs(), tx.clone(), Revives);
        assert_keeps_only_its_body("the failed call", &tx, &failed);
        assert!(!holds_something(&rewritten, PAIRING), "no wei moved");
        assert_eq!(rewritten.gas, failed.gas, "gas limit {gas_limit}: every ledger");
        assert_eq!(rewritten.usage, failed.usage, "gas limit {gas_limit}: the counts");
        assert_eq!(rewritten.state, failed.state, "gas limit {gas_limit}: the state");
    }
}

/// The transaction's own frame: a wei sent to the BN254 addition precompile with a point off the
/// curve fails, and takes back the account EIP-2780 charged its start for and the write record
/// charged before execution. Rewritten into a success, the transaction reports the success and
/// keeps only its body, as the failure does. With a point on the curve, the same transaction adds
/// the account and keeps both charges.
#[test]
fn test_the_transactions_own_failed_frame_rewritten_into_a_success_keeps_nothing() {
    let point_off_the_curve = {
        let mut input = [0_u8; 128];
        input[31] = 1;
        input[63] = 1;
        Bytes::from(input.to_vec())
    };
    let envs = || crowded_account(minimal_envs(), ADDITION, 3);
    for gas_limit in GAS_LIMITS {
        let valid = to(ADDITION, 1, Bytes::from(vec![0_u8; 128]), gas_limit);
        let ran = plain(db(Bytes::new()), envs(), valid.clone());
        assert!(ran.result.is_success(), "{:?}", ran.result);
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");
        let body = transaction_body_bytes(&valid);
        assert_eq!(ran.gas.history_bytes, body + WRITE_RECORD_SIZE, "the body and the record");

        let tx = to(ADDITION, 1, point_off_the_curve.clone(), gas_limit);
        let failed = plain(db(Bytes::new()), envs(), tx.clone());
        assert!(failed.result.is_halt(), "{:?}", failed.result);
        assert_keeps_only_its_body("the failed transaction", &tx, &failed);

        let rewritten = inspected(db(Bytes::new()), envs(), tx.clone(), Revives);
        assert!(rewritten.result.is_success(), "the inspector's: {:?}", rewritten.result);
        assert_keeps_only_its_body("the rewritten transaction", &tx, &rewritten);
        assert!(!holds_something(&rewritten, ADDITION), "no wei moved");
        assert_eq!(rewritten.state, failed.state, "gas limit {gas_limit}: the state");
    }
}

/// A value call to an account that does not exist, answered by an inspector with a success, a
/// revert or a halt: the frame never started, so no wei moved and no account was added, and the
/// new-account charge its `CALL` made comes back whatever the answer. The three answers cost the
/// same, and the transaction keeps only its body. Run, the call adds the account in its bucket.
#[test]
fn test_a_value_call_an_inspector_answers_gives_its_new_account_charge_back() {
    let caller = || value_call(EMPTY).append(POP).stop().build();
    let envs = || crowded_account(minimal_envs(), EMPTY, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller()), envs(), tx.clone());
        assert!(holds_something(&ran, EMPTY), "the call added the account");
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");

        let answers =
            [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas].map(
                |result| {
                    let answered =
                        inspected(db(caller()), envs(), tx.clone(), Answers::call(EMPTY, result));
                    let case = format!("answered with {result:?}, gas limit {gas_limit}");
                    assert!(answered.result.is_success(), "{case}: {:?}", answered.result);
                    assert_keeps_only_its_body(&case, &tx, &answered);
                    assert!(!holds_something(&answered, EMPTY), "{case}: no account");
                    (answered.gas, answered.state.clone())
                },
            );
        assert_eq!(answers[0], answers[1], "gas limit {gas_limit}: a success costs a revert's");
    }
}

/// The transaction's own value transfer to an account that does not exist, answered by an
/// inspector: the account EIP-2780 charged the frame's start for is not added, and neither is the
/// write record charged before execution, whatever the answer.
#[test]
fn test_the_transactions_own_value_transfer_an_inspector_answers_keeps_nothing() {
    let envs = || crowded_account(minimal_envs(), EMPTY, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(EMPTY, 1, Bytes::new(), gas_limit);
        let ran = plain(db(Bytes::new()), envs(), tx.clone());
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");

        for result in [InstructionResult::Stop, InstructionResult::Revert] {
            let case = format!("answered with {result:?}, gas limit {gas_limit}");
            let answered =
                inspected(db(Bytes::new()), envs(), tx.clone(), Answers::call(EMPTY, result));
            assert_eq!(answered.result.is_success(), result.is_ok(), "{case}: the answer");
            assert_keeps_only_its_body(&case, &tx, &answered);
            assert!(!holds_something(&answered, EMPTY), "{case}: no account");
        }
    }
}

/// A creation an inspector answers adds no account, whatever the answer — a success with an
/// address, a success with none, a revert — so the creation's upfront charge comes back once, and
/// the three answers cost the same. Run, the creation adds its account in its bucket.
#[test]
fn test_a_creation_an_inspector_answers_gives_its_charge_back() {
    let caller = || create_with(&[STOP]).append(POP).stop().build();
    let created = CONTRACT.create(0);
    let envs = || crowded_account(minimal_envs(), created, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller()), envs(), tx.clone());
        assert!(ran.result.is_success(), "{:?}", ran.result);
        assert_eq!(ran.gas.state, 3 * entry(GasId::create_state_gas()), "in its bucket");

        let answers = [
            (InstructionResult::Return, Some(ANSWERED)),
            (InstructionResult::Stop, None),
            (InstructionResult::Revert, None),
        ]
        .map(|(result, address)| {
            let answered =
                inspected(db(caller()), envs(), tx.clone(), Answers::creation(result, address));
            let case = format!("answered with {result:?}, gas limit {gas_limit}");
            assert!(answered.result.is_success(), "{case}: {:?}", answered.result);
            assert_keeps_only_its_body(&case, &tx, &answered);
            assert!(!holds_something(&answered, created), "{case}: no account");
            answered.gas
        });
        assert_eq!(answers[0], answers[1], "gas limit {gas_limit}: with an address or without");
        assert_eq!(answers[0], answers[2], "gas limit {gas_limit}: a success costs a revert's");
    }
}

/// A value call an inspector answers leaves its caller holding none of the state gas its `CALL`
/// was charged for the account: the charge came back, so it never stands against the state-gas
/// limit. The caller then fills a slot, and a limit one gas short of the slot and the account
/// together stops the caller whose call ran, and not the one whose call was answered.
#[test]
fn test_an_answered_call_holds_nothing_against_the_state_gas_limit() {
    let caller =
        || value_call(EMPTY).append(POP).sstore(U256::from(1), U256::from(1)).stop().build();
    let slot = entry(GasId::sstore_set_state_gas());
    let limit = slot + entry(GasId::new_account_state_gas()) - 1;
    let limits = mega_evm::EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(limit);
    let tx = to(CONTRACT, 0, Bytes::new(), GAS_LIMITS[0]);
    let context = || salt_context(db(caller()), minimal_envs()).with_tx_runtime_limits(limits);

    let ran = MegaEvm::new(context()).execute_transaction(tx.clone()).unwrap();
    assert!(ran.limit_exceeded.is_some(), "the account and the slot cross: {:?}", ran.result);

    let answered = MegaEvm::new(context())
        .with_inspector(Answers::call(EMPTY, InstructionResult::Stop))
        .execute_transaction(tx)
        .unwrap();
    assert_eq!(answered.limit_exceeded, None, "{:?}", answered.result);
    assert!(answered.result.is_success(), "{:?}", answered.result);
    assert_eq!(answered.gas.state, slot, "the slot alone");
}
