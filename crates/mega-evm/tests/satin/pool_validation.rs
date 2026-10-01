//! What a transaction pool decides without state agrees with what the EVM decides with it.
//!
//! [`validate_transaction_stateless`] runs the handler's own validation phases on an EVM over an
//! empty database. Every row below is run through it and through a Satin EVM over a database that
//! holds the accounts the transaction touches, at the gas limit the helper names as the least one
//! and one gas below it: the helper admits exactly what the EVM admits, refuses with the error the
//! EVM refuses with, and the intrinsic gas it reports is what the EVM charged — the floor on the
//! result, and the body's history on the common execution layer.

use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    history_gas,
    system::{live_system_address, ORACLE_CONTRACT_ADDRESS},
    test_utils::{op_transaction, MemoryDatabase},
    transaction_body_bytes, validate_transaction_stateless, IntrinsicGas, MegaEvm, MegaSpecId,
    MegaTransaction, WRITE_RECORD_SIZE,
};
use op_revm::{OpHaltReason, OpTransactionError};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
        transaction::{AccessList, AccessListItem, TransactionType},
        CfgEnv, TxEnv,
    },
    ExecuteEvm,
};

use crate::common::{account_state_gas, authorizing_call, block, context, history_is_free};

const CALLER: Address = address!("0000000000000000000000000000000000c00000");
const EXISTING: Address = address!("0000000000000000000000000000000000c00001");
const NEW: Address = address!("0000000000000000000000000000000000c00002");
const DELEGATE: Address = address!("0000000000000000000000000000000000c00003");
const AUTHORITY: Address = address!("0000000000000000000000000000000000c00004");
const SYSTEM: Address = address!("0000000000000000000000000000000000c0ffee");

/// A gas limit every row's intrinsic gas fits in, below the execution cap.
const ROOMY: u64 = 10_000_000;

/// A transaction the rows build, on the gas limit they are given.
type Build = fn(u64) -> MegaTransaction;

/// A transaction from `caller` of type `tx_type`, on `gas_limit`.
fn tx(tx_type: TransactionType, caller: Address, kind: TxKind, gas_limit: u64) -> TxEnv {
    TxEnv { tx_type: tx_type as u8, caller, kind, gas_limit, ..Default::default() }
}

fn wrap(tx: TxEnv) -> MegaTransaction {
    alloy_op_evm::OpTx(op_transaction(tx))
}

fn call(gas_limit: u64) -> MegaTransaction {
    wrap(tx(TransactionType::Legacy, CALLER, TxKind::Call(EXISTING), gas_limit))
}

fn call_with_calldata(gas_limit: u64) -> MegaTransaction {
    let mut call = tx(TransactionType::Legacy, CALLER, TxKind::Call(EXISTING), gas_limit);
    call.data = Bytes::from(vec![0x5a; 100]);
    wrap(call)
}

fn value_to_existing(gas_limit: u64) -> MegaTransaction {
    let mut call = tx(TransactionType::Legacy, CALLER, TxKind::Call(EXISTING), gas_limit);
    call.value = U256::from(1);
    wrap(call)
}

fn value_to_new(gas_limit: u64) -> MegaTransaction {
    let mut call = tx(TransactionType::Legacy, CALLER, TxKind::Call(NEW), gas_limit);
    call.value = U256::from(1);
    wrap(call)
}

fn value_to_self(gas_limit: u64) -> MegaTransaction {
    let mut call = tx(TransactionType::Legacy, CALLER, TxKind::Call(CALLER), gas_limit);
    call.value = U256::from(1);
    wrap(call)
}

fn create(gas_limit: u64) -> MegaTransaction {
    let mut create = tx(TransactionType::Legacy, CALLER, TxKind::Create, gas_limit);
    // PUSH1 0 PUSH1 0 RETURN, padded to 64 bytes: two init-code words.
    let mut init_code = vec![0x60, 0x00, 0x60, 0x00, 0xf3];
    init_code.resize(64, 0);
    create.data = Bytes::from(init_code);
    wrap(create)
}

