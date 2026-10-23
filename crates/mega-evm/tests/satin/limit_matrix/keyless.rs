//! Authorization totals and a keyless creation's forward, from the page. Kind 1 on the
//! authorization list runs under reduced transaction limits. A fatal database error is a
//! block verdict; the injected error here is not fatal, so the test checks that the transaction
//! has no outcome.

use alloy_primitives::{hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    system::keyless::{
        decode_error_result, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
    },
    test_utils::{
        note_price_guard, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase,
        SequencerRegistryRoles,
    },
    MegaEvm, MegaLimitExceeded, MegaTransaction,
};
use revm::{
    context::{result::ExecutionResult, transaction::TransactionType},
    context_interface::{
        cfg::GasId,
        either::Either,
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    },
};

use super::{
    at_spec_prices, call_tx, delegatecall, execute, execute_forward, frame_share,
    halted_out_of_gas, nonce_of, reservoir_share, reverted_stop, store_n_slots, world, Forward,
    ACCOUNT_LEAF_STATE_GAS_M1, BODY_BYTES, CALLER, CHILD, CONTRACT, EXECUTION_CAP, EXISTING,
    INDICATOR_STATE_GAS, TX_STATE_GROWTH_LIMIT,
};
use crate::common::{self, context};

/// Above any forward a `keylessDeploy` call here can make.
const LARGE_OVERRIDE: u64 = 10_000_000_000;
/// Signed gas of a creation that only stops. Below what the call has left.
const SIGNED_STOP_GAS: u64 = 1_000_000;
/// Signed gas of a creation that fills hundreds of slots. Below the call's regular gas, above the
/// state gas of 980 slots at the spec's byte price.
const SIGNED_SLOT_GAS: u64 = 150_000_000;
/// The gas price Nick's Method signs. The outer transaction's gas price stays zero.
const SIGNED_GAS_PRICE: u128 = 100_000_000_000;

/// One authorization the list simulates.
struct Auth {
    /// `None` is an unrecoverable signature.
    authority: Option<Address>,
    chain_id: u64,
    nonce: u64,
    delegate: Address,
}

/// A pre-EIP-155 creation, signed with Nick's Method's fixed signature.
struct Deployment {
    tx: Bytes,
    signer: Address,
    address: Address,
}

impl Deployment {
    fn signed(gas_limit: u64, init_code: Bytes) -> Self {
        let tx = TxLegacy {
            nonce: 0,
            gas_price: SIGNED_GAS_PRICE,
            gas_limit,
            to: TxKind::Create,
            value: U256::ZERO,
            input: init_code,
            chain_id: None,
        };
        let word = U256::from_be_bytes(hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));
        let signed = Signed::new_unchecked(tx, Signature::new(word, word, false), B256::ZERO);
        let mut encoded = Vec::new();
        signed.rlp_encode(&mut encoded);
        let signer = signed.recover_signer().expect("a Nick's-Method signature recovers");
        Self { tx: encoded.into(), signer, address: signer.create(0) }
    }

    fn call_data(&self, gas_limit_override: u64) -> Bytes {
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: self.tx.clone(),
            gasLimitOverride: U256::from(gas_limit_override),
        }
        .abi_encode()
        .into()
    }
}

fn keyless_db() -> MemoryDatabase {
    // The relayer is [`CALLER`]. The registry's system address is a different account, so this
    // call is not a system-address transaction.
    MemoryDatabase::default()
        .rex6_predeploys(SequencerRegistryRoles::from(mega_evm::system::MEGA_SYSTEM_ADDRESS))
        .account_balance(CALLER, U256::from(10u128.pow(24)))
}

fn keyless_tx(data: Bytes, gas_limit: u64) -> MegaTransaction {
    common::call_with_data(CALLER, KEYLESS_DEPLOY_ADDRESS, data, gas_limit)
}

