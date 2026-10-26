//! What a frame's result an inspector rewrites settles into: the result as given, as revm takes it.
//!
//! revm commits or reverts a frame's journal checkpoint before the inspector sees the frame's
//! result, and the result the inspector hands back is the one its caller settles. So a failed frame
//! an inspector revives into a success — Foundry's `expectRevert` — keeps its journal reverted and
//! settles as a success: its state gas, history, refund and the records it counted merge into its
//! caller. A success it rewrites into a failure keeps its journal and settles as a failure: its
//! charges are rolled back, its counts restored and its start charge refunded. A frame it answers
//! in place of running never started: the charges its caller's opcode made for it stay charged,
//! since the answer carries no upfront-charge flag and no start charge for revm to give back, and
//! it counts nothing of its own.
//!
//! Every expectation is read off a twin no inspector touches: the same frame succeeding, or
//! failing, or the call running. Where a new account is charged, its bucket is crowded (`m = 3`),
//! so the charge is the one that was made, priced at the account it was made for.

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    history_gas,
    op_revm::OpHaltReason,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    transaction_body_bytes, untouched_call_gas, untouched_create_gas, EvmTxRuntimeLimits,
    LimitUsage, MegaContext, MegaEvm, MegaTransaction, MegaTransactionOutcome,
};
use revm::{
    bytecode::{
        opcode::{CALL, POP, PUSH0, RETURN, REVERT, STOP},
        Bytecode,
    },
    context::{result::ExecutionResult, ContextTr, JournalTr},
    context_interface::{cfg::GasId, journaled_state::account::JournaledAccountTr, Transaction},
    handler::SYSTEM_ADDRESS,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CallValue, CreateInputs,
        CreateOutcome, InstructionResult, Interpreter, InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

#[cfg(debug_assertions)]
use crate::common::{answered_on_its_shares, first_frame_pools, frame_shares};
use crate::salt::{
    create_with, crowded_account, db, entry, minimal_envs, salt_context, tx_with_gas, value_call,
    SaltEnvs, CALLER, CONTRACT, EMPTY,
};
#[cfg(debug_assertions)]
use mega_evm::test_utils::{take_start_ledger_divergences, tolerate_start_ledger_divergences};

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

/// Answers every call to `target`, and every creation when `target` is zero, itself with `result`,
/// on the gas the frame was forwarded, untouched: the frame never starts. An answered creation
/// reports `address`.
#[derive(Clone, Copy, Debug)]
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
            let gas = untouched_create_gas(inputs);
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

/// [`inspected`] under `inspector`, which answers start `start`, at `depth`, on the gas its input
/// carries, the shares of its caller's pools included: an answer with pools the inspector set, an
/// API removal of the inspector table. A debug build's start ledger, which settles an inspector's
/// answer at unlimited pools, records where it parts from the fork, and each part is asserted
/// ([`answered_on_its_shares`]): the caller is the transaction's own frame, or the transaction,
/// which holds its first frame's pools when the start is made.
fn answered<I>(
    db: MemoryDatabase,
    envs: SaltEnvs,
    tx: MegaTransaction,
    inspector: I,
    (start, depth): (u32, usize),
) -> MegaTransactionOutcome
where
    I: Inspector<MegaContext<MemoryDatabase, SaltEnvs>, EthInterpreter>,
{
    let mut evm = MegaEvm::new(salt_context(db, envs)).with_inspector(inspector);
    #[cfg(debug_assertions)]
    tolerate_start_ledger_divergences(evm.ctx_mut());
    let outcome = evm.execute_transaction(tx.clone()).expect("a valid transaction");
    #[cfg(debug_assertions)]
    {
        let caller = first_frame_pools(&EvmTxRuntimeLimits::default(), &tx);
        let shares = if depth == 0 { caller } else { frame_shares(caller) };
        assert_eq!(
            take_start_ledger_divergences(evm.ctx_mut()),
            answered_on_its_shares(start, depth, caller, shares),
            "start {start} at depth {depth}"
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = (start, depth);
    outcome
}

/// A call from `CALLER` to `to` carrying `value` and `data`, at `gas_limit`.
fn to(to: Address, value: u64, data: Bytes, gas_limit: u64) -> MegaTransaction {
    tx_with_gas(TxKind::Call(to), data, U256::from(value), gas_limit)
}

/// What `tx` counts before any frame: its body's bytes, and no write record.
fn body_usage(tx: &MegaTransaction) -> LimitUsage {
    LimitUsage { data_size: transaction_body_bytes(tx), write_records: 0, state_growth: 0 }
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
    assert_eq!(outcome.usage, body_usage(tx), "{case}");
}

/// Requires `outcome` to bill the state gas and the history `charged` bills: the charges the run
/// made and kept, which a result an inspector rewrote kept too.
fn assert_bills_as(case: &str, outcome: &MegaTransactionOutcome, charged: &MegaTransactionOutcome) {
    assert_eq!(outcome.gas.state, charged.gas.state, "{case}: the state gas");
    assert_eq!(outcome.gas.history, charged.gas.history, "{case}: the history gas");
}

/// Whether `account` holds anything in `outcome`'s state.
fn holds_something(outcome: &MegaTransactionOutcome, account: Address) -> bool {
    outcome.state.get(&account).is_some_and(|account| !account.info.is_empty())
}

/* ---------- a failure revived into a success ---------- */

/// `CALLEE` fills a fresh slot, clears a slot it held (a refund), logs a word, then reverts;
/// `CONTRACT` calls it and drops the flag. Revived into a success, the call keeps the state of the
/// revert — its journal was reverted before the inspector saw it, so no slot and no log — and
/// settles as the success: its caller merges the state gas, the history, the refund and the
/// records of the same writes kept, on every ledger.
#[test]
fn test_a_failed_call_revived_into_a_success_settles_as_a_success() {
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
    // Returning nothing costs what reverting with nothing does, so the two spend the same and the
    // refund's cap is the same for both.
    let stopping = setup(writes().return_empty().build());
    for gas_limit in GAS_LIMITS {
        let case = format!("gas limit {gas_limit}");
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let kept = plain(stopping.clone(), minimal_envs(), tx.clone());
        assert!(kept.result.is_success(), "{:?}", kept.result);
        assert_eq!(kept.gas.state, entry(GasId::sstore_set_state_gas()), "the fresh slot");
        assert_eq!(kept.usage.write_records, 2, "and both slots are records");
        let failed = plain(reverting.clone(), minimal_envs(), tx.clone());
        assert_keeps_only_its_body("the failed call", &tx, &failed);

        let revived = inspected(reverting.clone(), minimal_envs(), tx.clone(), Revives);
        assert!(revived.result.is_success(), "{case}: {:?}", revived.result);
        assert!(revived.result.logs().is_empty(), "{case}: the log went with the journal");
        assert_eq!(revived.state, failed.state, "{case}: the state is the revert's");
        assert_bills_as(&case, &revived, &kept);
        assert_eq!(revived.gas.history_bytes, kept.gas.history_bytes, "{case}");
        assert_eq!(revived.usage, kept.usage, "{case}: the counts are the success's");
        assert_eq!(
            revived.result.gas().inner_refunded(),
            kept.result.gas().inner_refunded(),
            "{case}: and so is the refund"
        );
        assert_eq!(revived.gas, kept.gas, "{case}: every ledger is the success's");
    }
}

/// `CONTRACT` sends a wei to the BN254 pairing precompile with no gas of its own: the stipend is
/// short of the pairing's price, so the call runs out of gas, and the account its value would add —
/// the upfront new-account charge its `CALL` made — and its start's records go with it. Revived
/// into a success, no wei moved, and the call keeps both charges, as the call that runs keeps them:
/// a success gives back neither. revm decided the start as a failure, which counted nothing, so the
/// transaction counts its body alone.
#[test]
fn test_a_failed_value_call_revived_into_a_success_keeps_its_charges() {
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
        let case = format!("gas limit {gas_limit}");
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller(1_000_000)), envs(), tx.clone());
        assert!(holds_something(&ran, PAIRING), "the pairing ran and kept the wei");
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");
        let failed = plain(db(caller(0)), envs(), tx.clone());
        assert_keeps_only_its_body("the failed call", &tx, &failed);

        let revived = inspected(db(caller(0)), envs(), tx.clone(), Revives);
        assert!(revived.result.is_success(), "{case}: {:?}", revived.result);
        assert!(!holds_something(&revived, PAIRING), "{case}: no wei moved");
        assert_bills_as(&case, &revived, &ran);
        assert_eq!(revived.usage, body_usage(&tx), "{case}: nothing counted");
    }
}