fn authorizations(gas_limit: u64) -> MegaTransaction {
    authorizing_call(CALLER, EXISTING, U256::ZERO, gas_limit, DELEGATE, &[(AUTHORITY, 0), (NEW, 0)])
}

fn access_list(gas_limit: u64) -> MegaTransaction {
    let mut call = tx(TransactionType::Eip2930, CALLER, TxKind::Call(EXISTING), gas_limit);
    call.access_list = AccessList(vec![AccessListItem {
        address: EXISTING,
        storage_keys: (0..3).map(B256::with_last_byte).collect(),
    }]);
    wrap(call)
}

/// A deposit carrying a kilobyte of calldata: it pays no history gas, so the floor, which prices
/// every byte at 64 gas where the intrinsic charge prices it at 16, is what the gas limit must
/// cover.
fn deposit_with_calldata(gas_limit: u64) -> MegaTransaction {
    let mut deposit = call(gas_limit);
    deposit.0.base.data = Bytes::from(vec![0x5a; 1_024]);
    deposit.0.deposit.source_hash = B256::repeat_byte(0x11);
    deposit
}

/// A call to the Oracle from `caller`, of type `tx_type`: of the system shape when it is legacy.
fn to_the_oracle(caller: Address, tx_type: TransactionType, gas_limit: u64) -> MegaTransaction {
    let mut call = tx(tx_type, caller, TxKind::Call(ORACLE_CONTRACT_ADDRESS), gas_limit);
    call.data = Bytes::from(vec![0x5a; 36]);
    if tx_type != TransactionType::Legacy {
        call.gas_priority_fee = Some(0);
    }
    wrap(call)
}

fn system_transaction(gas_limit: u64) -> MegaTransaction {
    to_the_oracle(SYSTEM, TransactionType::Legacy, gas_limit)
}

fn system_shape_from_a_user(gas_limit: u64) -> MegaTransaction {
    to_the_oracle(CALLER, TransactionType::Legacy, gas_limit)
}

fn another_shape_from_the_system_address(gas_limit: u64) -> MegaTransaction {
    to_the_oracle(SYSTEM, TransactionType::Eip1559, gas_limit)
}

/// A database holding what the transactions touch: a funded caller, an existing recipient, the
/// system address's account, and, when `system_address` names one, a `SequencerRegistry` naming
/// it.
///
/// The system address exists so that its transaction runs on what the helper names: a deposit-like
/// transaction whose caller does not exist is charged for creating it when it runs, which only the
/// state can tell.
fn db(system_address: Option<Address>) -> MemoryDatabase {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(EXISTING, U256::from(1))
        .account_balance(SYSTEM, U256::from(1));
    match system_address {
        Some(address) => db.sequencer_registry(address),
        None => db,
    }
}

/// The error an EVM reported, whatever its database: the transaction's or the header's.
fn describe<DBError: core::fmt::Debug>(err: EVMError<DBError, OpTransactionError>) -> String {
    match err {
        EVMError::Transaction(invalid) => format!("{invalid:?}"),
        EVMError::Header(invalid) => format!("{invalid:?}"),
        other => panic!("not a validation error: {other:?}"),
    }
}

/// What the helper decides for `tx`, given the live system address a pool reads off the state
/// [`db`] holds for `system_address`.
fn helper(tx: MegaTransaction, system_address: Option<Address>) -> Result<IntrinsicGas, String> {
    let live = live_system_address(db(system_address)).expect("the read succeeds");
    assert_eq!(live, system_address, "the state names the system address the row gives");
    validate_transaction_stateless(CfgEnv::new_with_spec(MegaSpecId::SATIN), block(), tx, live)
        .map_err(describe)
}