/// Gas above the execution cap that can pay `count` new authorities from the reservoir.
fn gas_above(count: u64) -> Option<u64> {
    let history = common::history(BODY_BYTES + count.saturating_mul(101));
    let per = common::account_state_gas().saturating_add(indicator_at_schedule());
    let state = count.checked_mul(per)?.checked_add(common::account_state_gas())?;
    let Some(gas) = history
        .checked_add(state)
        .and_then(|gas| gas.checked_add(5_000_000))
        .and_then(|gas| gas.checked_add(EXECUTION_CAP))
    else {
        note_price_guard("the authorization list does not fit above the execution cap");
        return None;
    };
    Some(gas)
}

/// Gas below the execution cap. A list of 1,001 authorities cannot pay its state gas from it.
fn gas_below(count: u64) -> Option<u64> {
    let history = common::history(BODY_BYTES + count.saturating_mul(101));
    let floor = history.saturating_add(21_000).saturating_add(count.saturating_mul(25_000));
    if floor >= EXECUTION_CAP {
        note_price_guard("the authorization list's history does not fit below the execution cap");
        return None;
    }
    Some(EXECUTION_CAP - 1)
}

/// Gas that covers one authority's intrinsic cost and cannot pay its leaf and indicator.
fn gas_under_one_authority() -> Option<u64> {
    let floor = common::history(BODY_BYTES + 101).saturating_add(80_000);
    let bill = common::account_state_gas().saturating_add(indicator_at_schedule());
    if bill <= floor || floor >= EXECUTION_CAP {
        note_price_guard("one authority's state gas fits in the gas under test");
        return None;
    }
    Some(floor)
}

fn indicator_at_schedule() -> u64 {
    mega_evm::satin_gas_params().get(GasId::tx_eip7702_state_gas_bytecode())
}

fn pins_state() -> bool {
    at_spec_prices() && !common::state_is_free()
}

fn authority(n: u16) -> Address {
    let mut raw = [0u8; 20];
    raw[0] = 0xa1;
    raw[18] = (n >> 8) as u8;
    raw[19] = n as u8;
    Address::from(raw)
}

fn auth_tx(gas_limit: u64, value: U256, to: Address, auths: &[Auth]) -> MegaTransaction {
    let authorization_list = auths
        .iter()
        .map(|auth| {
            Either::Right(RecoveredAuthorization::new_unchecked(
                Authorization {
                    chain_id: U256::from(auth.chain_id),
                    address: auth.delegate,
                    nonce: auth.nonce,
                },
                auth.authority.map_or(RecoveredAuthority::Invalid, RecoveredAuthority::Valid),
            ))
        })
        .collect();
    let mut tx = call_tx(gas_limit);
    tx.0.base.tx_type = TransactionType::Eip7702 as u8;
    tx.0.base.kind = TxKind::Call(to);
    tx.0.base.value = value;
    tx.0.base.gas_priority_fee = Some(0);
    tx.0.base.authorization_list = authorization_list;
    tx
}

/// Code that returns what one `BALANCE` of `address` cost: `GAS`, `PUSH20`, `BALANCE`, `POP`,
/// `GAS`, and the difference, which is the access plus 7.
fn balance_cost(address: Address) -> Bytes {
    use revm::bytecode::opcode::{BALANCE, GAS, POP, SUB, SWAP1};
    BytecodeBuilder::default()
        .append(GAS)
        .push_address(address)
        .append_many([BALANCE, POP, GAS, SWAP1, SUB])
        .return_top()
        .build()
}

/// The cost of the `BALANCE` [`balance_cost`] measured.
fn access_cost(outcome: &mega_evm::MegaTransactionOutcome) -> u64 {
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("the measuring call did not succeed: {:?}", outcome.result);
    };
    U256::from_be_slice(output.data()).to::<u64>() - 7
}

fn new_auth(n: u16, nonce: u64) -> Auth {
    Auth { authority: Some(authority(n)), chain_id: 0, nonce, delegate: CHILD }
}

