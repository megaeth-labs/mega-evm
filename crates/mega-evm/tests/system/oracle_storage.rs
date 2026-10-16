//! The Oracle's storage, read through the oracle environment: an `SLOAD` in the Oracle's own
//! frame loads the slot through the journal, answers the node's oracle service's value when it
//! has one and the loaded value otherwise, is always priced as a cold access, and is a read of
//! volatile data for gas detention.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::{ORACLE_ACCESS_COMPUTE_GAS, TX_GAS_LIMIT_CAP},
    system::{IOracle, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{
        op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase, ReplayingOracleEnv,
    },
    volatile_data_access_disabled_revert_data, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvs,
    LimitCheck, LimitKind, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome, OracleEnv, OracleRead, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLDATASIZE, DELEGATECALL, DUP2, GAS, JUMPDEST, JUMPI, MSTORE, POP, PUSH0, PUSH1,
        RETURN, SLOAD, SSTORE, STOP, SUB, SWAP1, TIMESTAMP,
    },
    context::TxEnv,
    interpreter::{interpreter::EthInterpreter, interpreter_types::Jumps, Interpreter},
    Inspector,
};

use crate::common::{
    block, call_tx, calls_with, split_outcome, system_db, CALLER, CONTRACT, GAS_LIMIT,
};

/// A second contract, which never is the Oracle.
const OTHER: Address = address!("0x0000000000000000000000000000000000300002");

/// The slot the tests read, and the two values it can hold: the one the oracle service answers
/// and the one the chain's state holds.
const SLOT: U256 = U256::from_limbs([42, 0, 0, 0]);
const SERVICE_VALUE: U256 = U256::from_limbs([0x1234_5678_90ab_cdef, 0, 0, 0]);
const STATE_VALUE: U256 = U256::from_limbs([0xfedc_ba98_7654_3210, 0, 0, 0]);

/// What the oracle service saw, in the order it saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    /// A hint, with its topic and data.
    Hint(B256, Bytes),
    /// A read of a slot.
    Read(U256),
}

/// An oracle service that answers the slots it holds and records every hint and read.
///
/// A hint whose data is one word tells the service to fetch that value for the slot its topic
/// names, so a read after it finds the value and a read before it does not.
#[derive(Clone, Debug, Default)]
struct Service {
    values: Rc<RefCell<HashMap<U256, U256>>>,
    seen: Rc<RefCell<Vec<Seen>>>,
}

impl Service {
    fn holding(slot: U256, value: U256) -> Self {
        let service = Self::default();
        service.values.borrow_mut().insert(slot, value);
        service
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.borrow().clone()
    }
}

impl OracleEnv for Service {
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        self.seen.borrow_mut().push(Seen::Read(slot));
        self.values.borrow().get(&slot).copied()
    }

    fn on_hint(&self, _from: Address, topic: B256, data: Bytes) {
        self.seen.borrow_mut().push(Seen::Hint(topic, data.clone()));
        if data.len() == 32 {
            self.values.borrow_mut().insert(topic.into(), U256::from_be_slice(&data));
        }
    }
}

type Envs = (EmptyExternalEnv, Service);

/// What a transaction did, what gas detention made of its reads, and the oracle reads the EVM
/// reports for it afterwards.
struct Run {
    outcome: MegaTransactionOutcome,
    accessed: VolatileDataAccess,
    limit: Option<u64>,
    evm_reads: Vec<OracleRead>,
}

/// Runs `tx` over `db` against `service`, under `limits`.
fn run_under(
    db: MemoryDatabase,
    service: &Service,
    limits: EvmTxRuntimeLimits,
    configure: impl FnOnce(MegaContext<MemoryDatabase, Envs>) -> MegaContext<MemoryDatabase, Envs>,
    tx: MegaTransaction,
) -> Run {
    let envs = ExternalEnvs { salt_env: EmptyExternalEnv, oracle_env: service.clone() };
    let ctx = MegaContext::new_with_external_envs(db, MegaSpecId::SATIN, envs)
        .with_block(block())
        .with_chain(zero_fee_l1_block_info())
        .with_tx_runtime_limits(limits);
    let mut evm = MegaEvm::new(configure(ctx));
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let detention = evm.ctx().detention();
    Run {
        outcome,
        accessed: detention.accessed(),
        limit: detention.compute_limit(),
        evm_reads: evm.oracle_reads().to_vec(),
    }
}

/// Runs `tx` over `db` against `service`, under the default limits, which detain.
fn run(db: MemoryDatabase, service: &Service, tx: MegaTransaction) -> Run {
    run_under(db, service, EvmTxRuntimeLimits::default(), |ctx| ctx, tx)
}