/// What a Satin EVM decided for a transaction.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Admitted, and charged this floor and this history for the body: the floor on the result,
    /// the body's history on the common execution layer.
    Admitted { floor: u64, history: u64 },
    /// Included as a failed deposit: op-revm does not refuse a deposit that fails validation, it
    /// bumps the sender's nonce and reports the whole gas limit used.
    FailedDeposit,
    /// Refused with this error.
    Refused(String),
}

/// What a Satin EVM over [`db`] decides for `tx`.
fn evm(tx: MegaTransaction, system_address: Option<Address>) -> Verdict {
    let mut evm = MegaEvm::new(context(db(system_address)));
    match ExecuteEvm::transact(&mut evm, tx) {
        Err(err) => Verdict::Refused(describe(err)),
        Ok(outcome) => match outcome.result {
            ExecutionResult::Halt { reason: OpHaltReason::FailedDeposit, .. } => {
                Verdict::FailedDeposit
            }
            result => Verdict::Admitted {
                floor: result.gas().floor_gas(),
                history: evm.ctx().additional_limit().intrinsic_history_gas(),
            },
        },
    }
}

/// The error a gas limit below `gas` is refused with.
fn below(gas: IntrinsicGas, gas_limit: u64) -> String {
    let invalid = if gas.total() >= gas.floor {
        InvalidTransaction::CallGasCostMoreThanGasLimit { initial_gas: gas.total(), gas_limit }
    } else {
        InvalidTransaction::GasFloorMoreThanGasLimit { gas_floor: gas.floor, gas_limit }
    };
    format!("{:?}", OpTransactionError::Base(invalid))
}

/// One row: a transaction, the live system address the helper is given and the database's
/// registry names, and whether the transaction is executed as a deposit.
struct Row {
    name: &'static str,
    build: Build,
    system_address: Option<Address>,
    deposit_like: bool,
}

impl Row {
    const fn new(name: &'static str, build: Build, system_address: Option<Address>) -> Self {
        Self { name, build, system_address, deposit_like: false }
    }

    const fn deposit_like(mut self) -> Self {
        self.deposit_like = true;
        self
    }

    /// The helper's intrinsic gas, then the least gas limit and the one below it through both.
    /// Below the least one the helper reports the error; the EVM refuses with it, or, for a
    /// transaction executed as a deposit, includes it as a failed deposit.
    fn agree(&self) -> IntrinsicGas {
        let Self { name, build, system_address, deposit_like } = *self;
        let gas =
            helper(build(ROOMY), system_address).unwrap_or_else(|err| panic!("{name}: {err}"));
        let least = gas.min_gas_limit();
        assert_eq!(
            helper(build(least), system_address),
            Ok(gas),
            "{name}: the gas limit moves nothing"
        );
        assert_eq!(
            evm(build(least), system_address),
            Verdict::Admitted { floor: gas.floor, history: gas.history },
            "{name}: the EVM admits the least gas limit and charges what the helper reports",
        );
        let error = below(gas, least - 1);
        assert_eq!(
            helper(build(least - 1), system_address),
            Err(error.clone()),
            "{name}: the helper"
        );
        let expected = if deposit_like { Verdict::FailedDeposit } else { Verdict::Refused(error) };
        assert_eq!(evm(build(least - 1), system_address), expected, "{name}: the EVM");
        gas
    }
}

