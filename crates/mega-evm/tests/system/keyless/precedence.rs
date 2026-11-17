//! Which error a `keylessDeploy` call reports when several rules refuse it: the legacy engine's.
//!
//! The rules are checked in the legacy engine's order, and the one charge it made among them —
//! the signer's account, which the creation's nonce bump creates for an empty signer — is made
//! where it made it: after the signer's rules, with the forward checked right after, before the
//! deploy address and the balance. So each pair of failing rules below reports the error the
//! legacy engine reported for it. What the `CREATE` opcode charges the call for the creation's
//! start is this engine's alone, and comes after every rule: a call a rule refuses is refused
//! with that rule, whether or not it could have paid those charges. The last test pins what those
//! charges refuse on their own, where the legacy engine charged nothing.
//!
//! One pair reports an error the legacy engine never reported: a call carrying value whose
//! carried transaction does not decode is refused for the value, where the legacy engine ran the
//! bytecode and reverted with empty data. A call whose arguments do not decode is no deployment
//! at all, with or without value, and runs the bytecode as the legacy engine did.

use mega_evm::{system::keyless::KEYLESS_DEPLOY_OVERHEAD_GAS, test_utils::MemoryDatabase};

use super::*;

/// A deployment signed with `gas_limit`, carrying `value`.
fn signed(gas_limit: u64, value: u64) -> Deployment {
    Deployment::signed(0, gas_limit, U256::from(value), deploying(&runtime(1)))
}

/// `db` with code at `deployment`'s address.
fn occupied(db: MemoryDatabase, deployment: &Deployment) -> MemoryDatabase {
    db.account_code(deployment.address, Bytes::from_static(&[0x00]))
}

/// `db` with `deployment`'s signer holding one wei: an account, which its nonce bump does not
/// create.
fn funded(db: MemoryDatabase, deployment: &Deployment) -> MemoryDatabase {
    db.account_balance(deployment.signer, U256::ONE)
}

/// The gas limit, below the execution cap, at which a call carrying `data` has `left` once it
/// paid its overhead.
fn leaving(data: &Bytes, left: u64) -> u64 {
    reference(data.clone(), GAS_LIMITS[0]).result.gas().total_gas_spent() +
        KEYLESS_DEPLOY_OVERHEAD_GAS +
        left
}

/// A call that carries value and a transaction that does not decode is refused for the value: the
/// value rule comes before decoding the transaction. The same call without value is refused
/// `MalformedEncoding()`. Arguments that do not decode as `(bytes, uint256)` are no deployment at
/// all and reach neither rule: they run the bytecode, which reverts with empty data.
#[test]
fn test_a_value_bearing_call_that_does_not_decode_is_refused_for_the_value() {
    let truncated: Bytes =
        IKeylessDeploy::keylessDeployCall::SELECTOR.iter().copied().chain([0; 16]).collect();
    let undecodable_tx = keyless_deploy_call(b"a transaction", U256::from(LARGE_OVERRIDE));
    for gas_limit in GAS_LIMITS {
        for value in [0, 1] {
            let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, truncated.clone(), U256::from(value));
            tx.0.base.gas_limit = gas_limit;
            let outcome = MegaEvm::new(context(system_db()))
                .execute_transaction(tx)
                .expect("a valid transaction");
            let ExecutionResult::Revert { output, .. } = &outcome.result else {
                panic!("the bytecode did not revert: {:?}", outcome.result);
            };
            assert!(output.is_empty(), "undecodable arguments, value {value}, at {gas_limit}");
        }
        for (value, answer) in
            [(0, KeylessDeployError::MalformedEncoding), (1, KeylessDeployError::NoEtherTransfer)]
        {
            let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, undecodable_tx.clone(), U256::from(value));
            tx.0.base.gas_limit = gas_limit;
            let outcome = MegaEvm::new(context(system_db()))
                .execute_transaction(tx)
                .expect("a valid transaction");
            assert_eq!(
                refusal(&outcome),
                answer,
                "undecodable transaction, value {value}, at {gas_limit}",
            );
        }
    }
}

/// Above the execution cap the reservoir pays the signer's account, and only a signed gas limit
/// near the cap leaves the forward short of it: the forward still comes before the deploy address
/// and the balance.
#[test]
fn test_the_forward_comes_before_the_address_and_the_balance_above_the_cap() {
    let near_cap = TX_GAS_LIMIT_CAP - 50_000;
    let plain = Deployment::signed(0, near_cap, U256::ZERO, deploying(&runtime(1)));
    let carrying = Deployment::signed(0, near_cap, U256::from(2), deploying(&runtime(1)));
    for (rules, db, deployment) in [
        (
            "a forward below the signed gas limit, and an occupied address",
            occupied(funded(system_db(), &plain), &plain),
            &plain,
        ),
        (
            "a forward below the signed gas limit, and a value the signer cannot fund",
            funded(system_db(), &carrying),
            &carrying,
        ),
    ] {
        let outcome = run_with(
            db,
            deployment.call_data(LARGE_OVERRIDE),
            GAS_LIMITS[1],
            EvmTxRuntimeLimits::no_limits(),
        );
        let KeylessDeployError::GasLimitTooLow { tx_gas_limit, provided_gas_limit } =
            refusal(&outcome)
        else {
            panic!("{rules}: {:?}", outcome.result);
        };
        assert_eq!(tx_gas_limit, near_cap, "{rules}");
        assert!(provided_gas_limit < near_cap, "{rules}");
    }
}

/// An out-of-gas at step 14 halts the call, consuming its regular gas, and gives back the state
/// and history gas the call was charged: the transaction's gas used is its gas limit, its state
/// ledger is empty and its history ledger is its body's.
///
/// Two calls run out at the `CREATE` opcode's regular gas: one whose signer holds a wei, which
/// was charged nothing before, and one whose signer has no account, which step 10 charged its
/// account's state gas first. Below the execution cap that charge spilled onto the regular gas:
/// the halt gives it back to the state ledger and then consumes it with the rest, so the second
/// call's gas used is its whole gas limit too. Neither creation started, so the signer's nonce is
/// unspent.
///
/// Expected values `independent`: the gas limit is the transaction's own,
/// and the empty ledgers are zero; the body's history is the schedule's price of its bytes
/// (`constants`).
#[test]
fn test_an_out_of_gas_at_the_creations_charges_consumes_the_regular_gas_alone() {
    let small = signed(1_000, 0);
    let data = small.call_data(LARGE_OVERRIDE);
    for (signer, db, left) in [
        ("holding a wei", funded(system_db(), &small), 1_500),
        ("without an account", system_db(), crate::common::account_state_gas() + 1_500),
    ] {
        let gas_limit = leaving(&data, left);
        let outcome = run_with(db, data.clone(), gas_limit, EvmTxRuntimeLimits::no_limits());
        assert!(
            matches!(outcome.result, ExecutionResult::Halt { .. }),
            "a signer {signer}: {:?}",
            outcome.result
        );
        assert_eq!(outcome.result.tx_gas_used(), gas_limit, "a signer {signer}: the regular gas");
        assert_eq!(outcome.gas.state, 0, "a signer {signer}: the state gas is given back");
        assert_eq!(
            outcome.gas.history,
            crate::common::body_history(data.len() as u64),
            "a signer {signer}: the history gas is the body's",
        );
        assert_eq!(nonce(&outcome, small.signer), 0, "a signer {signer}: no creation started");
    }
}