/// The calldata of `getSlot(slot)`.
fn get_slot(slot: U256) -> Vec<u8> {
    IOracle::getSlotCall { slot }.abi_encode()
}

/// The word a successful call to the Oracle, made through [`calls_with`], returned.
fn returned_word(run: &Run) -> U256 {
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    let output = run.outcome.result.output().cloned().unwrap_or_default();
    let (status, data) = split_outcome(&output);
    assert!(status, "the call to the Oracle failed");
    U256::from_be_slice(data)
}

/// A database holding the system contracts, with `SLOT` of the Oracle at `STATE_VALUE`.
fn db_with_state() -> MemoryDatabase {
    system_db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, STATE_VALUE)
}

/// Code that reads `SLOT` twice and returns what each read cost: the price of `GAS`, of pushing
/// the slot, of the `SLOAD` and of popping its value.
fn two_reads() -> Bytes {
    let read = |code: BytecodeBuilder| code.push_u256(SLOT).append_many([SLOAD, POP, GAS]);
    read(read(BytecodeBuilder::default().append(GAS)))
        // [g0, g1, g2]: store g1 - g2 at 0x20 and g0 - g1 at 0.
        .append_many([DUP2, SUB])
        .push_number(0x20_u8)
        .append(MSTORE)
        .append_many([SWAP1, SUB, PUSH0, MSTORE])
        .push_number(0x40_u8)
        .append_many([PUSH0, RETURN])
        .build()
}

/// What each of the two reads [`two_reads`] makes cost, run at `at`.
fn read_costs(db: MemoryDatabase, service: &Service, at: Address) -> (u64, u64) {
    let run = run(db.account_code(at, two_reads()), service, call_tx(at, [], U256::ZERO));
    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    let output = run.outcome.result.output().cloned().unwrap_or_default();
    (
        U256::from_be_slice(&output[..32]).to::<u64>(),
        U256::from_be_slice(&output[32..64]).to::<u64>(),
    )
}

/// The oracle service's value is what a contract reading the Oracle's slot sees, over the value
/// the chain's state holds, and the read is a read of the Oracle's storage.
#[test]
fn test_oracle_storage_sload_uses_oracle_env() {
    let service = Service::holding(SLOT, SERVICE_VALUE);
    let code = calls_with(CALL, ORACLE_CONTRACT_ADDRESS, &get_slot(SLOT), 0);
    let run = run(
        db_with_state().account_code(CONTRACT, code),
        &service,
        call_tx(CONTRACT, [], U256::ZERO),
    );

    assert_eq!(returned_word(&run), SERVICE_VALUE);
    assert_eq!(service.seen(), vec![Seen::Read(SLOT)]);
    assert_eq!(run.accessed, VolatileDataAccess::ORACLE);
}

/// A slot the oracle service has no value for is read from the chain's state, and is still a
/// read of the Oracle's storage.
#[test]
fn test_oracle_storage_sload_fallback_to_database() {
    let service = Service::default();
    let code = calls_with(CALL, ORACLE_CONTRACT_ADDRESS, &get_slot(SLOT), 0);
    let run = run(
        db_with_state().account_code(CONTRACT, code),
        &service,
        call_tx(CONTRACT, [], U256::ZERO),
    );

    assert_eq!(returned_word(&run), STATE_VALUE);
    assert_eq!(service.seen(), vec![Seen::Read(SLOT)], "the service was asked");
    assert_eq!(run.accessed, VolatileDataAccess::ORACLE);
}

/// A transaction that calls the Oracle directly reads through the service too: the Oracle's own
/// frame is the transaction's.
#[test]
fn test_oracle_storage_sload_direct_call() {
    let service = Service::holding(SLOT, SERVICE_VALUE);
    let run = run(
        db_with_state(),
        &service,
        call_tx(ORACLE_CONTRACT_ADDRESS, get_slot(SLOT), U256::ZERO),
    );

    assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
    assert_eq!(
        IOracle::getSlotCall::abi_decode_returns(run.outcome.result.output().unwrap()).unwrap(),
        B256::from(SERVICE_VALUE),
    );
    assert_eq!(run.accessed, VolatileDataAccess::ORACLE);
}