fn many(count: u16) -> Vec<Auth> {
    (1..=count).map(|n| new_auth(n, 0)).collect()
}

fn keyless_failure(outcome: &mega_evm::MegaTransactionOutcome) -> KeylessDeployError {
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("the keyless call did not succeed: {:?}", outcome.result);
    };
    let ret = IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
        .expect("a keylessDeploy return");
    decode_error_result(&ret.errorData).expect("errorData is a keyless error")
}

/// The creation's state-gas and history charges the call makes before it forwards the reservoir:
/// the signer's leaf, the deploy address's leaf, and one record for each.
fn start_charges() -> u64 {
    2 * common::account_state_gas() + 2 * common::history(super::WRITE_RECORD_SIZE)
}

/// 1,001 distinct new authorities, below the execution cap. Apply none. The transaction's own
/// frame stops before it runs, kind 3, `limit` 1,000. No authority is created.
#[test]
fn test_an_authorization_list_of_1001_new_authorities_stops_before_the_first_frame() {
    let Some(gas) = gas_below(1_001) else { return };
    let db = world(&[(CONTRACT, Bytes::from_static(&[0x00]))]);
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &many(1_001)));
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT });
    assert!(outcome.limit_exceeded.is_some());
    assert_eq!(nonce_of(&outcome, authority(1)), 0, "no authority account is kept");
    assert_eq!(nonce_of(&outcome, CALLER), 1, "the sender's nonce stands");
    assert_eq!(outcome.usage.write_records, 0);
    if pins_state() {
        assert_eq!(outcome.gas.state, 0, "no authority state gas is charged");
    }
}

/// 1,000 new authorities fit and are applied. The transaction's value would create one more
/// account. That recipient stops the first frame, kind 3, `limit` 0, and is not kept. The
/// authorities stay.
#[test]
fn test_a_fitting_list_keeps_the_authorities_when_the_recipient_stops() {
    let Some(gas) = gas_above(1_000) else { return };
    let recipient = Address::from_word(B256::repeat_byte(0x27));
    let db = world(&[(CONTRACT, Bytes::from_static(&[0x00]))]);
    let outcome = execute(db, auth_tx(gas, U256::from(1), recipient, &many(1_000)));
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: 0 });
    assert_eq!(super::balance_of(&outcome, recipient), U256::ZERO, "the recipient is not kept");
    assert_eq!(nonce_of(&outcome, authority(1)), 1, "an applied authority stays");
    assert_eq!(nonce_of(&outcome, authority(1_000)), 1);
}