/// The transaction's own frame: a wei sent to the BN254 addition precompile with a point off the
/// curve fails, and takes back the account EIP-2780 charged its start for and the write record
/// charged before execution. Revived into a success, no wei moved, and the transaction keeps both
/// charges, as the transaction with a point on the curve keeps them; its start counted nothing.
#[test]
fn test_the_transactions_own_failed_frame_revived_into_a_success_keeps_its_charges() {
    let point_off_the_curve = {
        let mut input = [0_u8; 128];
        input[31] = 1;
        input[63] = 1;
        Bytes::from(input.to_vec())
    };
    let envs = || crowded_account(minimal_envs(), ADDITION, 3);
    for gas_limit in GAS_LIMITS {
        let case = format!("gas limit {gas_limit}");
        let valid = to(ADDITION, 1, Bytes::from(vec![0_u8; 128]), gas_limit);
        let ran = plain(db(Bytes::new()), envs(), valid);
        assert!(ran.result.is_success(), "{:?}", ran.result);
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");

        let tx = to(ADDITION, 1, point_off_the_curve.clone(), gas_limit);
        let failed = plain(db(Bytes::new()), envs(), tx.clone());
        assert!(failed.result.is_halt(), "{:?}", failed.result);
        assert_keeps_only_its_body("the failed transaction", &tx, &failed);

        let revived = inspected(db(Bytes::new()), envs(), tx.clone(), Revives);
        assert!(revived.result.is_success(), "{case}: the inspector's: {:?}", revived.result);
        assert!(!holds_something(&revived, ADDITION), "{case}: no wei moved");
        assert_eq!(revived.state, failed.state, "{case}: the state is the failure's");
        assert_bills_as(&case, &revived, &ran);
        assert_eq!(revived.usage, body_usage(&tx), "{case}: nothing counted");
    }
}