/// Three reads of one slot cost the same whether the service answered them or the chain's
/// state did: a replaying node cannot tell which source the node that built the block read.
#[test]
fn test_oracle_sload_determinism_between_oracle_env_and_state() {
    let code = BytecodeBuilder::default()
        .push_u256(SLOT)
        .append_many([SLOAD, POP])
        .push_u256(SLOT)
        .append_many([SLOAD, POP])
        .push_u256(SLOT)
        .append_many([SLOAD, POP, STOP])
        .build();
    let from_service = run(
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(ORACLE_CONTRACT_ADDRESS, code.clone()),
        &Service::holding(SLOT, SERVICE_VALUE),
        call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO),
    );
    let from_state = run(
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(ORACLE_CONTRACT_ADDRESS, code)
            .account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, SERVICE_VALUE),
        &Service::default(),
        call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO),
    );

    assert!(from_service.outcome.result.is_success());
    assert!(from_state.outcome.result.is_success());
    assert_eq!(from_service.outcome.result.tx_gas_used(), from_state.outcome.result.tx_gas_used());
    assert_eq!(from_service.outcome.gas, from_state.outcome.gas, "every ledger is the same");
    assert_eq!(from_service.accessed, VolatileDataAccess::ORACLE);
    assert_eq!(from_state.accessed, VolatileDataAccess::ORACLE);
}

/// Every read of the Oracle's storage is a cold access, the second read of a slot in the same
/// transaction included, whichever source answered. The same code in another contract pays the
/// warm price for the second read.
#[test]
fn test_every_oracle_read_is_cold() {
    let params = mega_evm::satin_gas_params();
    let (warm, cold_extra) =
        (params.warm_storage_read_cost(), params.cold_storage_additional_cost());
    // GAS, the push of the slot, the SLOAD and the POP.
    let read = |sload: u64| 2 + 3 + sload + 2;
    let cold = read(warm + cold_extra);

    for service in [Service::holding(SLOT, SERVICE_VALUE), Service::default()] {
        let costs = read_costs(db_with_state(), &service, ORACLE_CONTRACT_ADDRESS);
        assert_eq!(costs, (cold, cold), "both reads of the Oracle's slot are cold");
        assert_eq!(service.seen(), vec![Seen::Read(SLOT), Seen::Read(SLOT)], "and both ask");
    }

    let service = Service::holding(SLOT, SERVICE_VALUE);
    let costs = read_costs(db_with_state(), &service, OTHER);
    assert_eq!(costs, (cold, read(warm)), "another contract's second read is warm");
    assert!(service.seen().is_empty(), "and never asks the oracle service");
}

/// A read of the Oracle's storage is a read of volatile data under the Oracle's own cap: the
/// limit it sets is the transaction's compute at the read — the push and the `SLOAD`, with its
/// cold access — plus the Oracle's cap, not the block environment's.
#[test]
fn test_an_oracle_read_is_detained_under_the_oracles_cap() {
    let code = BytecodeBuilder::default().append_many([PUSH0, SLOAD, STOP]).build();
    for service in [Service::holding(U256::ZERO, SERVICE_VALUE), Service::default()] {
        let limits = EvmTxRuntimeLimits::default()
            .with_block_env_access_compute_gas_limit(7_000_000)
            .with_oracle_access_compute_gas_limit(5_000_000);
        let run = run_under(
            system_db().account_code(ORACLE_CONTRACT_ADDRESS, code.clone()),
            &service,
            limits,
            |ctx| ctx,
            call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO),
        );
        assert!(run.outcome.result.is_success());
        assert_eq!(run.accessed, VolatileDataAccess::ORACLE);
        assert_eq!(run.limit, Some(2 + 2_100 + 5_000_000));
    }

    // Under the default limits the cap is the spec's.
    let run = run(
        system_db().account_code(ORACLE_CONTRACT_ADDRESS, code),
        &Service::default(),
        call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO),
    );
    assert_eq!(run.limit, Some(2 + 2_100 + ORACLE_ACCESS_COMPUTE_GAS));
}