/// Signer absent, deploy address empty, a creation that deploys one byte. The call is the top
/// frame. The creation's regular gas is `min(gasLimitOverride, remaining)`, which at this gas
/// limit is above the 98/100 cap of the execution cap. After the call's start charges, the two
/// leaves and the two records, the creation receives the reservoir share. Its state-growth budget
/// is the share of the call's whole 1,000, `floor(1,000 × 98 / 100)` = 980, the signer's rise
/// being counted on the call once the creation returned, and the empty deploy address is +1
/// against it: 980 fresh slots take it to 981 and stop it at 980.
#[test]
fn test_a_keyless_creation_receives_the_reservoir_share_and_a_980_budget() {
    let one_byte = BytecodeBuilder::default().return_with_data([0x00]).build();
    let deployment = Deployment::signed(SIGNED_STOP_GAS, one_byte);
    let data = deployment.call_data(LARGE_OVERRIDE);
    let reservoir_after_body = 50 * super::SLOT_STATE_GAS_M1;
    let gas = EXECUTION_CAP + common::body_history(data.len() as u64) + reservoir_after_body;
    let (outcome, probe) = execute_forward(keyless_db(), keyless_tx(data, gas), Forward::any());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(super::has_code(&outcome, deployment.address), "the creation succeeds");
    let (forwarded, reservoir) = probe.created.expect("the creation started");
    assert!(
        forwarded > super::gas_cap(EXECUTION_CAP),
        "regular gas {forwarded} is min(override, remaining), above the 98/100 cap {}",
        super::gas_cap(EXECUTION_CAP)
    );
    let charges = start_charges();
    assert!(reservoir_after_body > charges, "the reservoir covers the start charges");
    let remainder = reservoir_after_body - charges;
    assert_eq!(
        reservoir,
        reservoir_share(remainder),
        "creation reservoir {reservoir}; remainder after the start charges {remainder}; share {}",
        reservoir_share(remainder)
    );

    if 980 * common::slot_state_gas() > SIGNED_SLOT_GAS {
        note_price_guard("980 slots of state gas do not fit the keyless forward");
        return;
    }
    let filling = Deployment::signed(SIGNED_SLOT_GAS, store_n_slots(980));
    let data = filling.call_data(LARGE_OVERRIDE);
    let state = 980 * common::slot_state_gas() + 2 * common::account_state_gas();
    let gas = EXECUTION_CAP + common::body_history(data.len() as u64) + state + 5_000_000;
    let outcome = execute(keyless_db(), keyless_tx(data, gas));
    let KeylessDeployError::ExecutionReverted { output, .. } = keyless_failure(&outcome) else {
        panic!("the creation did not revert: {:?}", outcome.result);
    };
    let stop = MegaLimitExceeded::abi_decode(&output).expect("the creation's revert is a stop");
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: frame_share(1_000) });
    assert_eq!(nonce_of(&outcome, filling.signer), 1, "the signer's bump stands");
    assert!(!super::has_code(&outcome, filling.address));
}

