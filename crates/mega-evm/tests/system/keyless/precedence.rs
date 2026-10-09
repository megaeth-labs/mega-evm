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
//! arguments do not decode is refused for the value, where the legacy engine ran the bytecode and
//! reverted with empty data.

use mega_evm::{
    system::keyless::{decode_error_result, KEYLESS_DEPLOY_OVERHEAD_GAS},
    test_utils::MemoryDatabase,
};

use super::*;
use crate::common::state_is_free;

/// The gas limit the matrix's deployments are signed with: 500,000 on top of the state gas of the
/// account the creation adds, at the byte prices in effect, so the creation's charges leave part of
/// it whatever a state byte costs.
fn signed_gas() -> u64 {
    500_000 + entry(GasId::create_state_gas())
}

/// What a call is answered with.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// A revert with a rule's error.
    Refused(KeylessDeployError),
    /// An out-of-gas halt.
    OutOfGas,
}

/// A call that fails the rules named, over `db`, carrying `data`, at `gas_limit`.
struct Case {
    rules: &'static str,
    db: MemoryDatabase,
    data: Bytes,
    gas_limit: u64,
    answer: Answer,
}

impl Case {
    fn check(self) {
        let outcome = run_with(self.db, self.data, self.gas_limit, EvmTxRuntimeLimits::no_limits());
        let answer = match &outcome.result {
            ExecutionResult::Revert { output, .. } => {
                Answer::Refused(decode_error_result(output).expect("a keyless deploy error"))
            }
            ExecutionResult::Halt { .. } => Answer::OutOfGas,
            other => panic!("{}: the call was not refused: {other:?}", self.rules),
        };
        assert_eq!(answer, self.answer, "{} at {}", self.rules, self.gas_limit);
    }
}

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

/// `GasLimitTooLow` for a deployment signed with [`signed_gas`], with `provided`.
fn too_low(provided: u64) -> Answer {
    Answer::Refused(KeylessDeployError::GasLimitTooLow {
        tx_gas_limit: signed_gas(),
        provided_gas_limit: provided,
    })
}

/// Pairs of failing rules that do not depend on the call's gas, at both gas limits: the rule the
/// legacy engine checked first is the one reported.
#[test]
fn test_a_call_two_rules_refuse_reports_the_first_the_legacy_engine_checked() {
    let plain = signed(signed_gas(), 0);
    let carrying = signed(signed_gas(), 1);
    for gas_limit in GAS_LIMITS {
        let data = plain.call_data(LARGE_OVERRIDE);
        let cases = [
            Case {
                rules: "gasLimitOverride below the signed gas limit, and an occupied address",
                db: occupied(funded(system_db(), &plain), &plain),
                data: plain.call_data(signed_gas() - 1),
                gas_limit,
                answer: too_low(signed_gas() - 1),
            },
            Case {
                rules: "the signer's nonce, and an occupied address",
                db: occupied(system_db().account_nonce(plain.signer, 2), &plain),
                data: data.clone(),
                gas_limit,
                answer: Answer::Refused(KeylessDeployError::SignerNonceTooHigh { signer_nonce: 2 }),
            },
            Case {
                rules: "the signer's nonce, and a value it cannot fund",
                db: system_db().account_nonce(carrying.signer, 2),
                data: carrying.call_data(LARGE_OVERRIDE),
                gas_limit,
                answer: Answer::Refused(KeylessDeployError::SignerNonceTooHigh { signer_nonce: 2 }),
            },
            Case {
                rules: "the signer's code, and an occupied address",
                db: occupied(
                    system_db().account_code(plain.signer, Bytes::from_static(&[0x00])),
                    &plain,
                ),
                data,
                gas_limit,
                answer: Answer::Refused(KeylessDeployError::SignerHasCode),
            },
            Case {
                rules: "an occupied address, and a value the signer cannot fund",
                db: occupied(system_db().account_nonce(carrying.signer, 1), &carrying),
                data: carrying.call_data(LARGE_OVERRIDE),
                gas_limit,
                answer: Answer::Refused(KeylessDeployError::ContractAlreadyExists),
            },
        ];
        cases.into_iter().for_each(Case::check);
    }
}

/// A call that carries value and whose arguments do not decode, or carry a transaction that does
/// not, is refused for the value: the value rule comes before decoding. The same call without
/// value is refused `MalformedEncoding()`. The legacy engine let both fall through to the
/// bytecode, which reverted with empty data.
#[test]
fn test_a_value_bearing_call_that_does_not_decode_is_refused_for_the_value() {
    let truncated: Bytes =
        IKeylessDeploy::keylessDeployCall::SELECTOR.iter().copied().chain([0; 16]).collect();
    let undecodable_tx = keyless_deploy_call(b"a transaction", U256::from(LARGE_OVERRIDE));
    for gas_limit in GAS_LIMITS {
        for (payload, data) in [("arguments", &truncated), ("transaction", &undecodable_tx)] {
            for (value, answer) in [
                (0, KeylessDeployError::MalformedEncoding),
                (1, KeylessDeployError::NoEtherTransfer),
            ] {
                let mut tx = call_tx(KEYLESS_DEPLOY_ADDRESS, data.clone(), U256::from(value));
                tx.0.base.gas_limit = gas_limit;
                let outcome = MegaEvm::new(context(system_db()))
                    .execute_transaction(tx)
                    .expect("a valid transaction");
                assert_eq!(
                    refusal(&outcome),
                    answer,
                    "undecodable {payload}, value {value}, at {gas_limit}",
                );
            }
        }
    }
}