/// A hint reaches the oracle service before a read the transaction makes after it, so the read
/// finds what the hint asked the service to fetch; a read made before the hint does not, and
/// reads the chain's state.
#[test]
fn test_a_hint_reaches_the_service_before_a_later_read() {
    let hint = IOracle::sendHintCall {
        topic: B256::from(SLOT),
        data: Bytes::copy_from_slice(&SERVICE_VALUE.to_be_bytes::<32>()),
    }
    .abi_encode();
    let read = get_slot(SLOT);

    // Each call writes its calldata at 0x100, and a read returns its word at 0.
    let call = |code: BytecodeBuilder, data: &[u8]| {
        code.mstore(0x100, data)
            .push_number(32_u8) // retSize
            .push_number(0_u8) // retOffset
            .push_number(data.len() as u64) // argsSize
            .push_number(0x100_u16) // argsOffset
            .push_number(0_u8) // value
            .push_address(ORACLE_CONTRACT_ADDRESS)
            .append(GAS)
            .append_many([CALL, POP])
    };
    let finish =
        |code: BytecodeBuilder| code.push_number(32_u8).append_many([PUSH0, RETURN]).build();

    for (hint_first, expected, seen) in [
        (
            true,
            SERVICE_VALUE,
            vec![
                Seen::Hint(B256::from(SLOT), SERVICE_VALUE.to_be_bytes::<32>().into()),
                Seen::Read(SLOT),
            ],
        ),
        (
            false,
            STATE_VALUE,
            vec![
                Seen::Read(SLOT),
                Seen::Hint(B256::from(SLOT), SERVICE_VALUE.to_be_bytes::<32>().into()),
            ],
        ),
    ] {
        let code = if hint_first {
            finish(call(call(BytecodeBuilder::default(), &hint), &read))
        } else {
            // The read's word is at 0 before the hint is sent; the hint returns nothing.
            finish(call(call(BytecodeBuilder::default(), &read), &hint))
        };
        let service = Service::default();
        let run = run(
            db_with_state().account_code(CONTRACT, code),
            &service,
            call_tx(CONTRACT, [], U256::ZERO),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(
            U256::from_be_slice(run.outcome.result.output().unwrap()),
            expected,
            "hint first: {hint_first}"
        );
        assert_eq!(service.seen(), seen, "hint first: {hint_first}");
    }
}

/// A read outside the Oracle's own frame is not a read of the Oracle's storage: a contract that
/// runs the Oracle's code on its own storage, through `DELEGATECALL`, reads its own slot from the
/// chain's state, never asks the service, and reads nothing volatile.
#[test]
fn test_a_read_outside_the_oracles_frame_is_an_ordinary_read() {
    let service = Service::holding(SLOT, SERVICE_VALUE);
    let code = calls_with(DELEGATECALL, ORACLE_CONTRACT_ADDRESS, &get_slot(SLOT), 0);
    let db =
        db_with_state().account_code(CONTRACT, code).account_storage(CONTRACT, SLOT, U256::from(7));
    let run = run(db, &service, call_tx(CONTRACT, [], U256::ZERO));

    assert_eq!(returned_word(&run), U256::from(7), "the caller's own slot");
    assert!(service.seen().is_empty());
    assert_eq!(run.accessed, VolatileDataAccess::empty());
    assert_eq!(run.limit, None);
}

/// A frame that cannot pay the cold access reads nothing: it runs out of gas as revm's own
/// skipped cold load does, the service is not asked, and nothing volatile was read.
#[test]
fn test_a_read_the_frame_cannot_pay_for_asks_nothing() {
    let oracle = BytecodeBuilder::default().append_many([PUSH0, SLOAD, STOP]).build();
    // The caller forwards 1,000 gas: enough for the push and the warm part of the `SLOAD`, not
    // for its cold access.
    let caller = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_u16)
        .append(CALL)
        .append_many([PUSH0, MSTORE])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN])
        .build();
    let service = Service::holding(U256::ZERO, SERVICE_VALUE);
    let db =
        system_db().account_code(ORACLE_CONTRACT_ADDRESS, oracle).account_code(CONTRACT, caller);
    let run = run(db, &service, call_tx(CONTRACT, [], U256::ZERO));

    assert!(run.outcome.result.is_success());
    assert_eq!(
        U256::from_be_slice(run.outcome.result.output().unwrap()),
        U256::ZERO,
        "the call failed"
    );
    assert!(service.seen().is_empty(), "a read the frame cannot pay for asks nothing");
    assert!(run.outcome.oracle_reads.is_empty(), "and records nothing");
    assert_eq!(run.accessed, VolatileDataAccess::empty());
}

/// A read refused because the frame's volatile-data access is off asks the service nothing: the
/// frame reverts with `VolatileDataAccessDisabled(Oracle)`.
#[test]
fn test_a_refused_read_asks_nothing() {
    let service = Service::holding(SLOT, SERVICE_VALUE);
    let code = calls_with(CALL, ORACLE_CONTRACT_ADDRESS, &get_slot(SLOT), 0);
    let run = run_under(
        db_with_state().account_code(CONTRACT, code),
        &service,
        EvmTxRuntimeLimits::default(),
        |ctx| ctx.with_volatile_access_disabled_from(1),
        call_tx(CONTRACT, [], U256::ZERO),
    );

    assert!(run.outcome.result.is_success());
    let output = run.outcome.result.output().cloned().unwrap_or_default();
    let (status, data) = split_outcome(&output);
    assert!(!status);
    assert_eq!(data, &volatile_data_access_disabled_revert_data(VolatileDataAccess::ORACLE)[..]);
    assert!(service.seen().is_empty());
    assert!(run.outcome.oracle_reads.is_empty(), "a refused read records nothing");
    assert_eq!(run.accessed, VolatileDataAccess::empty());
}