/// Turns every failed creation result into a success at `0x…01`, as Foundry's `expectRevert` on a
/// creation does: the revert's output and gas stay as they are, and the address is one the
/// creation never made.
struct RevivesCreations;

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for RevivesCreations {
    fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, outcome: &mut CreateOutcome) {
        if !outcome.result.result.is_ok() {
            outcome.result.result = InstructionResult::Return;
            outcome.address = Some(Address::with_last_byte(1));
        }
    }
}

/// Where a creation case runs one init code: the code of `CONTRACT` and the transaction.
type CreationSetup<'a> = &'a dyn Fn(&[u8]) -> (Bytes, MegaTransaction);

/// A creation whose init code fills a fresh slot, logs a word and reverts, revived into a success,
/// keeps the state of the revert — no account, no slot, no log — and settles as the creation that
/// returns no code does: the upfront charge for the account, priced at its bucket (`m = 3`), the
/// slot, the records and the log's bytes, all kept. At the transaction's own frame and one call
/// down, carrying no value and one wei, below and above the execution cap.
#[test]
fn test_a_failed_creation_revived_into_a_success_settles_as_a_success() {
    let writes = || BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).log3_word();
    // Returning no code, as long as reverting, so the transaction's own creation carries a body of
    // the same size either way.
    let stopping = writes().append_many([PUSH0, PUSH0, RETURN]).build();
    let reverting = writes().revert().build();
    for (gas_limit, value) in GAS_LIMITS.into_iter().flat_map(|limit| [(limit, 0), (limit, 1)]) {
        let creates = |init: &[u8]| {
            BytecodeBuilder::default().create(U256::from(value), init).append(POP).stop().build()
        };
        let own = |init: &[u8]| {
            let data = Bytes::copy_from_slice(init);
            (Bytes::new(), tx_with_gas(TxKind::Create, data, U256::from(value), gas_limit))
        };
        let down = |init: &[u8]| (creates(init), to(CONTRACT, 0, Bytes::new(), gas_limit));
        let cases: [(&str, Address, CreationSetup<'_>); 2] = [
            ("the transaction's own", CALLER.create(0), &own),
            ("one call down", CONTRACT.create(0), &down),
        ];
        for (at, created, setup) in cases {
            let envs = || crowded_account(minimal_envs(), created, 3);
            let case = format!("{at}, value {value}, gas limit {gas_limit}");
            let (code, tx) = setup(&stopping[..]);
            let ran = plain(db(code), envs(), tx);
            assert!(holds_something(&ran, created), "{case}: run, the creation adds its account");
            assert_eq!(
                ran.gas.state,
                3 * entry(GasId::create_state_gas()) + entry(GasId::sstore_set_state_gas()),
                "{case}: the account in its bucket and the slot"
            );

            let (code, tx) = setup(&reverting[..]);
            let failed = plain(db(code.clone()), envs(), tx.clone());
            assert!(!holds_something(&failed, created), "{case}: reverting, it adds none");
            let revived = inspected(db(code), envs(), tx.clone(), RevivesCreations);
            assert!(revived.result.is_success(), "{case}: {:?}", revived.result);
            assert!(!holds_something(&revived, created), "{case}: no account");
            assert!(revived.result.logs().is_empty(), "{case}: no log");
            assert_bills_as(&case, &revived, &ran);
            assert_eq!(revived.usage, ran.usage, "{case}: the counts are the success's");
        }
    }
}

/* ---------- a frame an inspector answers ---------- */

/// A value call to an account that does not exist, answered by an inspector with a success, a
/// revert or a halt: the frame never started, so no wei moved and no account was added, and nothing
/// of the start is counted. The charges the `CALL` made for it — the account, at its bucket, and
/// the history of the records its start would make — stay charged, as for the call that runs: the
/// answer carries no flag and no start charge for revm to give back.
#[test]
fn test_a_value_call_an_inspector_answers_keeps_what_its_opcode_was_charged() {
    let caller = || value_call(EMPTY).append(POP).stop().build();
    let envs = || crowded_account(minimal_envs(), EMPTY, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller()), envs(), tx.clone());
        assert!(holds_something(&ran, EMPTY), "the call added the account");
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");

        for result in
            [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas]
        {
            let case = format!("answered with {result:?}, gas limit {gas_limit}");
            let answers = Answers::call(EMPTY, result);
            let answered = answered(db(caller()), envs(), tx.clone(), answers, (2, 1));
            assert!(answered.result.is_success(), "{case}: {:?}", answered.result);
            assert!(!holds_something(&answered, EMPTY), "{case}: no account");
            assert_bills_as(&case, &answered, &ran);
            assert_eq!(answered.usage, body_usage(&tx), "{case}: nothing counted");
        }
    }
}

