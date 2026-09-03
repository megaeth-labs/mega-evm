//! Boundary coverage for the `EQUIVALENCE` side of the `MINI_REX` gates in the execution face.
//!
//! Three properties are pinned here:
//!
//! 1. `MegaHandler::before_run` promotes a transaction sent by the runtime system address into the
//!    OP deposit-style path — bypassing fee accounting — and rejects one whose callee is not
//!    whitelisted. Both behaviors start at `MINI_REX`; under `EQUIVALENCE` such a transaction is an
//!    ordinary user transaction that pays its own fees and may call anything.
//! 2. The whole `AdditionalLimit` subsystem is dormant before `MINI_REX`: no reset, no intrinsic
//!    accounting, and revm's stock instruction table, so every metered dimension stays at zero
//!    however much state a transaction touches.
//! 3. That dormancy reaches the frame settlement's inspector lanes too. An edit an inspector makes
//!    to a frame result's gas is booked at `AdditionalLimit::finalize_frame`, which does not run
//!    before `MINI_REX`, so under `EQUIVALENCE` the edit reaches the receipt with the ledger's
//!    result lane — and therefore the block guard — reading it as untouched. What the measurement
//!    shim books at its own callback boundaries is unaffected, because the shim is not spec-gated.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
    MegaTransactionOutcome, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{BlockEnv, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    interpreter::{CallInputs, CallOutcome, Interpreter},
    primitives::TxKind,
    Database as _, Inspector,
};

/// A callee that is deliberately absent from `MEGA_SYSTEM_TX_WHITELIST`.
const NON_WHITELISTED_CALLEE: Address = address!("0000000000000000000000000000000000DEAD01");
/// Externally-owned sender of the tracker-dormancy transaction.
const EOA_CALLER: Address = address!("0000000000000000000000000000000000100000");
/// Plain contract used as the target of the tracker-dormancy transaction.
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
/// Inner contract called by [`CALLEE`] so the transaction spans more than one frame.
const INNER: Address = address!("0000000000000000000000000000000000100002");
const BENEFICIARY: Address = address!("00000000000000000000000000000000000BEEF1");

const GAS_LIMIT: u64 = 1_000_000;
const INITIAL_BALANCE: u64 = 1_000_000_000;

fn build_evm<DB: revm::Database + core::fmt::Debug>(
    db: DB,
) -> MegaEvm<DB, NoOpInspector, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::EQUIVALENCE).with_block(BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 0,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    MegaEvm::new(context)
}

fn system_address_tx(callee: Address, gas_price: u128) -> MegaTransaction {
    let mut tx = MegaTransaction::new(TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(callee),
        gas_limit: GAS_LIMIT,
        gas_price,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Under `EQUIVALENCE` a transaction from the system address is never inspected against
/// `MEGA_SYSTEM_TX_WHITELIST`, so a non-whitelisted callee executes normally instead of failing
/// validation with the "callee is not in the whitelist" string error.
#[test]
fn test_equivalence_system_address_tx_skips_whitelist_rejection() {
    let db = MemoryDatabase::default()
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(INITIAL_BALANCE))
        .account_code(NON_WHITELISTED_CALLEE, BytecodeBuilder::default().stop().build());
    let mut evm = build_evm(db);

    let result =
        alloy_evm::Evm::transact_raw(&mut evm, system_address_tx(NON_WHITELISTED_CALLEE, 0))
            .expect("EQUIVALENCE must run a system-address tx as an ordinary transaction");
    assert!(result.result.is_success(), "expected ordinary execution, got {:?}", result.result);
}

/// The other half of the same gate: under `EQUIVALENCE` the system-address transaction is not
/// rewritten into a deposit (which forces `gas_price` to zero and degenerates fee accounting),
/// so the sender pays for its own gas even when the callee *is* whitelisted.
#[test]
fn test_equivalence_system_address_tx_still_pays_its_own_fees() {
    let mut db = MemoryDatabase::default()
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(INITIAL_BALANCE))
        .account_code(ORACLE_CONTRACT_ADDRESS, BytecodeBuilder::default().stop().build());
    {
        let mut evm = build_evm(&mut db);
        let result =
            alloy_evm::Evm::transact_raw(&mut evm, system_address_tx(ORACLE_CONTRACT_ADDRESS, 1))
                .expect("whitelisted callee must execute");
        assert!(result.result.is_success(), "got {:?}", result.result);
        assert!(result.result.tx_gas_used() > 0);
        let sender =
            result.state.get(&MEGA_SYSTEM_ADDRESS).expect("sender must appear in the state diff");
        assert!(
            sender.info.balance < U256::from(INITIAL_BALANCE),
            "EQUIVALENCE must charge the system address for its own gas; balance stayed at {}",
            sender.info.balance,
        );
    }
    // Guard the fixture itself: the account really did start with the full balance.
    let stored = db.basic(MEGA_SYSTEM_ADDRESS).expect("db read").expect("account exists");
    assert_eq!(stored.balance, U256::from(INITIAL_BALANCE));
}