/// Every read a transaction makes through the service is on its outcome, in the order the
/// service was asked, with the answer the frame saw: the service's value, or `None` where the
/// service had none and the loaded value stood. The record is the service's own view of the
/// transaction, so a validator given it in place of the service answers every read alike.
#[test]
fn test_the_reads_a_transaction_made_are_on_its_outcome() {
    let answered = Service::holding(SLOT, SERVICE_VALUE);
    let code = calls_with(CALL, ORACLE_CONTRACT_ADDRESS, &get_slot(SLOT), 0);
    let tx = || call_tx(CONTRACT, [], U256::ZERO);
    let with_answer = run(db_with_state().account_code(CONTRACT, code.clone()), &answered, tx());
    assert_eq!(returned_word(&with_answer), SERVICE_VALUE);
    let read = OracleRead { slot: SLOT, answer: Some(SERVICE_VALUE) };
    assert_eq!(with_answer.outcome.oracle_reads, [read]);
    assert_eq!(with_answer.evm_reads, [read], "the EVM reports the last transaction's reads");

    let silent = Service::default();
    let without = run(db_with_state().account_code(CONTRACT, code), &silent, tx());
    assert_eq!(returned_word(&without), STATE_VALUE, "the loaded value stood");
    assert_eq!(without.outcome.oracle_reads, [OracleRead { slot: SLOT, answer: None }]);

    // Two reads of one slot in one frame are two records, in order, as the service saw them.
    let answered = Service::holding(SLOT, SERVICE_VALUE);
    let db = db_with_state().account_code(ORACLE_CONTRACT_ADDRESS, two_reads());
    let twice = run(db, &answered, call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO));
    assert!(twice.outcome.result.is_success(), "{:?}", twice.outcome.result);
    assert_eq!(twice.outcome.oracle_reads, [read, read]);
    assert_eq!(answered.seen(), [Seen::Read(SLOT), Seen::Read(SLOT)]);
}

/// Runs `tx` over `db` as a validator does, under the default limits, answering the Oracle's
/// reads from `oracle` in place of a service.
fn validate<O: OracleEnv>(
    db: MemoryDatabase,
    oracle: O,
    tx: MegaTransaction,
) -> MegaTransactionOutcome {
    let envs =
        ExternalEnvs::<(EmptyExternalEnv, O)> { salt_env: EmptyExternalEnv, oracle_env: oracle };
    let ctx = MegaContext::new_with_external_envs(db, MegaSpecId::SATIN, envs)
        .with_block(block())
        .with_chain(zero_fee_l1_block_info())
        .with_tx_runtime_limits(EvmTxRuntimeLimits::default());
    MegaEvm::new(ctx).execute_transaction(tx).expect("the transaction is valid")
}

/// One transaction can be answered two values for one slot: it reads the slot, sends a hint that
/// has the service fetch another value, and reads the slot again. Its outcome records both
/// answers, in order, and a validator given that record replays the transaction. A validator that
/// runs no service answers each read from the Oracle's slot, which only the system address writes,
/// so the transaction finds one value there at both reads: whichever of the two answers the node
/// has the slot hold, the other read comes out differently, and the transaction computes another
/// result.
#[test]
fn test_two_answers_for_one_slot_replay_only_from_the_record() {
    let hint = IOracle::sendHintCall {
        topic: B256::from(SLOT),
        data: Bytes::copy_from_slice(&SERVICE_VALUE.to_be_bytes::<32>()),
    }
    .abi_encode();
    let read = get_slot(SLOT);

    // Each call writes its calldata at 0x100 and its answer at `ret`: the first read's at 0, the
    // second's at 0x20. The hint returns nothing.
    let call = |code: BytecodeBuilder, data: &[u8], ret: u16| {
        code.mstore(0x100, data)
            .push_number(32_u8) // retSize
            .push_number(ret) // retOffset
            .push_number(data.len() as u64) // argsSize
            .push_number(0x100_u16) // argsOffset
            .push_number(0_u8) // value
            .push_address(ORACLE_CONTRACT_ADDRESS)
            .append(GAS)
            .append_many([CALL, POP])
    };
    let code = call(call(call(BytecodeBuilder::default(), &read, 0), &hint, 0x40), &read, 0x20)
        .push_number(64_u8)
        .append_many([PUSH0, RETURN])
        .build();
    let db = || db_with_state().account_code(CONTRACT, code.clone());
    let tx = || call_tx(CONTRACT, [], U256::ZERO);
    let words = |outcome: &MegaTransactionOutcome| {
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let output = outcome.result.output().cloned().unwrap_or_default();
        (U256::from_be_slice(&output[..32]), U256::from_be_slice(&output[32..64]))
    };

    // The building node: its service has no value for the slot until the hint, so the first read
    // takes the chain's value and the second the value the hint had the service fetch.
    let built = run(db(), &Service::default(), tx()).outcome;
    assert_eq!(words(&built), (STATE_VALUE, SERVICE_VALUE));
    assert_eq!(
        built.oracle_reads,
        [
            OracleRead { slot: SLOT, answer: None },
            OracleRead { slot: SLOT, answer: Some(SERVICE_VALUE) }
        ],
        "two answers for one slot, in order"
    );

    // A validator given the record answers both reads as the service did.
    let record = ReplayingOracleEnv::new(built.oracle_reads.clone());
    let replayed = validate(db(), record.clone(), tx());
    assert!(record.replayed_exactly(), "every recorded answer was replayed, in order");
    assert_eq!(replayed.result, built.result);
    assert_eq!(replayed.state, built.state);
    assert_eq!(replayed.gas, built.gas);
    assert_eq!(replayed.usage, built.usage);
    assert_eq!(replayed.oracle_reads, built.oracle_reads);

    // A validator without a service finds one value in the slot at both reads, whichever of the
    // two answers the slot holds.
    for held in [STATE_VALUE, SERVICE_VALUE] {
        let db = db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, held);
        let without = validate(db, ReplayingOracleEnv::absent(), tx());
        assert_eq!(words(&without), (held, held), "the slot holds {held:#x}");
        assert_ne!(without.result, built.result, "the slot holds {held:#x}");
    }
}