/// Every row the helper admits, against the EVM.
#[test]
fn test_the_helper_and_the_evm_agree_on_what_they_admit() {
    let rows = [
        Row::new("a call", call, None),
        Row::new("a call with calldata", call_with_calldata, None),
        Row::new("value to an existing account", value_to_existing, None),
        Row::new("value to a new account", value_to_new, None),
        Row::new("value to the sender itself", value_to_self, None),
        Row::new("a creation", create, None),
        Row::new("EIP-7702 authorizations", authorizations, None),
        Row::new("an access list", access_list, None),
        Row::new("a deposit whose floor binds", deposit_with_calldata, None).deposit_like(),
        Row::new("a system-address transaction", system_transaction, Some(SYSTEM)).deposit_like(),
        Row::new("the system shape from a user", system_shape_from_a_user, Some(SYSTEM)),
        Row::new(
            "another shape from the system address",
            another_shape_from_the_system_address,
            Some(SYSTEM),
        ),
        Row::new("the system shape with no registry", system_transaction, None),
    ];
    for row in rows {
        let gas = row.agree();
        let name = row.name;
        assert_eq!(
            gas.state, 0,
            "{name}: EIP-2780 charges the state a transaction adds when it runs"
        );
        // The transactions executed as deposits are the ones exempt from history gas.
        let body = if row.deposit_like {
            0
        } else {
            history_gas(transaction_body_bytes(&(row.build)(ROOMY))).unwrap()
        };
        assert_eq!(
            gas.history, body,
            "{name}: the body's history, or none for an exempt transaction"
        );
    }
}

/// What a pool reads off the helper's numbers.
#[test]
fn test_what_the_intrinsic_gas_is_made_of() {
    let helper = |build: Build, system_address| helper(build(ROOMY), system_address).unwrap();

    // Whether a recipient is new is the state's to say: the intrinsic gas of a value transfer is
    // the same either way, and the new account is charged when the transaction runs.
    assert_eq!(helper(value_to_new, None), helper(value_to_existing, None));
    // A self-transfer pays EIP-2780's base alone; a transfer to another account pays for reaching
    // it and for the value on top.
    assert!(helper(value_to_self, None).regular < helper(call, None).regular);
    assert!(helper(call, None).regular < helper(value_to_existing, None).regular);

    // A deposit and a system-address transaction pay no history; the same transaction from
    // anyone else, or of another shape, or with no registry naming its sender, does.
    assert_eq!(helper(deposit_with_calldata, None).history, 0);
    assert_eq!(helper(system_transaction, Some(SYSTEM)).history, 0);
    if !history_is_free() {
        assert!(helper(system_shape_from_a_user, Some(SYSTEM)).history > 0);
        assert!(helper(another_shape_from_the_system_address, Some(SYSTEM)).history > 0);
        assert!(helper(system_transaction, None).history > 0);
    }

    // The floor binds a transaction that pays no history, and it is what its gas limit must
    // cover.
    let deposit = helper(deposit_with_calldata, None);
    assert!(deposit.floor > deposit.total());
    assert_eq!(deposit.min_gas_limit(), deposit.floor);
}

fn on_another_chain() -> MegaTransaction {
    let mut tx = call(ROOMY);
    tx.0.base.chain_id = Some(4326);
    tx
}

fn without_authorizations() -> MegaTransaction {
    authorizing_call(CALLER, EXISTING, U256::ZERO, ROOMY, DELEGATE, &[])
}

fn above_the_block_gas_limit() -> MegaTransaction {
    call(block().gas_limit + 1)
}

fn init_code_over_the_limit() -> MegaTransaction {
    let mut tx = create(ROOMY);
    tx.0.base.data = Bytes::from(vec![0; mega_evm::constants::MAX_INITCODE_SIZE + 1]);
    tx
}

fn nonce_at_the_maximum() -> MegaTransaction {
    let mut tx = call(ROOMY);
    tx.0.base.nonce = u64::MAX;
    tx
}

fn priority_fee_above_the_fee_cap() -> MegaTransaction {
    let mut tx = call(ROOMY);
    tx.0.base.tx_type = TransactionType::Eip1559 as u8;
    tx.0.base.gas_price = 1;
    tx.0.base.gas_priority_fee = Some(2);
    tx
}

/// A transaction the EVM refuses without reading state, and the error it refuses it with.
type Refusal = (fn() -> MegaTransaction, InvalidTransaction);