/// Pairs of failing rules one of which is the call's gas, below the execution cap, where the call
/// pays every charge out of the gas it forwards. The signer's account and the forward come before
/// the deploy address and the balance, as they did in the legacy engine; the charges of the
/// creation's start come after every rule, so a rule they would have run out of gas behind still
/// refuses the call.
#[test]
fn test_the_signers_account_and_the_forward_come_before_the_address_and_the_balance() {
    let new_account = entry(GasId::new_account_state_gas());
    // What a call left one short of the signer's account has once it paid its overhead. The cases
    // that leave it are filtered out below where the account costs nothing.
    let short_of_the_account = new_account.saturating_sub(1);
    let plain = signed(signed_gas(), 0);
    let carrying = signed(signed_gas(), 1);
    let carrying_two = signed(signed_gas(), 2);
    let small = signed(1_000, 0);
    let small_carrying = signed(1_000, 1);
    let data = |deployment: &Deployment| deployment.call_data(LARGE_OVERRIDE);
    let cases = [
        Case {
            rules: "the signer's account the call cannot pay, and an occupied address",
            db: occupied(system_db(), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), short_of_the_account),
            answer: Answer::OutOfGas,
        },
        Case {
            rules: "the signer's account the call cannot pay, and a value it cannot fund",
            db: system_db(),
            data: data(&carrying),
            gas_limit: leaving(&data(&carrying), short_of_the_account),
            answer: Answer::OutOfGas,
        },
        Case {
            rules: "a forward below the signed gas limit, and an occupied address",
            db: occupied(funded(system_db(), &plain), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), signed_gas() - 1),
            answer: too_low(signed_gas() - 1),
        },
        Case {
            rules: "a forward the signer's account takes below the signed gas limit, and an \
                    occupied address",
            db: occupied(system_db(), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), new_account + signed_gas() - 1),
            answer: too_low(signed_gas() - 1),
        },
        Case {
            rules: "a forward below the signed gas limit, and a value the signer cannot fund",
            db: funded(system_db(), &carrying_two),
            data: data(&carrying_two),
            gas_limit: leaving(&data(&carrying_two), signed_gas() - 1),
            answer: too_low(signed_gas() - 1),
        },
        Case {
            rules:
                "an occupied address, and a forward the creation's charges would take below the \
                    signed gas limit",
            db: occupied(funded(system_db(), &plain), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), signed_gas()),
            answer: Answer::Refused(KeylessDeployError::ContractAlreadyExists),
        },
        Case {
            rules: "an occupied address, and creation charges the call cannot pay",
            db: occupied(funded(system_db(), &small), &small),
            data: data(&small),
            gas_limit: leaving(&data(&small), 1_500),
            answer: Answer::Refused(KeylessDeployError::ContractAlreadyExists),
        },
        Case {
            rules: "a value the signer cannot fund, and a forward the creation's charges would \
                    take below the signed gas limit",
            db: system_db(),
            data: data(&carrying),
            gas_limit: leaving(&data(&carrying), new_account + signed_gas()),
            answer: Answer::Refused(KeylessDeployError::InsufficientBalance),
        },
        Case {
            rules: "a value the signer cannot fund, and creation charges the call cannot pay",
            db: system_db(),
            data: data(&small_carrying),
            gas_limit: leaving(&data(&small_carrying), new_account + 1_500),
            answer: Answer::Refused(KeylessDeployError::InsufficientBalance),
        },
    ];
    // An account that costs nothing is one any call can pay for: the two cases of a signer's
    // account the call cannot pay have nothing to run where a state byte is free.
    let free = state_is_free();
    cases
        .into_iter()
        .filter(|case| {
            !(free && case.rules.starts_with("the signer's account the call cannot pay"))
        })
        .for_each(Case::check);
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

/// What the `CREATE` opcode charges the call for the creation's start can refuse a call no rule
/// refuses: a call that cannot pay it runs out of gas, and one it leaves short of the signed gas
/// limit is refused `GasLimitTooLow` with what it left. The legacy engine charged none of it to
/// the call, and ran such a deployment in its sandbox on what the call had. Above the execution
/// cap the refusal is
/// `limits::test_a_refusal_after_the_creations_charges_gives_the_reservoir_back`.
#[test]
fn test_the_creations_charges_can_refuse_a_call_no_rule_refuses() {
    let plain = signed(signed_gas(), 0);
    let small = signed(1_000, 0);
    let data = |deployment: &Deployment| deployment.call_data(LARGE_OVERRIDE);
    let charges = create_regular(deploying(&runtime(1)).len()) +
        entry(GasId::create_state_gas()) +
        2 * record();
    let cases = [
        Case {
            rules: "creation charges the call cannot pay",
            db: funded(system_db(), &small),
            data: data(&small),
            gas_limit: leaving(&data(&small), 1_500),
            answer: Answer::OutOfGas,
        },
        Case {
            rules: "a forward the creation's charges take below the signed gas limit",
            db: funded(system_db(), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), signed_gas()),
            answer: too_low(signed_gas() - charges),
        },
    ];
    cases.into_iter().for_each(Case::check);
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
/// Rules [S15.8], [S15.22]. Expected values `independent`: the gas limit is the transaction's own,
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