/// The Oracle's own code answers `getSlot` from the service too: the read is the `SLOAD` in its
/// frame, whatever code runs there.
#[test]
fn test_the_deployed_oracle_reads_through_the_service() {
    let service = Service::holding(SLOT, SERVICE_VALUE);
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_u64))
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE);
    let run = run(db, &service, call_tx(ORACLE_CONTRACT_ADDRESS, get_slot(SLOT), U256::ZERO));
    assert_eq!(
        IOracle::getSlotCall::abi_decode_returns(run.outcome.result.output().unwrap()).unwrap(),
        B256::from(SERVICE_VALUE),
    );
}

/// A detained frame whose regular gas holds the cold access, but which may not spend it past the
/// compute limit, reads and asks the service all the same: the read's charge then stops the
/// transaction at the limit, and the Oracle read is not marked. A frame whose regular gas cannot
/// hold the cold access at all asks nothing
/// ([`test_a_read_the_frame_cannot_pay_for_asks_nothing`]).
#[test]
fn test_a_detained_frame_that_may_not_spend_the_cold_access_asks_and_is_stopped() {
    let code = BytecodeBuilder::default().append_many([TIMESTAMP, POP, PUSH0, SLOAD, STOP]).build();
    let service = Service::default();
    let run = run_under(
        system_db().account_code(ORACLE_CONTRACT_ADDRESS, code),
        &service,
        EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(1_500),
        |ctx| ctx,
        call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO),
    );

    assert_eq!(service.seen(), vec![Seen::Read(U256::ZERO)], "the service was asked");
    assert!(
        matches!(
            run.outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, .. })
        ),
        "{:?}",
        run.outcome.limit_exceeded
    );
    assert_eq!(run.accessed, VolatileDataAccess::TIMESTAMP, "the Oracle read is not marked");
    assert_eq!(run.limit, Some(2 + 1_500), "the timestamp's limit binds");
}

/* ---------- which source answered leaves no trace ---------- */

/// Below the execution cap, and above it, where the reservoir pays state and history gas first.
const TIERS: [u64; 2] = [GAS_LIMIT, TX_GAS_LIMIT_CAP + GAS_LIMIT];

/// A transaction from [`CALLER`] to `to` with `data`, carrying `gas_limit`.
fn tx_with_gas(to: Address, data: &[u8], gas_limit: u64) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data: Bytes::copy_from_slice(data),
        gas_limit,
        ..Default::default()
    }))
}

/// Code for the Oracle's own frame that reads `SLOT` when `read` is set, then writes 7 to it and
/// returns what the write cost the frame's regular gas, with the two pushes and the `GAS` that
/// measure it.
fn write_after(read: bool) -> Bytes {
    let code = BytecodeBuilder::default();
    let code = if read { code.push_u256(SLOT).append_many([SLOAD, POP]) } else { code };
    code.append(GAS)
        .push_number(7_u8)
        .push_u256(SLOT)
        // [g0, g1]: return g0 - g1.
        .append_many([SSTORE, GAS, SWAP1, SUB, PUSH0, MSTORE])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN])
        .build()
}