/// The clauses EIP-7702 already decides come first: one rise, one record and one indicator for two
/// acceptances of one new authority; nothing loaded or counted for a wrong chain id, a nonce of
/// 2^64 − 1 or an unrecoverable signature, and no warm access for the first; a zero delegation
/// target that creates its authority without an indicator; an authorization signed for the
/// sender's pre-transaction nonce, skipped; no rise for an existing authority; no indicator for one
/// that holds one at the start, a clear and a later non-zero authorization included; one indicator
/// for a clear and a later non-zero authorization of an authority that holds none; a database
/// error on a consulted authority, after an overflowing prefix too, gives no outcome. Then the
/// 999 / 1,000 / 1,001 boundaries of the state-growth stop.
///
/// Where the list would cross both KV and state growth, the kind is 1. Whether a database error is
/// fatal is the node's database's to say, a block verdict this EVM call does not report; the
/// witness of every loaded authority is the block witness harness's, not this outcome's.
#[test]
fn test_authorization_list_boundaries() {
    let stop_code = Bytes::from_static(&[0x00]);
    let db = world(&[(CONTRACT, stop_code.clone())]);
    let Some(gas) = gas_above(2) else { return };

    // Two acceptances of one new authority: one rise, one record, one indicator.
    let twice = [new_auth(1, 0), new_auth(1, 1)];
    let outcome = execute(db.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &twice));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(nonce_of(&outcome, authority(1)), 2);
    assert_eq!(outcome.usage.write_records, 1);
    if pins_state() {
        assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1 + INDICATOR_STATE_GAS);
    }

    // Wrong chain id, nonce 2^64 − 1, unrecoverable signature: not loaded, not created.
    for auth in [
        Auth { authority: Some(authority(2)), chain_id: 7, nonce: 0, delegate: CHILD },
        Auth { authority: Some(authority(3)), chain_id: 0, nonce: u64::MAX, delegate: CHILD },
        Auth { authority: None, chain_id: 0, nonce: 0, delegate: CHILD },
    ] {
        let outcome = execute(db.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &[auth]));
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(outcome.usage.write_records, 0);
        if pins_state() {
            assert_eq!(outcome.gas.state, 0);
        }
    }
    assert_eq!(
        nonce_of(
            &execute(
                db.clone(),
                auth_tx(
                    gas,
                    U256::ZERO,
                    CONTRACT,
                    &[Auth {
                        authority: Some(authority(2)),
                        chain_id: 7,
                        nonce: 0,
                        delegate: CHILD
                    },]
                )
            ),
            authority(2)
        ),
        0
    );

    // A zero delegation target creates the account and charges no indicator.
    let zero =
        Auth { authority: Some(authority(4)), chain_id: 0, nonce: 0, delegate: Address::ZERO };
    let outcome = execute(db.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &[zero]));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(nonce_of(&outcome, authority(4)), 1);
    assert_eq!(outcome.usage.write_records, 1);
    if pins_state() {
        assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1);
    }

    // Signed for the sender's pre-transaction nonce: skipped, and the sender adds no record.
    let sender = Auth { authority: Some(CALLER), chain_id: 0, nonce: 0, delegate: CHILD };
    let outcome = execute(db.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &[sender]));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 0);
    if pins_state() {
        assert_eq!(outcome.gas.state, 0);
    }

    // An existing authority adds no state growth. The indicator is charged once.
    let mut existing = db.clone();
    existing.set_account_balance(authority(5), U256::from(1));
    let outcome = execute(existing, auth_tx(gas, U256::ZERO, CONTRACT, &[new_auth(5, 0)]));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 1);
    if pins_state() {
        assert_eq!(outcome.gas.state, INDICATOR_STATE_GAS);
    }

    // An authority that already holds an indicator is charged none, a clear and a later
    // non-zero authorization included.
    let mut delegated = crate::detention::with_delegation(db.clone(), authority(6), CHILD);
    delegated.set_account_nonce(authority(6), 1);
    delegated.set_account_balance(authority(6), U256::from(1));
    let outcome = execute(delegated.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &[new_auth(6, 1)]));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    if pins_state() {
        assert_eq!(outcome.gas.state, 0);
    }
    let clear_and_set = [
        Auth { authority: Some(authority(6)), chain_id: 0, nonce: 1, delegate: Address::ZERO },
        new_auth(6, 2),
    ];
    let outcome = execute(delegated, auth_tx(gas, U256::ZERO, CONTRACT, &clear_and_set));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(nonce_of(&outcome, authority(6)), 3);
    if pins_state() {
        assert_eq!(outcome.gas.state, 0, "an indicator held at the start is never charged");
    }

    // An entry skipped for its chain id is not loaded, so it is not warmed.
    let measuring = world(&[(CONTRACT, balance_cost(authority(2)))]);
    let wrong_chain =
        Auth { authority: Some(authority(2)), chain_id: 7, nonce: 0, delegate: CHILD };
    let outcome = execute(measuring, auth_tx(gas, U256::ZERO, CONTRACT, &[wrong_chain]));
    assert!(access_cost(&outcome) > super::WARM_READ, "the skipped authority is cold");

    // A clear of an authority that holds none, then the first non-zero designator: one indicator.
    let clear_then = [
        Auth { authority: Some(authority(7)), chain_id: 0, nonce: 0, delegate: Address::ZERO },
        new_auth(7, 1),
    ];
    let outcome = execute(db.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &clear_then));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(nonce_of(&outcome, authority(7)), 2);
    assert_eq!(outcome.usage.write_records, 1);
    if pins_state() {
        assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1 + INDICATOR_STATE_GAS);
    }

    // A database error on a consulted authority gives the transaction no outcome.
    let mut failing = ErrorInjectingDatabase::new(db.clone());
    failing.fail_on_account = Some(authority(8));
    let err = MegaEvm::new(context(failing)).execute_transaction(auth_tx(
        gas,
        U256::ZERO,
        CONTRACT,
        &[new_auth(8, 0)],
    ));
    assert!(err.is_err(), "a database error is not a limit stop: {err:?}");

    // Both totals crossed: the kind is 1 and `limit` the KV transaction limit.
    {
        let limits = mega_evm::EvmTxRuntimeLimits::default()
            .with_tx_kv_update_limit(3)
            .with_tx_state_growth_limit(3);
        let outcome =
            super::execute_under(db.clone(), auth_tx(gas, U256::ZERO, CONTRACT, &many(4)), limits);
        assert_eq!(reverted_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 3 });
        assert_eq!(nonce_of(&outcome, authority(1)), 0);
    }

    // The same, after a prefix of 1,001 new authorities that would already cross the limit.
    if let Some(gas) = gas_above(1_002) {
        let mut failing = ErrorInjectingDatabase::new(db);
        failing.fail_on_account = Some(authority(1_002));
        let mut list = many(1_001);
        list.push(new_auth(1_002, 0));
        let err = MegaEvm::new(context(failing)).execute_transaction(auth_tx(
            gas,
            U256::ZERO,
            CONTRACT,
            &list,
        ));
        assert!(err.is_err(), "an overflowing prefix does not hide the fault: {err:?}");
    }

    // 999 applied: the first frame's budget is 1. The second fresh slot stops it, `limit` 1.
    let Some(gas) = gas_above(999) else { return };
    let db = world(&[(CONTRACT, store_n_slots(2))]);
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &many(999)));
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: 1 });
    assert!(super::slot_value(&outcome, CONTRACT, 1).is_zero());

    // 1,000 applied: the budget is 0. One slot stops it, `limit` 0. The authorities stay.
    let Some(gas) = gas_above(1_000) else { return };
    let db = world(&[(CONTRACT, store_n_slots(1))]);
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &many(1_000)));
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: 0 });
    assert_eq!(nonce_of(&outcome, authority(1)), 1);

    // 1,001 skip the list. Above the cap, so a success here is the missing stop.
    let Some(gas) = gas_above(1_001) else { return };
    let db = world(&[(CONTRACT, stop_code)]);
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &many(1_001)));
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT });
    assert_eq!(nonce_of(&outcome, authority(1)), 0);
    assert_eq!(outcome.usage.write_records, 0);
}