/// Every row the EVM refuses without reading state, the helper refuses alike.
#[test]
fn test_the_helper_and_the_evm_agree_on_what_they_refuse() {
    let refusals: [Refusal; 6] = [
        (on_another_chain, InvalidTransaction::InvalidChainId),
        (without_authorizations, InvalidTransaction::EmptyAuthorizationList),
        (above_the_block_gas_limit, InvalidTransaction::CallerGasLimitMoreThanBlock),
        (init_code_over_the_limit, InvalidTransaction::CreateInitCodeSizeLimit),
        (nonce_at_the_maximum, InvalidTransaction::NonceOverflowInTransaction),
        (priority_fee_above_the_fee_cap, InvalidTransaction::PriorityFeeGreaterThanMaxFee),
    ];
    for (build, invalid) in refusals {
        let error = format!("{:?}", OpTransactionError::Base(invalid));
        assert_eq!(helper(build(), None), Err(error.clone()), "the helper");
        assert_eq!(evm(build(), None), Verdict::Refused(error), "the EVM");
    }
}

/// Above the execution cap the gas limit is valid when the regular part and the floor fit under
/// the cap, the rest going to the reservoir; a floor above the cap is refused, by both.
#[test]
fn test_the_execution_cap_is_held_alike() {
    let above_the_cap = |len: usize| {
        let mut tx = call(block().gas_limit);
        tx.0.base.data = Bytes::from(vec![0x5a; len]);
        tx
    };
    let admitted = helper(above_the_cap(1_024), None).expect("a floor under the cap");
    assert!(admitted.floor < TX_GAS_LIMIT_CAP);
    assert!(matches!(evm(above_the_cap(1_024), None), Verdict::Admitted { .. }));

    // 64 gas a byte in the floor: enough bytes to take it past the cap.
    let len = (TX_GAS_LIMIT_CAP / 64) as usize + 1;
    let refused = helper(above_the_cap(len), None);
    assert!(
        refused.as_ref().is_err_and(|err| err.starts_with("Base(GasFloorMoreThanGasLimit")),
        "{refused:?}",
    );
    assert_eq!(evm(above_the_cap(len), None), Verdict::Refused(refused.unwrap_err()));
}

/// The least gas limit is what validation requires, not what a transaction's start costs: the
/// history of the write record a value transfer makes for its recipient, and the state gas of a
/// recipient that is new, are charged once the transaction is admitted. At the least gas limit a
/// call without value and a transfer to the sender run; a value transfer runs out of gas unless
/// its record's history is on top, and one to a new account unless that account's state gas is
/// too: it is included, and halts out of gas. It holds at any byte price: a price of nothing
/// leaves nothing to pay on top.
#[test]
fn test_the_least_gas_limit_leaves_out_what_the_start_adds() {
    let succeeds = |build: Build, gas_limit| {
        let mut evm = MegaEvm::new(context(db(None)));
        let result = ExecuteEvm::transact(&mut evm, build(gas_limit)).expect("admitted").result;
        if !result.is_success() {
            assert!(
                matches!(
                    result,
                    ExecutionResult::Halt {
                        reason: OpHaltReason::Base(HaltReason::OutOfGas(_)),
                        ..
                    }
                ),
                "{result:?}"
            );
        }
        result.is_success()
    };
    let least = |build: Build| helper(build(ROOMY), None).unwrap().min_gas_limit();
    let record = history_gas(WRITE_RECORD_SIZE).unwrap();
    let account = account_state_gas();

    assert!(succeeds(call, least(call)));
    assert!(succeeds(value_to_self, least(value_to_self)));

    let existing = least(value_to_existing);
    assert_eq!(succeeds(value_to_existing, existing), record == 0);
    assert!(succeeds(value_to_existing, existing + record));
    if record > 0 {
        assert!(!succeeds(value_to_existing, existing + record - 1));
    }

    let new = least(value_to_new);
    assert_eq!(new, existing, "the intrinsic gas does not say whether the recipient is new");
    assert_eq!(succeeds(value_to_new, new + record), account == 0);
    assert!(succeeds(value_to_new, new + record + account));
    if record + account > 0 {
        assert!(!succeeds(value_to_new, new + record + account - 1));
    }
}