/// A write after a read of the Oracle's slot costs the same whether the oracle service answered
/// the read or the chain's state did, below and above the execution cap: both load the slot
/// through the journal, so the write finds it warm either way, and a node that replays the block
/// without the service prices it as the node that built it did. The chain holds the value the
/// service answers, as it does for the replaying node. Without the read the same write pays the
/// cold access.
#[test]
fn test_a_write_after_a_read_costs_the_same_whichever_source_answered() {
    let cold_write = mega_evm::satin_gas_params().cold_storage_cost();
    for gas_limit in TIERS {
        let at = |read: bool, service: &Service| {
            let db = system_db()
                .account_code(ORACLE_CONTRACT_ADDRESS, write_after(read))
                .account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, SERVICE_VALUE);
            let run = run(db, service, tx_with_gas(ORACLE_CONTRACT_ADDRESS, &[], gas_limit));
            assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
            let cost = U256::from_be_slice(run.outcome.result.output().unwrap()).to::<u64>();
            (cost, run.outcome)
        };
        let service = Service::holding(SLOT, SERVICE_VALUE);
        let (from_service, served) = at(true, &service);
        assert_eq!(service.seen(), vec![Seen::Read(SLOT)], "at {gas_limit}: the service answered");
        let (from_state, read) = at(true, &Service::default());
        let (unread, _) = at(false, &Service::default());

        assert_eq!(from_service, from_state, "at {gas_limit}: the write costs the same");
        assert_eq!(served.result.tx_gas_used(), read.result.tx_gas_used(), "at {gas_limit}");
        assert_eq!(served.gas, read.gas, "at {gas_limit}: every ledger is the same");
        assert_eq!(unread, from_state + cold_write, "at {gas_limit}: the read warmed the slot");
    }
}

/// The same through the deployed Oracle, which any sender reaches: `multiCall` runs `getSlot`,
/// then `setSlot`, whose body writes the slot before its check reverts the call for a sender that
/// is not the system address. The transaction costs the same whichever source answered the read,
/// below and above the execution cap.
#[test]
fn test_a_multicall_read_then_write_costs_the_same_whichever_source_answered() {
    let get = IOracle::getSlotCall { slot: SLOT }.abi_encode();
    let set = IOracle::setSlotCall { slot: SLOT, value: B256::from(U256::from(7)) }.abi_encode();
    let data = IOracle::multiCallCall { data: vec![get.into(), set.into()] }.abi_encode();
    let not_system_address = &alloy_primitives::keccak256("NotSystemAddress()")[..4];
    for gas_limit in TIERS {
        let at = |service: &Service| {
            let db = system_db().account_storage(ORACLE_CONTRACT_ADDRESS, SLOT, SERVICE_VALUE);
            let run = run(db, service, tx_with_gas(ORACLE_CONTRACT_ADDRESS, &data, gas_limit));
            let output = run.outcome.result.output().cloned().unwrap_or_default();
            assert!(!run.outcome.result.is_success(), "at {gas_limit}: {:?}", run.outcome.result);
            assert_eq!(&output[..], not_system_address, "at {gas_limit}");
            run.outcome
        };
        let service = Service::holding(SLOT, SERVICE_VALUE);
        let served = at(&service);
        assert_eq!(service.seen(), vec![Seen::Read(SLOT)], "at {gas_limit}: the service answered");
        let read = at(&Service::default());

        assert_eq!(served.result.tx_gas_used(), read.result.tx_gas_used(), "at {gas_limit}");
        assert_eq!(served.gas, read.gas, "at {gas_limit}: every ledger is the same");
    }
}