/// Signer nonce 0, signer absent. With the deploy address's +1, the init code's 980th fresh slot
/// crosses the creation's budget of 980, the share of the call's 1,000; the signer's rise is the
/// call's, counted once the creation returned. The call answers `ExecutionReverted` with that stop.
/// The deploy account and its record are taken back. The signer's bump stands, and its record with
/// it, while the account still differs.
#[test]
fn test_a_keyless_creation_from_nonce_0_stopped_in_its_initcode() {
    if 980 * common::slot_state_gas() > SIGNED_SLOT_GAS {
        note_price_guard("980 slots of state gas do not fit the keyless forward");
        return;
    }
    let deployment = Deployment::signed(SIGNED_SLOT_GAS, store_n_slots(980));
    let data = deployment.call_data(LARGE_OVERRIDE);
    let data_len = data.len() as u64;
    let state = 980 * common::slot_state_gas() + 2 * common::account_state_gas();
    let gas = EXECUTION_CAP + common::body_history(data.len() as u64) + state + 5_000_000;
    let outcome = execute(keyless_db(), keyless_tx(data, gas));
    let KeylessDeployError::ExecutionReverted { output, .. } = keyless_failure(&outcome) else {
        panic!("the creation did not revert: {:?}", outcome.result);
    };
    let stop = MegaLimitExceeded::abi_decode(&output).expect("the revert data is a stop");
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: frame_share(1_000) });
    assert_eq!(nonce_of(&outcome, deployment.signer), 1);
    assert!(!super::has_code(&outcome, deployment.address));
    assert!(outcome.limit_exceeded.is_none(), "the call caught the creation's stop");
    // The signer's record stands, the deploy address's goes with the creation.
    assert_eq!(outcome.usage.write_records, 1);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + data_len + super::WRITE_RECORD_SIZE);
    if pins_state() {
        assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1, "only the signer's leaf");
    }
}