/// The transaction's own value transfer to an account that does not exist, answered by an
/// inspector: the account is not added and nothing is counted, and what the transaction was charged
/// before its frame ran — the account EIP-2780 charged its start for, the write record — stays
/// charged, as for the transfer that runs.
#[test]
fn test_the_transactions_own_value_transfer_an_inspector_answers_keeps_its_charges() {
    let envs = || crowded_account(minimal_envs(), EMPTY, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(EMPTY, 1, Bytes::new(), gas_limit);
        let ran = plain(db(Bytes::new()), envs(), tx.clone());
        assert_eq!(ran.gas.state, 3 * entry(GasId::new_account_state_gas()), "in its bucket");

        for result in [InstructionResult::Stop, InstructionResult::Revert] {
            let case = format!("answered with {result:?}, gas limit {gas_limit}");
            let answers = Answers::call(EMPTY, result);
            let answered = answered(db(Bytes::new()), envs(), tx.clone(), answers, (1, 0));
            assert_eq!(answered.result.is_success(), result.is_ok(), "{case}: the answer");
            assert!(!holds_something(&answered, EMPTY), "{case}: no account");
            assert_bills_as(&case, &answered, &ran);
            assert_eq!(answered.usage, body_usage(&tx), "{case}: nothing counted");
        }
    }
}

/// A creation an inspector answers adds no account and bumps no nonce, whatever the answer — a
/// success with an address, a success with none, a revert — and the charges its `CREATE` made stay
/// charged, as for the creation that runs: the account at its bucket, the history of its records.
/// It counts no record: the creator's nonce was not bumped, so no record of it stands. The three
/// answers cost the same.
#[test]
fn test_a_creation_an_inspector_answers_keeps_what_its_opcode_was_charged() {
    let caller = || create_with(&[STOP]).append(POP).stop().build();
    let created = CONTRACT.create(0);
    let envs = || crowded_account(minimal_envs(), created, 3);
    for gas_limit in GAS_LIMITS {
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller()), envs(), tx.clone());
        assert!(ran.result.is_success(), "{:?}", ran.result);
        assert_eq!(ran.gas.state, 3 * entry(GasId::create_state_gas()), "in its bucket");
        let nonce = ran.state[&CONTRACT].info.nonce;

        let answers = [
            (InstructionResult::Return, Some(ANSWERED)),
            (InstructionResult::Stop, None),
            (InstructionResult::Revert, None),
        ]
        .map(|(result, address)| {
            let case = format!("answered with {result:?}, gas limit {gas_limit}");
            let answers = Answers::creation(result, address);
            let answered = answered(db(caller()), envs(), tx.clone(), answers, (2, 1));
            assert!(answered.result.is_success(), "{case}: {:?}", answered.result);
            assert!(!holds_something(&answered, created), "{case}: no account");
            assert_eq!(answered.state[&CONTRACT].info.nonce, nonce - 1, "{case}: no bump");
            assert_bills_as(&case, &answered, &ran);
            assert_eq!(answered.usage.write_records, 0, "{case}: no bump, no record");
            answered.gas
        });
        assert_eq!(answers[0], answers[1], "gas limit {gas_limit}: with an address or without");
        assert_eq!(answers[0], answers[2], "gas limit {gas_limit}: a success costs a revert's");
    }
}

/// The transaction's own creation, answered by an inspector: the account is not added and nothing
/// is counted, and what the transaction was charged before its frame ran — the account EIP-2780
/// charged its start for, the write record — stays charged, as for the creation that runs.
#[test]
fn test_the_transactions_own_creation_an_inspector_answers_keeps_its_charges() {
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
            let answers = Answers::creation(result, address);
            let answered = answered(db(Bytes::new()), envs(), tx.clone(), answers, (1, 0));
            assert_eq!(answered.result.is_success(), result.is_ok(), "{case}: the answer");
            assert!(!holds_something(&answered, created), "{case}: no account");
            assert_bills_as(&case, &answered, &ran);
            assert_eq!(answered.usage, body_usage(&tx), "{case}: nothing counted");
        }
    }
}

/* ---------- a call an inspector retargets ---------- */

/// The address `Retargets` sends a call to.
const RETARGET: Address = address!("0000000000000000000000000000000000c00020");