/// The slot a read loaded is in the transaction's state, which a stateless witness is built
/// from, whichever source answered: a node that replays the block without the service finds it
/// there. The state holds the chain's value, not the service's, and the read changed nothing.
#[test]
fn test_the_read_slot_is_in_the_transactions_state_whichever_source_answered() {
    for (service, answered) in
        [(Service::holding(SLOT, SERVICE_VALUE), SERVICE_VALUE), (Service::default(), STATE_VALUE)]
    {
        let run = run(
            db_with_state(),
            &service,
            call_tx(ORACLE_CONTRACT_ADDRESS, get_slot(SLOT), U256::ZERO),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        assert_eq!(
            IOracle::getSlotCall::abi_decode_returns(run.outcome.result.output().unwrap()).unwrap(),
            B256::from(answered),
        );

        let slot = run.outcome.state[&ORACLE_CONTRACT_ADDRESS]
            .storage
            .get(&SLOT)
            .unwrap_or_else(|| panic!("answered {answered}: the slot is in the state"));
        assert_eq!((slot.original_value, slot.present_value), (STATE_VALUE, STATE_VALUE));
        assert!(!slot.is_changed(), "answered {answered}");
    }
}

/* ---------- reads next to writes and in nested frames ---------- */

/// A read after the Oracle's frame wrote the slot answers the service's value over the one the
/// frame stored, as on the legacy engine, and costs the cold access like every other read; with
/// no value from the service it answers what the frame stored.
#[test]
fn test_the_services_value_wins_over_the_frames_own_write() {
    let params = mega_evm::satin_gas_params();
    // The push of the slot, the `SLOAD` and the `GAS` after it.
    let cold = 3 + params.warm_storage_read_cost() + params.cold_storage_additional_cost() + 2;
    let code = BytecodeBuilder::default()
        .push_number(99_u8)
        .push_u256(SLOT)
        .append_many([SSTORE, GAS])
        .push_u256(SLOT)
        // [g0, value, g1]: the value at 0, g0 - g1 at 0x20.
        .append_many([SLOAD, GAS, SWAP1, PUSH0, MSTORE, SWAP1, SUB])
        .push_number(0x20_u8)
        .append(MSTORE)
        .push_number(0x40_u8)
        .append_many([PUSH0, RETURN])
        .build();

    for (service, expected) in [
        (Service::holding(SLOT, SERVICE_VALUE), SERVICE_VALUE),
        (Service::default(), U256::from(99)),
    ] {
        let run = run(
            db_with_state().account_code(ORACLE_CONTRACT_ADDRESS, code.clone()),
            &service,
            call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO),
        );
        assert!(run.outcome.result.is_success(), "{:?}", run.outcome.result);
        let output = run.outcome.result.output().cloned().unwrap_or_default();
        assert_eq!(U256::from_be_slice(&output[..32]), expected);
        assert_eq!(U256::from_be_slice(&output[32..64]).to::<u64>(), cold, "the read is cold");
        assert_eq!(service.seen(), vec![Seen::Read(SLOT)]);
    }
}

/// Records what every `SLOAD` cost the frame that ran it, with the frame's depth.
#[derive(Debug, Default)]
struct SloadCosts {
    running: Option<(usize, u64)>,
    costs: Vec<(usize, u64)>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for SloadCosts {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if interp.bytecode.opcode() == SLOAD {
            self.running = Some((interp.input.depth, interp.gas.remaining()));
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if let Some((depth, before)) = self.running.take() {
            self.costs.push((depth, before - interp.gas.remaining()));
        }
    }
}

/// Every read of a nested Oracle frame goes through the service and costs the cold access too:
/// the Oracle reads the slot once, calls itself, and the nested frame reads it twice.
#[test]
fn test_reads_in_a_nested_oracle_frame_are_cold_and_ask() {
    let params = mega_evm::satin_gas_params();
    let cold = params.warm_storage_read_cost() + params.cold_storage_additional_cost();
    // Called with calldata, the code jumps to the nested frame's reads.
    let outer = BytecodeBuilder::default()
        .push_u256(SLOT)
        .append_many([SLOAD, POP, PUSH0, PUSH0])
        .push_number(1_u8) // argsSize
        .append_many([PUSH0, PUSH0]) // argsOffset, value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL, POP, STOP])
        .build_vec();
    let nested = BytecodeBuilder::default()
        .append(JUMPDEST)
        .push_u256(SLOT)
        .append_many([SLOAD, POP])
        .push_u256(SLOT)
        .append_many([SLOAD, POP, STOP])
        .build_vec();
    let mut code = vec![CALLDATASIZE, PUSH1, (4 + outer.len()) as u8, JUMPI];
    code.extend(outer);
    code.extend(nested);

    for service in [Service::holding(SLOT, SERVICE_VALUE), Service::default()] {
        let envs: ExternalEnvs<Envs> =
            ExternalEnvs { salt_env: EmptyExternalEnv, oracle_env: service.clone() };
        let db = db_with_state().account_code(ORACLE_CONTRACT_ADDRESS, Bytes::from(code.clone()));
        let ctx = MegaContext::new_with_external_envs(db, MegaSpecId::SATIN, envs)
            .with_block(block())
            .with_chain(zero_fee_l1_block_info());
        let mut evm = MegaEvm::new(ctx).with_inspector(SloadCosts::default());
        let outcome = evm
            .execute_transaction(call_tx(ORACLE_CONTRACT_ADDRESS, [], U256::ZERO))
            .expect("the transaction is valid");

        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(evm.inspector().costs, vec![(0, cold), (1, cold), (1, cold)]);
        assert_eq!(service.seen(), vec![Seen::Read(SLOT); 3]);
    }
}
