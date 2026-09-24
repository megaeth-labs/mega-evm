//! The Oracle's storage, read through the oracle environment: an `SLOAD` in the Oracle's own
//! frame asks the node's oracle service first and the database when the service has no value,
//! is always priced as a cold access, and is a read of volatile data for gas detention.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::ORACLE_ACCESS_COMPUTE_GAS,
    system::{IOracle, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    volatile_data_access_disabled_revert_data, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvs,
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionOutcome, OracleEnv,
    VolatileDataAccess,
};
use revm::bytecode::opcode::{
    CALL, DELEGATECALL, DUP2, GAS, MSTORE, POP, PUSH0, RETURN, SLOAD, STOP, SUB, SWAP1,
};

use crate::common::{block, call_tx, calls_with, split_outcome, system_db, CALLER, CONTRACT};

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

/// What a transaction did, and what gas detention made of its reads.
struct Run {
    outcome: MegaTransactionOutcome,
    accessed: VolatileDataAccess,
    limit: Option<u64>,
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
    Run { outcome, accessed: detention.accessed(), limit: detention.compute_limit() }
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
    assert_eq!(service.seen(), vec![Seen::Read(SLOT)], "the service was asked first");
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
    assert_eq!(run.accessed, VolatileDataAccess::empty());
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