/// Rewrites every call to `from` into a call to `RETARGET`, whose code it supplies, in the
/// inspector's `call` hook, and answers it with `answer` when it has one: a tool that redirects a
/// call after the opcode charged for the account it was made to.
struct Retargets {
    from: Address,
    code: Bytecode,
    answer: Option<InstructionResult>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for Retargets {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address != self.from {
            return None;
        }
        // The new target is loaded before the frame starts, as a hook that redirects a call must.
        context.journal_mut().load_account(RETARGET).expect("in memory");
        inputs.target_address = RETARGET;
        inputs.bytecode_address = RETARGET;
        inputs.known_bytecode = (self.code.hash_slow(), self.code.clone());
        self.answer.map(|result| {
            CallOutcome::new(
                InterpreterResult::new(result, Bytes::new(), untouched_call_gas(inputs)),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

/// A value call to an account that does not exist charges that account's state gas upfront, in its
/// SALT bucket. An inspector that redirects the call to another account rewrites the inputs revm
/// reads the charge's account off when the frame fails: the call that runs at the new target and
/// reverts gives the charge back at the new target's price. Where the account is dearer (`m` = 8
/// and 1) the transaction keeps the difference as state gas; where it is cheaper (1 and 8) more
/// comes back than was charged, which revm credits to the reservoir, so the state ledger reads
/// nothing and the sender gets the excess back with the reservoir. The call answered in place of
/// running gives back nothing, as every answer does.
#[test]
fn test_a_retargeted_value_calls_upfront_charge_comes_back_at_the_new_targets_price() {
    let caller = || value_call(EMPTY).append(POP).stop().build();
    let reverts = Bytecode::new_raw(Bytes::from_static(&[PUSH0, PUSH0, REVERT]));
    let account = entry(GasId::new_account_state_gas());
    for (account_m, retarget_m) in [(8, 1), (1, 8)] {
        let envs = || {
            crowded_account(crowded_account(minimal_envs(), EMPTY, account_m), RETARGET, retarget_m)
        };
        let db = || db(caller()).account_code(RETARGET, reverts.original_bytes());
        for gas_limit in GAS_LIMITS {
            let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
            let ran = plain(db(), envs(), tx.clone());
            assert_eq!(
                ran.gas.state,
                account_m * account,
                "{account_m}/{retarget_m}: run, the call adds the account in its bucket"
            );
            let case = format!("m {account_m}/{retarget_m}, gas limit {gas_limit}");
            let answer = Some(InstructionResult::Stop);
            let inspector = Retargets { from: EMPTY, code: reverts.clone(), answer };
            let answered = answered(db(), envs(), tx.clone(), inspector, (2, 1));
            assert!(answered.result.is_success(), "{case}: {:?}", answered.result);
            assert_eq!(answered.gas.state, account_m * account, "{case}: answered, kept whole");

            let inspector = Retargets { from: EMPTY, code: reverts.clone(), answer: None };
            let reverted = inspected(db(), envs(), tx.clone(), inspector);
            assert!(reverted.result.is_success(), "{case}: {:?}", reverted.result);
            assert!(!holds_something(&reverted, EMPTY), "{case}: no account");
            let body = history_gas(transaction_body_bytes(&tx)).unwrap();
            assert_eq!(reverted.gas.history, body, "{case}: the start's records given back");
            let kept = i128::from(account_m * account) - i128::from(retarget_m * account);
            let reservoir =
                i128::from(gas_limit.saturating_sub(TX_GAS_LIMIT_CAP).saturating_sub(body));
            let reservoir = if reservoir == 0 { (-kept).max(0) } else { reservoir - kept };
            assert_eq!(
                i128::from(reverted.gas.state),
                kept.max(0),
                "{case}: reverting, given back at the new target's price"
            );
            assert_eq!(i128::from(reverted.gas.reservoir_remaining), reservoir, "{case}");
        }
    }
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
/// instruction of both: a rewrite leaves the depth as it was.
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

/// A value call that adds an account, rewritten into a failure, settles as a failure and keeps the
/// state of its success. `EMPTY` has no code, so revm answers the call without building a frame and
/// commits what its start journaled — the value's move, the account it adds, the transfer log —
/// before the inspector sees the result: the wei stays with `EMPTY`. The failure gives back the
/// upfront charge for the account, the history of the start's records and the records themselves,
/// so the transaction bills its body alone. At the transaction's own frame and one call down,
/// below and above the execution cap.
#[test]
fn test_a_value_call_rewritten_into_a_failure_keeps_its_state_and_bills_its_body() {
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
            for result in FAILURES {
                let case = format!("{at}, rewritten into {result:?}, gas limit {gas_limit}");
                let rewritten = rewritten(db, envs, &tx, Fails::call(EMPTY, result));
                assert!(holds_something(&rewritten, EMPTY), "{case}: the wei moved");
                assert_eq!(
                    rewritten.state[&EMPTY].info, ran.state[&EMPTY].info,
                    "{case}: the account the success added"
                );
                assert_keeps_only_its_body(&case, &tx, &rewritten);
            }
        }
    }
}

/// A creation rewritten into a failure keeps the state of its success — its account, its code and
/// its endowment — and settles as the creation whose init code reverts in place of returning does:
/// its creator's nonce record, which outlives the creation, and nothing of the creation's own — no
/// state gas, no history and no record of the account. At the transaction's own frame and one call
/// down, below and above the execution cap.
#[test]
fn test_a_creation_rewritten_into_a_failure_keeps_its_state_and_settles_as_a_failure() {
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
                assert_eq!(
                    rewritten.state[&created].info.balance, ran.state[&created].info.balance,
                    "{case}: the account and its endowment"
                );
                assert_eq!(rewritten.usage, failed.usage, "{case}: every count");
                assert_bills_as(&case, &rewritten, &failed);
                assert_eq!(rewritten.gas.history_bytes, failed.gas.history_bytes, "{case}");
            }
        }
    }
}

/// Where a call case runs one program: the database holding it.
type CallSetup<'a> = &'a dyn Fn(&Bytes) -> MemoryDatabase;

/// A call whose frame ran and wrote — a fresh slot filled and a word logged — rewritten into a
/// failure keeps its writes: revm committed the frame's checkpoint before the inspector saw the
/// result, so the slot is in the state, and one call down the log is in the receipt. It settles as
/// the same writes reverting do: their state gas, history and records given back. At the
/// transaction's own frame and one call down, below and above the execution cap.
#[test]
fn test_a_call_that_wrote_rewritten_into_a_failure_keeps_its_writes_and_settles_as_a_failure() {
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
            let failed = plain(setup(&reverting), minimal_envs(), tx.clone());
            assert!(!slot(&failed), "{at}: reverting, the slot is not kept");
            for result in FAILURES {
                let case = format!("{at}, rewritten into {result:?}, gas limit {gas_limit}");
                let fails = Fails::call(writer, result);
                let rewritten = rewritten(|| setup(&stopping), minimal_envs, &tx, fails);
                assert!(slot(&rewritten), "{case}: the slot stays");
                if writer == CALLEE {
                    assert_eq!(rewritten.result.logs().len(), 1, "{case}: and the log");
                }
                assert_eq!(rewritten.usage, failed.usage, "{case}: every count");
                assert_bills_as(&case, &rewritten, &failed);
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

/// What an inspector writes to the journal in `call_end` comes after the frame's checkpoint was
/// committed or reverted, so it stays whatever the frame's result says: left a success, turned into
/// a failure, or failed by revm itself.
#[test]
fn test_a_write_an_inspector_makes_at_a_frames_end_stays() {
    let caller = BytecodeBuilder::default().call(CALLEE, U256::ZERO).append(POP).stop().build();
    let db = |callee: Bytes| db(caller.clone()).account_code(CALLEE, callee);
    let stopping = BytecodeBuilder::default().stop().build();
    let reverting = BytecodeBuilder::default().revert().build();
    let tx = to(CONTRACT, 0, Bytes::new(), GAS_LIMITS[0]);
    let run = |callee: &Bytes, fails: bool| {
        inspected(db(callee.clone()), minimal_envs(), tx.clone(), WritesAtTheEnd { fails })
    };
    assert!(holds_something(&run(&stopping, false), NOTE), "left a success, the write stays");
    assert!(holds_something(&run(&stopping, true), NOTE), "failing it, the write stays");
    assert!(holds_something(&run(&reverting, false), NOTE), "revm failed it: the write stays");
}

/// Rewrites the creator of every creation to `creator`: Foundry's `vm.prank` before `new C()`.
struct Pranks {
    creator: Address,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Pranks {
    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        inputs.set_call(self.creator);
        None
    }
}

/// A creation whose creator an inspector rewrites to an account the transaction already changed
/// (the sender, whose validation changed it) runs, and counts the records of what revm did: the
/// account it created, and no record of the rewritten creator, whose bump changed an account
/// changed already. The opcode's creator was never bumped and has no record either; what its
/// opcode was charged for one stays charged.
#[test]
fn test_a_creation_whose_creator_an_inspector_rewrites_to_a_changed_account_runs() {
    let caller = || create_with(&[STOP]).append(POP).stop().build();
    let tx = to(CONTRACT, 0, Bytes::new(), 30_000_000);
    let outcome = inspected(db(caller()), minimal_envs(), tx, Pranks { creator: CALLER });
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 1, "the created account alone");
}

/// A creation whose creator an inspector rewrites to an account the transaction has not changed
/// counts that account's nonce record, the record of the account revm bumped, and, the account
/// being empty, one account of state growth for the account the bump made, with the created
/// account's record and growth.
#[test]
fn test_a_creation_whose_creator_an_inspector_rewrites_to_an_empty_account_counts_its_record() {
    let caller = || create_with(&[STOP]).append(POP).stop().build();
    let tx = to(CONTRACT, 0, Bytes::new(), 30_000_000);
    let outcome = inspected(db(caller()), minimal_envs(), tx, Pranks { creator: EMPTY });
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.state[&EMPTY].info.nonce, 1, "revm bumped the rewritten creator");
    assert_eq!(outcome.usage.write_records, 2, "its nonce record and the created account's");
    assert_eq!(outcome.usage.state_growth, 2, "its account, which the bump made, and the created");
}

/// A contract `CONTRACT` calls to send a wei to `CALLEE`, which changes `CALLEE` and leaves
/// `CONTRACT` unchanged.
const SENDER: Address = address!("0000000000000000000000000000000000c00011");

/// A rewrite of a frame's start, made in the inspector's `call` or `create` after the start's hook
/// charged for the records the start would make.
#[derive(Clone, Copy, Debug)]
enum Rewrite {
    /// A zero-value call to `CALLEE` carries a wei: the start moves value between two accounts
    /// it left unchanged, two records.
    Value,
    /// A creation's creator becomes `CALLEE`, which `SENDER` changed: the creator's nonce bump
    /// makes no record of its own.
    Creator,
}

impl Rewrite {
    /// `CONTRACT`'s code, which makes the start the rewrite is for.
    fn code(self) -> Bytes {
        match self {
            Self::Value => BytecodeBuilder::default().call(CALLEE, U256::ZERO).append(POP),
            Self::Creator => BytecodeBuilder::default()
                .call(SENDER, U256::ZERO)
                .append(POP)
                .append_many(create_with(&[STOP]).build_vec())
                .append(POP),
        }
        .stop()
        .build()
    }