/// The same creation from signer nonce 1. The bump does not create the signer, so the creation's
/// budget is `floor(1,000 × 98 / 100)` = 980, and the deploy address's rise is inside it. 980
/// fresh slots cross that budget. The bump is the last nonce change the deployment made and is
/// taken back, with its record and that record's history.
#[test]
fn test_a_keyless_creation_from_nonce_1_takes_the_bump_back() {
    if 980 * common::slot_state_gas() > SIGNED_SLOT_GAS {
        note_price_guard("980 slots of state gas do not fit the keyless forward");
        return;
    }
    let deployment = Deployment::signed(SIGNED_SLOT_GAS, store_n_slots(980));
    let mut db = keyless_db();
    db.set_account_nonce(deployment.signer, 1);
    db.set_account_balance(deployment.signer, U256::from(1));
    let data = deployment.call_data(LARGE_OVERRIDE);
    let data_len = data.len() as u64;
    let state = 980 * common::slot_state_gas() + common::account_state_gas();
    let gas = EXECUTION_CAP + common::body_history(data.len() as u64) + state + 5_000_000;
    let outcome = execute(db, keyless_tx(data, gas));
    let KeylessDeployError::ExecutionReverted { output, .. } = keyless_failure(&outcome) else {
        panic!("the creation did not revert: {:?}", outcome.result);
    };
    let stop = MegaLimitExceeded::abi_decode(&output).expect("the revert data is a stop");
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: frame_share(TX_STATE_GROWTH_LIMIT) });
    assert_eq!(nonce_of(&outcome, deployment.signer), 1, "the bump is taken back");
    assert!(!super::has_code(&outcome, deployment.address));
    // The signer is again as the transaction loaded it: its record and the record's history go.
    assert_eq!(outcome.usage.write_records, 0);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + data_len);
    assert_eq!(outcome.gas.history, common::body_history(data_len));
    if pins_state() {
        assert_eq!(outcome.gas.state, 0, "the deploy address's leaf goes with the creation");
    }
}

/// The constructor `DELEGATECALL`s a library that self-destructs the account, then returns one
/// byte of runtime. The creation's EVM result succeeds, and the call answers `EmptyCodeDeployed`
/// with the zero address. The destruction gives the deploy address's leaf and its record back to
/// the destroying frame, inside the creation, so what the creation spent from both pools net of
/// that give-back is below its regular gas, and `gasUsed` is the regular gas it spent. The code is
/// deposited after the mark: no state gas, and its data size and history stay charged. The
/// account is removed at the end of the transaction.
#[test]
fn test_a_keyless_constructor_that_self_destructs_answers_empty_code() {
    if !pins_state() {
        return;
    }
    let library = BytecodeBuilder::default().selfdestruct(EXISTING).build();
    let mut prefix = delegatecall(CHILD).append(revm::bytecode::opcode::POP).build().to_vec();
    prefix.extend_from_slice(&BytecodeBuilder::default().return_with_data([0x00]).build());
    let deployment = Deployment::signed(SIGNED_STOP_GAS, Bytes::from(prefix));
    let mut db = keyless_db();
    db.set_account_code(CHILD, library);
    db.set_account_balance(EXISTING, U256::from(1));
    db.set_account_code(EXISTING, Bytes::from_static(&[0x00]));
    let data = deployment.call_data(LARGE_OVERRIDE);
    let data_len = data.len() as u64;
    let gas = EXECUTION_CAP +
        common::body_history(data.len() as u64) +
        2 * common::account_state_gas() +
        5_000_000;
    let (outcome, probe) = execute_forward(db, keyless_tx(data, gas), Forward::any());
    let KeylessDeployError::EmptyCodeDeployed { gas_used } = keyless_failure(&outcome) else {
        panic!("expected EmptyCodeDeployed: {:?}", outcome.result);
    };
    let (forwarded, _) = probe.created.expect("the creation started");
    let left = probe.create_end_remaining.expect("the creation ended");
    assert_eq!(gas_used, forwarded - left, "gasUsed is the creation's regular gas");
    assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1, "only the signer's leaf stands");
    // The signer's record and the deposited byte stay; the deploy address's record goes.
    assert_eq!(
        outcome.gas.history,
        (BODY_BYTES + data_len + super::WRITE_RECORD_SIZE + 1) * super::HISTORY_GAS_PER_BYTE
    );
    assert!(super::is_destroyed(&outcome, deployment.address));
}

