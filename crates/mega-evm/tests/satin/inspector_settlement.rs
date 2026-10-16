//! What a frame that kept nothing settles into its caller, whatever an inspector made of its
//! result, and what a success an inspector rewrites into a failure keeps: nothing, the journal
//! following the result as it follows a frame that fails.
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
//! nothing, so the transaction's state gas is zero and its history is its body's. An answer
//! carries nothing but the regular gas the inspector chose, so every answer is given twice — on
//! the gas the frame was forwarded, untouched, and on `Gas::new` of it, as Foundry answers, which
//! carries no reservoir — and the two settle alike. Where a new
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
    bytecode::opcode::{CALL, POP, PUSH0, RETURN, REVERT, STOP},
    context::{ContextTr, JournalTr},
    context_interface::{cfg::GasId, journaled_state::account::JournaledAccountTr, Transaction},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome, Gas,
        InstructionResult, Interpreter, InterpreterResult, InterpreterTypes,
    },
    state::{AccountStatus, EvmStorageSlot},
    Inspector,
};

use crate::{
    common::state_is_free,
    salt::{
        create_with, crowded_account, db, entry, minimal_envs, salt_context, tx_with_gas,
        value_call, SaltEnvs, CALLER, CONTRACT, EMPTY,
    },
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
/// was forwarded, untouched, or, when `fresh`, on `Gas::new` of it: the frame never starts. An
/// answered creation reports `address`.
#[derive(Clone, Copy, Debug)]
struct Answers {
    target: Address,
    result: InstructionResult,
    address: Option<Address>,
    fresh: bool,
}

impl Answers {
    const fn call(target: Address, result: InstructionResult) -> Self {
        Self { target, result, address: None, fresh: false }
    }

    const fn creation(result: InstructionResult, address: Option<Address>) -> Self {
        Self { target: Address::ZERO, result, address, fresh: false }
    }

    /// The same answers, built on `Gas::new` of the gas the frame was forwarded, as Foundry
    /// answers a cheatcode: gas that carries no reservoir.
    const fn on_fresh_gas(self) -> Self {
        Self { fresh: true, ..self }
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Answers {
    fn call(&mut self, _: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (inputs.target_address == self.target).then(|| {
            let gas =
                if self.fresh { Gas::new(inputs.gas_limit) } else { untouched_call_gas(inputs) };
            CallOutcome::new(
                InterpreterResult::new(self.result, Bytes::new(), gas),
                inputs.return_memory_offset.clone(),
            )
        })
    }

    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        (self.target == Address::ZERO).then(|| {
            let gas = if self.fresh {
                Gas::new(inputs.gas_limit())
            } else {
                untouched_create_gas(inputs)
            };
            CreateOutcome::new(InterpreterResult::new(self.result, Bytes::new(), gas), self.address)
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

/// Runs `tx` over `db()` reading `envs()` under `answers`, on the gas the answered frame was
/// forwarded, untouched, and again on `Gas::new` of it; hands back the first once the second
/// settled alike. The frame never ran, so its answer carries nothing but the regular gas the
/// inspector chose, and its caller merges back the reservoir it forwarded whatever the answer was
/// built on.
fn answered(
    db: impl Fn() -> MemoryDatabase,
    envs: impl Fn() -> SaltEnvs,
    tx: &MegaTransaction,
    answers: Answers,
) -> MegaTransactionOutcome {
    let untouched = inspected(db(), envs(), tx.clone(), answers);
    let fresh = inspected(db(), envs(), tx.clone(), answers.on_fresh_gas());
    assert_eq!(fresh.result, untouched.result, "{answers:?}: on fresh gas");
    assert_eq!(fresh.gas, untouched.gas, "{answers:?}: every ledger, on fresh gas");
    assert_eq!(fresh.usage, untouched.usage, "{answers:?}: every count, on fresh gas");
    assert_eq!(fresh.state, untouched.state, "{answers:?}: the state, on fresh gas");
    untouched
}

/// A call from `CALLER` to `to` carrying `value` and `data`, at `gas_limit`.
fn to(to: Address, value: u64, data: Bytes, gas_limit: u64) -> MegaTransaction {
    tx_with_gas(TxKind::Call(to), data, U256::from(value), gas_limit)
}

/// A transaction that keeps nothing but its body: no state gas, its body's history alone, and its
/// body's bytes alone on the data size. Above the execution cap the reservoir paid for the body and
/// nothing else, so what is left of it comes back.
fn assert_keeps_only_its_body(case: &str, tx: &MegaTransaction, outcome: &MegaTransactionOutcome) {
    let body = transaction_body_bytes(tx);
    assert_eq!(outcome.gas.state, 0, "{case}: no state gas");
    assert_eq!(outcome.gas.history, history_gas(body).unwrap(), "{case}: the body's history");
    assert_eq!(
        outcome.gas.reservoir_remaining,
        tx.gas_limit().saturating_sub(TX_GAS_LIMIT_CAP).saturating_sub(outcome.gas.history),
        "{case}: the reservoir less the body's history"
    );
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
        assert_eq!(
            kept.gas.state,
            entry(GasId::sstore_set_state_gas()),
            "kept, the fresh slot costs state gas"
        );
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
                        answered(|| db(caller()), envs, &tx, Answers::call(EMPTY, result));
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
            let answered = answered(|| db(Bytes::new()), envs, &tx, Answers::call(EMPTY, result));
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
            let answered = answered(|| db(caller()), envs, &tx, Answers::creation(result, address));
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

/// The transaction's own creation, answered by an inspector: the account EIP-2780 charged the
/// frame's start for is not added, and neither is the write record charged before execution,
/// whatever the answer. Run, the creation adds its account in its bucket.
#[test]
fn test_the_transactions_own_creation_an_inspector_answers_keeps_nothing() {
    let created = CALLER.create(0);
    let envs = || crowded_account(minimal_envs(), created, 3);
    for gas_limit in GAS_LIMITS {
        let tx = tx_with_gas(TxKind::Create, Bytes::from_static(&[STOP]), U256::ZERO, gas_limit);
        let ran = plain(db(Bytes::new()), envs(), tx.clone());
        assert!(ran.result.is_success(), "{:?}", ran.result);
        assert_eq!(ran.gas.state, 3 * entry(GasId::create_state_gas()), "in its bucket");

        for (result, address) in [
            (InstructionResult::Return, Some(ANSWERED)),
            (InstructionResult::Stop, None),
            (InstructionResult::Revert, None),
        ] {
            let case = format!("answered with {result:?}, gas limit {gas_limit}");
            let answered =
                answered(|| db(Bytes::new()), envs, &tx, Answers::creation(result, address));
            assert_eq!(answered.result.is_success(), result.is_ok(), "{case}: the answer");
            assert_keeps_only_its_body(&case, &tx, &answered);
            assert!(!holds_something(&answered, created), "{case}: no account");
        }
    }
}

/// A value call an inspector answers leaves its caller holding none of the state gas its `CALL`
/// was charged for the account: the charge came back, so it never stands against the state-gas
/// limit. The caller then fills a slot, and a limit one gas short of the slot and the account
/// together stops the caller whose call ran, and not the one whose call was answered.
#[test]
fn test_an_answered_call_holds_nothing_against_the_state_gas_limit() {
    // A slot and an account that add no state gas have no state-gas limit to cross.
    if state_is_free() {
        return;
    }
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

/* ---------- a success rewritten into a failure ---------- */

/// The data Foundry's `expectRevert` gives a call that did not revert.
const DID_NOT_REVERT: &[u8] = b"call did not revert as expected";

/// Turns every successful result of a call to `target`, and of every creation when `target` is
/// zero, into `result` carrying Foundry's "did not revert" data — the shape of `expectRevert` on a
/// call that did not revert — or, with no `result`, rewrites nothing. Records the journal's depth
/// at every instruction, which Foundry's `expectRevert` compares.
#[derive(Clone)]
struct Fails {
    target: Address,
    result: Option<InstructionResult>,
    depths: Vec<usize>,
}

impl Fails {
    const fn call(target: Address, result: InstructionResult) -> Self {
        Self { target, result: Some(result), depths: Vec::new() }
    }

    const fn creation(result: InstructionResult) -> Self {
        Self::call(Address::ZERO, result)
    }

    /// The same inspector, rewriting nothing.
    fn recording(&self) -> Self {
        Self { result: None, ..self.clone() }
    }

    fn fail(&self, result: &mut InterpreterResult) {
        if let Some(failure) = self.result.filter(|_| result.result.is_ok()) {
            result.result = failure;
            result.output = Bytes::from_static(DID_NOT_REVERT);
        }
    }
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for Fails {
    fn step(&mut self, _: &mut Interpreter<INTR>, context: &mut CTX) {
        self.depths.push(context.journal_ref().depth());
    }

    fn call_end(&mut self, _: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == self.target {
            self.fail(&mut outcome.result);
        }
    }

    fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, outcome: &mut CreateOutcome) {
        if self.target == Address::ZERO {
            self.fail(&mut outcome.result);
        }
    }
}

/// Runs `tx` over `db()` reading `envs()` under `fails`, and again under the same inspector
/// rewriting nothing; hands back the first once the journal's depth was the same at every
/// instruction of both: taking a frame's journal back leaves the depth as it was.
fn rewritten(
    db: impl Fn() -> MemoryDatabase,
    envs: impl Fn() -> SaltEnvs,
    tx: &MegaTransaction,
    fails: Fails,
) -> MegaTransactionOutcome {
    let run = |inspector: Fails| {
        let mut evm = MegaEvm::new(salt_context(db(), envs())).with_inspector(inspector);
        let outcome = evm.execute_transaction(tx.clone()).expect("a valid transaction");
        (outcome, evm.inspector().depths.clone())
    };
    let (_, recorded) = run(fails.recording());
    let (outcome, depths) = run(fails);
    assert_eq!(depths, recorded, "the journal's depth at every instruction");
    outcome
}

/// The failures a success is rewritten into: Foundry's revert, and a halt.
const FAILURES: [InstructionResult; 2] = [InstructionResult::Revert, InstructionResult::OutOfGas];

/// An account as a state is compared on without its code: status, balance, nonce and storage.
type AccountBesideCode = (Address, AccountStatus, U256, u64, Vec<(U256, EvmStorageSlot)>);

/// `outcome`'s state without the code of its accounts, in address order: what two runs whose
/// programs differ in their last opcodes are compared on.
fn state_without_code(outcome: &MegaTransactionOutcome) -> Vec<AccountBesideCode> {
    let mut accounts: Vec<_> = outcome
        .state
        .iter()
        .map(|(address, account)| {
            let mut storage: Vec<_> =
                account.storage.iter().map(|(key, slot)| (*key, slot.clone())).collect();
            storage.sort_by_key(|(key, _)| *key);
            (*address, account.status, account.info.balance, account.info.nonce, storage)
        })
        .collect();
    accounts.sort_by_key(|(address, ..)| *address);
    accounts
}

/// A value call that adds an account, rewritten into a failure, keeps nothing. `EMPTY` has no code,
/// so revm answers the call without building a frame, and what its start journaled — the value's
/// move, the account it touched, the transfer log — is taken back to where the start began: the
/// value stays with its sender and no account is added. The failure then settles as the same
/// failure an inspector answers in place of the call does, on every ledger and in the state: no
/// state gas, the body's history alone. At the transaction's own frame and one call down, below
/// and above the execution cap; run, the call adds the account and logs the transfer.
#[test]
fn test_a_value_call_rewritten_into_a_failure_keeps_nothing() {
    let envs = || crowded_account(minimal_envs(), EMPTY, 3);
    let caller = value_call(EMPTY).append(POP).stop().build();
    for gas_limit in GAS_LIMITS {
        let cases = [
            ("the transaction's own", Bytes::new(), to(EMPTY, 1, Bytes::new(), gas_limit)),
            ("one call down", caller.clone(), to(CONTRACT, 0, Bytes::new(), gas_limit)),
        ];
        for (at, code, tx) in cases {
            let db = || db(code.clone());
            let ran = plain(db(), envs(), tx.clone());
            assert!(holds_something(&ran, EMPTY), "{at}: run, the call adds the account");
            assert_eq!(ran.result.logs().len(), 1, "{at}: and logs the transfer");
            for result in FAILURES {
                let case = format!("{at}, rewritten into {result:?}, gas limit {gas_limit}");
                let rewritten = rewritten(db, envs, &tx, Fails::call(EMPTY, result));
                let answered = inspected(db(), envs(), tx.clone(), Answers::call(EMPTY, result));
                assert_keeps_only_its_body(&case, &tx, &rewritten);
                assert!(!holds_something(&rewritten, EMPTY), "{case}: no account");
                assert!(rewritten.result.logs().is_empty(), "{case}: no transfer log");
                assert_eq!(rewritten.gas, answered.gas, "{case}: every ledger");
                assert_eq!(rewritten.usage, answered.usage, "{case}: every count");
                assert_eq!(rewritten.state, answered.state, "{case}: the state");
            }
        }
    }
}

/// Where a creation case runs one init code: the code of `CONTRACT` and the transaction.
type CreationSetup<'a> = &'a dyn Fn(&[u8]) -> (Bytes, MegaTransaction);

/// A creation rewritten into a failure keeps what a creation that fails keeps: its creator's nonce
/// bump, which revm makes before the creation's checkpoint, and its record — nothing after the
/// checkpoint: no account, no code, no endowment. It settles as a creation whose init code reverts
/// in place of returning does, on the state beside the code, state and history gas and the counts;
/// a revert keeps the regular gas the run spent. At the transaction's own frame and one call down,
/// below and above the execution cap; run, the creation adds its account.
#[test]
fn test_a_creation_rewritten_into_a_failure_keeps_what_a_failed_creation_keeps() {
    let (deploys, reverts) = (&[PUSH0, PUSH0, RETURN][..], &[PUSH0, PUSH0, REVERT][..]);
    let creates = |init: &[u8]| {
        BytecodeBuilder::default().create(U256::from(1), init).append(POP).stop().build()
    };
    for gas_limit in GAS_LIMITS {
        let own = |init: &[u8]| {
            let data = Bytes::copy_from_slice(init);
            (Bytes::new(), tx_with_gas(TxKind::Create, data, U256::from(1), gas_limit))
        };
        let down = |init: &[u8]| (creates(init), to(CONTRACT, 0, Bytes::new(), gas_limit));
        let cases: [(&str, Address, CreationSetup<'_>); 2] = [
            ("the transaction's own", CALLER.create(0), &own),
            ("one call down", CONTRACT.create(0), &down),
        ];
        for (at, created, setup) in cases {
            let envs = || crowded_account(minimal_envs(), created, 3);
            let ((deploying, tx), (reverting, reverting_tx)) = (setup(deploys), setup(reverts));
            let ran = plain(db(deploying.clone()), envs(), tx.clone());
            assert!(holds_something(&ran, created), "{at}: run, the creation adds its account");
            let failed = plain(db(reverting), envs(), reverting_tx);
            assert!(!holds_something(&failed, created), "{at}: reverting, it adds none");
            for result in FAILURES {
                let case = format!("{at}, rewritten into {result:?}, gas limit {gas_limit}");
                let rewritten =
                    rewritten(|| db(deploying.clone()), envs, &tx, Fails::creation(result));
                assert!(!holds_something(&rewritten, created), "{case}: no account");
                let state = state_without_code(&rewritten);
                assert_eq!(state, state_without_code(&failed), "{case}: the state");
                assert_eq!(rewritten.usage, failed.usage, "{case}: every count");
                assert_eq!(rewritten.gas.state, failed.gas.state, "{case}: state gas");
                assert_eq!(rewritten.gas.history, failed.gas.history, "{case}: history gas");
                assert_eq!(rewritten.gas.history_bytes, failed.gas.history_bytes, "{case}");
                if result == InstructionResult::Revert {
                    assert_eq!(rewritten.gas.regular, ran.gas.regular, "{case}: regular gas");
                }
            }
        }
    }
}

/// Where a call case runs one program: the database holding it.
type CallSetup<'a> = &'a dyn Fn(&Bytes) -> MemoryDatabase;

/// A call whose frame ran and wrote — a fresh slot filled and a word logged — rewritten into a
/// failure keeps none of it: the frame's journal is taken back to its checkpoint, so the slot is
/// not in the state and the log is not in the receipt. It settles as the same writes reverting do,
/// on the state beside the code, state and history gas and the counts; a revert keeps the regular
/// gas the run spent. At the transaction's own frame and one call down, below and above the
/// execution cap; run, the slot and the log are kept.
#[test]
fn test_a_call_that_wrote_rewritten_into_a_failure_keeps_none_of_it() {
    let writes = || BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).log3_word();
    let (stopping, reverting) = (writes().stop().build(), writes().revert().build());
    let caller = BytecodeBuilder::default().call(CALLEE, U256::ZERO).append(POP).stop().build();
    let own = |code: &Bytes| db(code.clone());
    let down = |code: &Bytes| db(caller.clone()).account_code(CALLEE, code.clone());
    let cases: [(&str, Address, CallSetup<'_>); 2] =
        [("the transaction's own", CONTRACT, &own), ("one call down", CALLEE, &down)];
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        for (at, writer, setup) in cases {
            let slot = |outcome: &MegaTransactionOutcome| {
                outcome.state[&writer].storage.get(&U256::from(1)).is_some_and(|s| s.is_changed())
            };
            let ran = plain(setup(&stopping), minimal_envs(), tx.clone());
            assert!(slot(&ran) && !ran.result.logs().is_empty(), "{at}: run, the writes are kept");
            let failed = plain(setup(&reverting), minimal_envs(), tx.clone());
            for result in FAILURES {
                let case = format!("{at}, rewritten into {result:?}, gas limit {gas_limit}");
                let fails = Fails::call(writer, result);
                let rewritten = rewritten(|| setup(&stopping), minimal_envs, &tx, fails);
                assert!(!slot(&rewritten), "{case}: no slot");
                assert!(rewritten.result.logs().is_empty(), "{case}: no log");
                let state = state_without_code(&rewritten);
                assert_eq!(state, state_without_code(&failed), "{case}: the state");
                assert_eq!(rewritten.usage, failed.usage, "{case}: every count");
                assert_eq!(rewritten.gas.state, failed.gas.state, "{case}: state gas");
                assert_eq!(rewritten.gas.history, failed.gas.history, "{case}: history gas");
                if result == InstructionResult::Revert {
                    assert_eq!(rewritten.gas.regular, ran.gas.regular, "{case}: regular gas");
                }
            }
        }
    }
}

/// An account the inspector of [`WritesAtTheEnd`] writes to.
const NOTE: Address = address!("0000000000000000000000000000000000c000bb");

/// Writes a balance to `NOTE` in the `call_end` of every call to `CALLEE`, and, when `fails`, turns
/// a success into a revert there.
struct WritesAtTheEnd {
    fails: bool,
}

impl Inspector<MegaContext<MemoryDatabase, SaltEnvs>, EthInterpreter> for WritesAtTheEnd {
    fn call_end(
        &mut self,
        context: &mut MegaContext<MemoryDatabase, SaltEnvs>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address != CALLEE {
            return;
        }
        let mut account = context.journal_mut().load_account_mut(NOTE).expect("loads");
        account.data.set_balance(U256::from(7));
        if self.fails && outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Revert;
        }
    }
}

/// What an inspector writes to the journal in the callback that turns a success into a failure is
/// journaled after the frame's checkpoint, and goes with the frame's writes; written in a callback
/// that leaves the success alone, it stays. Where revm fails the frame itself, the checkpoint is
/// gone before `call_end`, so a write made there stays: the one place the rewrite and a real
/// failure part.
#[test]
fn test_a_write_an_inspector_makes_as_it_fails_a_frame_goes_with_the_frame() {
    let caller = BytecodeBuilder::default().call(CALLEE, U256::ZERO).append(POP).stop().build();
    let db = |callee: Bytes| db(caller.clone()).account_code(CALLEE, callee);
    let stopping = BytecodeBuilder::default().stop().build();
    let reverting = BytecodeBuilder::default().revert().build();
    let tx = to(CONTRACT, 0, Bytes::new(), GAS_LIMITS[0]);
    let run = |callee: &Bytes, fails: bool| {
        inspected(db(callee.clone()), minimal_envs(), tx.clone(), WritesAtTheEnd { fails })
    };
    assert!(holds_something(&run(&stopping, false), NOTE), "left a success, the write stays");
    assert!(!holds_something(&run(&stopping, true), NOTE), "failing it, the write goes");
    assert!(holds_something(&run(&reverting, false), NOTE), "revm failed it: the write stays");
}