    /// The state the rewritten run reads: `CONTRACT` and `SENDER` funded, `SENDER` sending a wei.
    fn db(self) -> MemoryDatabase {
        db(self.code())
            .account_balance(SENDER, U256::from(10u64.pow(9)))
            .account_code(SENDER, value_call(CALLEE).append(POP).stop().build())
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Rewrite {
    fn call(&mut self, _: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if matches!(self, Self::Value) && inputs.target_address == CALLEE {
            inputs.value = CallValue::Transfer(U256::from(1));
        }
        None
    }

    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if matches!(self, Self::Creator) {
            inputs.set_call(CALLEE);
        }
        None
    }
}

/// Every way a tool runs an inspector: alloy-evm's `transact` with the inspector enabled, and
/// revm's `InspectEvm`, `InspectCommitEvm` and `InspectSystemCallEvm` called directly, with
/// alloy-evm's switch off.
#[derive(Clone, Copy, Debug)]
enum Entry {
    AlloyTransact,
    InspectOneTx,
    InspectTx,
    InspectOne,
    Inspect,
    InspectTxCommit,
    InspectCommit,
    InspectOneSystemCall,
    InspectOneSystemCallWithCaller,
    InspectSystemCall,
    InspectSystemCallWithCaller,
    InspectOneSystemCallWithInspector,
    InspectSystemCallWithInspector,
    InspectOneSystemCallWithInspectorAndCaller,
    InspectSystemCallWithInspectorAndCaller,
}

impl Entry {
    const ALL: [Self; 15] = [
        Self::AlloyTransact,
        Self::InspectOneTx,
        Self::InspectTx,
        Self::InspectOne,
        Self::Inspect,
        Self::InspectTxCommit,
        Self::InspectCommit,
        Self::InspectOneSystemCall,
        Self::InspectOneSystemCallWithCaller,
        Self::InspectSystemCall,
        Self::InspectSystemCallWithCaller,
        Self::InspectOneSystemCallWithInspector,
        Self::InspectSystemCallWithInspector,
        Self::InspectOneSystemCallWithInspectorAndCaller,
        Self::InspectSystemCallWithInspectorAndCaller,
    ];