/// The list fits. The authority's state gas cannot be paid. Out of gas, and the authorization is
/// taken back. This is not the limit stop.
#[test]
fn test_an_authority_whose_state_gas_cannot_be_paid_is_taken_back() {
    let Some(gas) = gas_under_one_authority() else { return };
    let db = world(&[(CONTRACT, Bytes::from_static(&[0x00]))]);
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &[new_auth(1, 0)]));
    assert!(halted_out_of_gas(&outcome), "out of gas, not a limit stop: {:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_none());
    assert_eq!(nonce_of(&outcome, authority(1)), 0, "the authorization is taken back");
    assert_eq!(outcome.usage.write_records, 0);
}

/// The list does not fit, and the state gas it would have charged also cannot be paid. The list is
/// skipped and the first frame stops with the limit. The out-of-gas path is not taken.
#[test]
fn test_a_list_that_does_not_fit_is_not_the_out_of_gas_path() {
    let Some(gas) = gas_below(1_001) else { return };
    let db = world(&[(CONTRACT, Bytes::from_static(&[0x00]))]);
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &many(1_001)));
    assert!(!halted_out_of_gas(&outcome), "the out-of-gas path is not taken: {:?}", outcome.result);
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT });
    assert_eq!(outcome.usage.write_records, 0);
    if pins_state() {
        assert_eq!(outcome.gas.state, 0);
    }
}

/// The list fits. One authority holds code that is not a delegation. The entry is skipped and its
/// nonce is not bumped; a later entry for it is compared with that unbumped nonce. The authority
/// was loaded before the code check, so it is warm.
#[test]
fn test_an_authority_with_non_delegation_code_is_skipped() {
    let Some(gas) = gas_above(1) else { return };
    let mut db = world(&[(CONTRACT, balance_cost(authority(1)))]);
    db.set_account_code(authority(1), Bytes::from_static(&[0x00]));
    db.set_account_balance(authority(1), U256::from(1));
    let auths = [new_auth(1, 0), new_auth(1, 0)];
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &auths));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(access_cost(&outcome), super::WARM_READ, "the authority is warm");
    assert_eq!(nonce_of(&outcome, authority(1)), 0, "the simulated nonce is not bumped");
    assert_eq!(outcome.usage.write_records, 0);
    if pins_state() {
        assert_eq!(outcome.gas.state, 0);
    }
}

/// Two entries for one new authority, both signed for nonce 0. The first is accepted, the
/// simulated nonce becomes 1, and the authority is created once, with one record and one
/// indicator. The second does not match that nonce and is skipped. The authority is warm.
#[test]
fn test_two_entries_for_one_new_authority_accept_the_first() {
    let Some(gas) = gas_above(1) else { return };
    let db = world(&[(CONTRACT, balance_cost(authority(1)))]);
    let auths = [new_auth(1, 0), new_auth(1, 0)];
    let outcome = execute(db, auth_tx(gas, U256::ZERO, CONTRACT, &auths));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(access_cost(&outcome), super::WARM_READ, "the authority is warm");
    assert_eq!(nonce_of(&outcome, authority(1)), 1);
    assert_eq!(outcome.usage.write_records, 1);
    if pins_state() {
        assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1 + INDICATOR_STATE_GAS);
    }
}