/// Pre-`MINI_REX` the `AdditionalLimit` trackers are never reset, never charged the intrinsic
/// cost, and never fed by an instruction wrapper — revm's stock table runs instead. A transaction
/// that writes storage, emits a log, spans two frames, and deploys a contract must therefore leave
/// every metered dimension at zero.
#[test]
fn test_equivalence_leaves_additional_limit_dormant() {
    // INNER: write one storage slot and emit an empty LOG0.
    let inner_code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(2))
        .push_number(0u64)
        .push_number(0u64)
        .append(LOG0)
        .stop()
        .build();
    // CALLEE: write a slot, CALL INNER, then CREATE a one-byte contract (initcode returns
    // `RETURN` of a single zero byte from memory).
    let callee_code = BytecodeBuilder::default()
        .sstore(U256::from(3), U256::from(4))
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(INNER)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .push_number(1u64) // size
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .append(POP)
        .stop()
        .build();

    let db = MemoryDatabase::default()
        .account_balance(EOA_CALLER, U256::from(INITIAL_BALANCE))
        .account_balance(CALLEE, U256::from(INITIAL_BALANCE))
        .account_code(CALLEE, callee_code)
        .account_code(INNER, inner_code);
    let mut evm = build_evm(db);

    let mut tx = MegaTransaction::new(TxEnv {
        caller: EOA_CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: 10_000_000,
        gas_price: 0,
        data: Bytes::from(vec![0xAAu8; 64]),
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    assert!(result.result.is_success(), "got {:?}", result.result);

    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    assert_eq!(usage.compute_gas, 0, "pre-MINI_REX must not meter compute gas");
    assert_eq!(usage.data_size, 0, "pre-MINI_REX must not meter data size");
    assert_eq!(usage.kv_updates, 0, "pre-MINI_REX must not meter KV updates");
    assert_eq!(usage.state_growth, 0, "pre-MINI_REX must not meter state growth");
}

// --- the frame settlement's inspector lanes ---------------------------------------------------

/// Sender of the two-frame transaction the inspector tests below rewrite.
const CHEAT_CALLER: Address = address!("0000000000000000000000000000000000200000");
/// Outer contract: makes one inner call and stops.
const CHEAT_OUTER: Address = address!("0000000000000000000000000000000000200001");
/// Inner contract: the frame whose result the inspector rewrites.
const CHEAT_INNER: Address = address!("0000000000000000000000000000000000200002");

/// How much gas each inspector below writes back into the EVM.
const CHEAT_AMOUNT: u64 = 1_000;

/// Rewrites the gas of the inner frame's result at `call_end` — the last callback that can touch a
/// frame result, and the one whose edit `AdditionalLimit::finalize_frame` books.
#[derive(Default)]
struct ResultGasCheat {
    rewrites: u32,
}

impl<DB: revm::Database + core::fmt::Debug> Inspector<MegaContext<DB, EmptyExternalEnv>>
    for ResultGasCheat
{
    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB, EmptyExternalEnv>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == CHEAT_INNER {
            self.rewrites += 1;
            outcome.result.gas.erase_cost(CHEAT_AMOUNT);
        }
    }
}

/// Writes the same amount into the interpreter's own gas counter instead, at the first `step_end`.
///
/// The measurement shim books this one at its own callback boundary, with no help from the frame
/// settlement, which is what makes it the control for [`ResultGasCheat`].
#[derive(Default)]
struct CounterGasCheat {
    done: bool,
}

impl<DB: revm::Database + core::fmt::Debug> Inspector<MegaContext<DB, EmptyExternalEnv>>
    for CounterGasCheat
{
    fn step_end(
        &mut self,
        interp: &mut Interpreter,
        _context: &mut MegaContext<DB, EmptyExternalEnv>,
    ) {
        if !self.done {
            self.done = true;
            interp.gas.erase_cost(CHEAT_AMOUNT);
        }
    }
}