    /// Runs a zero-value call to `CONTRACT` under `rewrite` through this entry point: a transaction
    /// from `CALLER`, or a system call.
    fn run(self, rewrite: Rewrite) -> ExecutionResult<OpHaltReason> {
        use mega_evm::alloy_evm::Evm as _;
        use revm::{InspectCommitEvm, InspectEvm, InspectSystemCallEvm};
        let mut evm =
            MegaEvm::new(salt_context(rewrite.db(), minimal_envs())).with_inspector(rewrite);
        if !matches!(self, Self::AlloyTransact) {
            evm.set_inspector_enabled(false);
        }
        let tx = to(CONTRACT, 0, Bytes::new(), 30_000_000);
        let (caller, data) = (SYSTEM_ADDRESS, Bytes::new());
        let result = match self {
            Self::AlloyTransact => {
                return evm.execute_transaction(tx).expect("valid").result_and_state.result
            }
            Self::InspectOneTx => evm.inspect_one_tx(tx),
            Self::InspectTx => evm.inspect_tx(tx).map(|done| done.result),
            Self::InspectOne => evm.inspect_one(tx, rewrite),
            Self::Inspect => evm.inspect(tx, rewrite).map(|done| done.result),
            Self::InspectTxCommit => evm.inspect_tx_commit(tx),
            Self::InspectCommit => evm.inspect_commit(tx, rewrite),
            Self::InspectOneSystemCall => evm.inspect_one_system_call(CONTRACT, data),
            Self::InspectOneSystemCallWithCaller => {
                evm.inspect_one_system_call_with_caller(caller, CONTRACT, data)
            }
            Self::InspectSystemCall => evm.inspect_system_call(CONTRACT, data).map(|r| r.result),
            Self::InspectSystemCallWithCaller => {
                evm.inspect_system_call_with_caller(caller, CONTRACT, data).map(|r| r.result)
            }
            Self::InspectOneSystemCallWithInspector => {
                evm.inspect_one_system_call_with_inspector(CONTRACT, data, rewrite)
            }
            Self::InspectSystemCallWithInspector => {
                evm.inspect_system_call_with_inspector(CONTRACT, data, rewrite).map(|r| r.result)
            }
            Self::InspectOneSystemCallWithInspectorAndCaller => evm
                .inspect_one_system_call_with_inspector_and_caller(caller, CONTRACT, data, rewrite),
            Self::InspectSystemCallWithInspectorAndCaller => evm
                .inspect_system_call_with_inspector_and_caller(caller, CONTRACT, data, rewrite)
                .map(|r| r.result),
        };
        result.expect("valid")
    }
}

/// An inspector may rewrite a frame's start after the start's hook charged for its records — a
/// call's value, a creation's creator — whichever entry point runs it: the run settles the start
/// revm made.
#[test]
fn test_every_inspected_entry_point_runs_a_start_an_inspector_rewrote() {
    for entry in Entry::ALL {
        for rewrite in [Rewrite::Value, Rewrite::Creator] {
            let result = entry.run(rewrite);
            assert!(result.is_success(), "{entry:?}, {rewrite:?}: {result:?}");
        }
    }
}

/// The records a rewritten start counts are those revm made. A zero-value call rewritten to carry
/// a wei moves it between `CONTRACT` and `CALLEE`, two accounts the transaction left unchanged
/// until then: two records and the transfer log. A creation whose creator is rewritten to `CALLEE`,
/// which `SENDER`'s wei changed, bumps an account changed already, which makes no record: the
/// transaction keeps `SENDER`'s transfer, two records and its log, and the created account.
#[test]
fn test_a_rewritten_start_counts_the_records_revm_made() {
    let tx = to(CONTRACT, 0, Bytes::new(), 30_000_000);
    for (rewrite, records, logs) in [(Rewrite::Value, 2, 1), (Rewrite::Creator, 3, 1)] {
        let outcome = inspected(rewrite.db(), minimal_envs(), tx.clone(), rewrite);
        assert!(outcome.result.is_success(), "{rewrite:?}: {:?}", outcome.result);
        assert_eq!(outcome.usage.write_records, records, "{rewrite:?}");
        assert_eq!(outcome.result.logs().len(), logs, "{rewrite:?}: the transfer log");
    }
}

/// Raises the endowment of every creation to `value`.
struct Endows {
    value: U256,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Endows {
    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        inputs.set_value(self.value);
        None
    }
}

/// A creation revm refuses on its creator's account — an inspector raised the endowment past the
/// creator's balance — bumps no nonce and makes no frame, so it counts no record: the transaction
/// keeps its body alone, at either gas limit. What the `CREATE` was charged at its opcode stays
/// charged, as for a start an inspector answers: the history of the created account's record,
/// which the start declared, comes back with the refusal, and that of the creator's record, which
/// it did not declare, stays.
#[test]
fn test_a_creation_refused_on_its_creators_account_counts_no_record() {
    let caller = || create_with(&[STOP]).append(POP).stop().build();
    for gas_limit in GAS_LIMITS {
        let case = format!("gas limit {gas_limit}");
        let tx = to(CONTRACT, 0, Bytes::new(), gas_limit);
        let ran = plain(db(caller()), minimal_envs(), tx.clone());
        let nonce = ran.state[&CONTRACT].info.nonce;
        let refused =
            inspected(db(caller()), minimal_envs(), tx.clone(), Endows { value: U256::MAX });
        assert!(refused.result.is_success(), "{case}: {:?}", refused.result);
        assert_eq!(refused.state[&CONTRACT].info.nonce, nonce - 1, "{case}: no bump");
        assert_keeps_only_its_body_and_the_opcodes_charge(&case, &tx, &refused);
    }
}

/// [`assert_keeps_only_its_body`], for a creation an inspector made revm refuse: the history of
/// the record of its creator's nonce, which its `CREATE` was charged and its start did not declare,
/// is on the history ledger beyond the body's bytes.
fn assert_keeps_only_its_body_and_the_opcodes_charge(
    case: &str,
    tx: &MegaTransaction,
    outcome: &MegaTransactionOutcome,
) {
    let body = transaction_body_bytes(tx);
    let charged = history_gas(body + mega_evm::WRITE_RECORD_SIZE).unwrap();
    assert_eq!(outcome.gas.state, 0, "{case}: no state gas");
    assert_eq!(outcome.gas.history, charged, "{case}: the body's history and the opcode's charge");
    assert_eq!(
        outcome.gas.reservoir_remaining,
        tx.gas_limit().saturating_sub(TX_GAS_LIMIT_CAP).saturating_sub(charged),
        "{case}: the reservoir less what the history ledger bills"
    );
    assert_eq!(outcome.gas.history_bytes, body, "{case}: the body's bytes");
    assert_eq!(outcome.usage, body_usage(tx), "{case}");
}

/// A refused creation draws nothing on its creator's pool: the creator writes one fresh slot after
/// it, and that write's record fits a data-size limit of the body and 40 bytes, and no tighter
/// one.
#[test]
fn test_a_refused_creation_leaves_its_creators_pool_whole() {
    let code = || create_with(&[STOP]).append(POP).sstore(U256::ZERO, U256::from(1)).stop().build();
    let tx = to(CONTRACT, 0, Bytes::new(), 30_000_000);
    let run = |limit: u64| {
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
        MegaEvm::new(salt_context(db(code()), minimal_envs()).with_tx_runtime_limits(limits))
            .with_inspector(Endows { value: U256::MAX })
            .execute_transaction(tx.clone())
            .expect("a valid transaction")
    };
    let fits = transaction_body_bytes(&tx) + 40;
    let kept = run(fits);
    assert!(kept.result.is_success(), "{:?}", kept.result);
    assert_eq!(kept.limit_exceeded, None, "the one slot's record fits");
    assert_eq!(kept.usage.write_records, 1, "the slot's record, and not the creator's");
    assert!(run(fits - 1).limit_exceeded.is_some(), "one byte less does not");
}

/// A creator with nothing left in its pool, whose creation revm refuses, is held to nothing: no
/// record was counted, and it resumes within its budget.
#[test]
fn test_a_refused_creation_holds_an_empty_pool_to_nothing() {
    let caller = || create_with(&[STOP]).append(POP).stop().build();
    let tx = to(CONTRACT, 0, Bytes::new(), 30_000_000);
    let limits =
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(transaction_body_bytes(&tx));
    let refused =
        MegaEvm::new(salt_context(db(caller()), minimal_envs()).with_tx_runtime_limits(limits))
            .with_inspector(Endows { value: U256::MAX })
            .execute_transaction(tx.clone())
            .expect("a valid transaction");
    assert!(refused.result.is_success(), "{:?}", refused.result);
    assert_eq!(refused.limit_exceeded, None, "no stop");
    assert_keeps_only_its_body_and_the_opcodes_charge("empty pool", &tx, &refused);
}