/// A database whose outer contract calls the inner one, so the transaction has a frame to settle
/// that is not the transaction's own.
fn cheat_db() -> MemoryDatabase {
    let inner = BytecodeBuilder::default().stop().build();
    let outer = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CHEAT_INNER)
        .push_number(100_000u64)
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    MemoryDatabase::default()
        .account_balance(CHEAT_CALLER, U256::from(INITIAL_BALANCE))
        .account_code(CHEAT_OUTER, outer)
        .account_code(CHEAT_INNER, inner)
}

fn cheat_tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(TxEnv {
        caller: CHEAT_CALLER,
        kind: TxKind::Call(CHEAT_OUTER),
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Runs [`cheat_tx`] over [`cheat_db`] under `spec`, optionally with an inspector attached.
fn transact_cheat<I>(spec: MegaSpecId, inspector: Option<&mut I>) -> MegaTransactionOutcome
where
    I: for<'a> Inspector<MegaContext<&'a mut MemoryDatabase, EmptyExternalEnv>>,
{
    let mut db = cheat_db();
    let mut context = MegaContext::new(&mut db, spec).with_block(BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 0,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let outcome = match inspector {
        Some(inspector) => {
            MegaEvm::new(context).with_inspector(inspector).execute_transaction(cheat_tx())
        }
        None => MegaEvm::new(context).execute_transaction(cheat_tx()),
    };
    outcome.expect("the cheat fixture must produce a receipt")
}

/// `AdditionalLimit::finalize_frame` is where an edit an inspector makes to a frame result's gas is
/// booked onto the ledger's result lane, and that settlement point starts at `MINI_REX` along with
/// the rest of the subsystem. Under `EQUIVALENCE` the rewrite still reaches the receipt — the
/// settlement is not what hands gas back to a caller — but nothing books it, so the lane the block
/// guard reads stays empty.
///
/// The `MINI_REX` half of the same fixture is the contrast that gives the assertion its meaning:
/// one spec later, the identical rewrite over the identical bytecode books its full amount.
#[test]
fn test_equivalence_does_not_book_a_frame_result_gas_rewrite() {
    for (spec, expected_lane) in
        [(MegaSpecId::EQUIVALENCE, 0i128), (MegaSpecId::MINI_REX, CHEAT_AMOUNT as i128)]
    {
        let plain = transact_cheat::<NoOpInspector>(spec, None);
        let mut cheat = ResultGasCheat::default();
        let cheated = transact_cheat(spec, Some(&mut cheat));

        assert_eq!(
            cheat.rewrites, 1,
            "{spec:?}: the fixture must reach the inner frame's call_end"
        );
        assert_eq!(
            cheated.result_and_state.result.tx_gas_used() + CHEAT_AMOUNT,
            plain.result_and_state.result.tx_gas_used(),
            "{spec:?}: the rewrite must reach the receipt on both sides of the gate",
        );

        let lane = cheated.inspector_ledger.result;
        assert_eq!(lane.net(), expected_lane, "{spec:?}: result lane net");
        assert_eq!(lane.gross(), expected_lane.unsigned_abs(), "{spec:?}: result lane gross");
        assert_eq!(
            cheated.inspector_ledger.is_zero(),
            expected_lane == 0,
            "{spec:?}: the block guard reads the ledger through is_zero, got {:?}",
            cheated.inspector_ledger,
        );
    }
}

/// The other half of the same statement: it is the frame settlement that is dormant before
/// `MINI_REX`, not the measurement shim. An edit written into the interpreter's own gas counter is
/// booked at the callback boundary that measured it, so it reads the same on both specs.
#[test]
fn test_equivalence_still_books_an_interpreter_gas_rewrite() {
    for spec in [MegaSpecId::EQUIVALENCE, MegaSpecId::MINI_REX] {
        let mut cheat = CounterGasCheat::default();
        let cheated = transact_cheat(spec, Some(&mut cheat));

        assert!(cheat.done, "{spec:?}: the fixture must reach step_end at least once");
        assert_eq!(
            cheated.inspector_ledger.gas.net(),
            CHEAT_AMOUNT as i128,
            "{spec:?}: an interpreter-counter edit is booked on every spec",
        );
        assert!(
            !cheated.inspector_ledger.is_zero(),
            "{spec:?}: the block guard must see it, got {:?}",
            cheated.inspector_ledger,
        );
    }
}
