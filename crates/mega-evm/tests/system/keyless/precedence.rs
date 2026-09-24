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

use mega_evm::{
    system::keyless::{decode_error_result, KEYLESS_DEPLOY_OVERHEAD_GAS},
    test_utils::MemoryDatabase,
};

use super::*;

/// The gas limit the matrix's deployments are signed with.
const SIGNED: u64 = 500_000;

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

/// `GasLimitTooLow` for a deployment signed with [`SIGNED`], with `provided`.
const fn too_low(provided: u64) -> Answer {
    Answer::Refused(KeylessDeployError::GasLimitTooLow {
        tx_gas_limit: SIGNED,
        provided_gas_limit: provided,
    })
}

/// Pairs of failing rules that do not depend on the call's gas, at both gas limits: the rule the
/// legacy engine checked first is the one reported.
#[test]
fn test_a_call_two_rules_refuse_reports_the_first_the_legacy_engine_checked() {
    let plain = signed(SIGNED, 0);
    let carrying = signed(SIGNED, 1);
    for gas_limit in GAS_LIMITS {
        let data = plain.call_data(LARGE_OVERRIDE);
        let cases = [
            Case {
                rules: "gasLimitOverride below the signed gas limit, and an occupied address",
                db: occupied(funded(system_db(), &plain), &plain),
                data: plain.call_data(SIGNED - 1),
                gas_limit,
                answer: too_low(SIGNED - 1),
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

/// Pairs of failing rules one of which is the call's gas, below the execution cap, where the call
/// pays every charge out of the gas it forwards. The signer's account and the forward come before
/// the deploy address and the balance, as they did in the legacy engine; the charges of the
/// creation's start come after every rule, so a rule they would have run out of gas behind still
/// refuses the call.
#[test]
fn test_the_signers_account_and_the_forward_come_before_the_address_and_the_balance() {
    let new_account = entry(GasId::new_account_state_gas());
    let plain = signed(SIGNED, 0);
    let carrying = signed(SIGNED, 1);
    let carrying_two = signed(SIGNED, 2);
    let small = signed(1_000, 0);
    let small_carrying = signed(1_000, 1);
    let data = |deployment: &Deployment| deployment.call_data(LARGE_OVERRIDE);
    let cases = [
        Case {
            rules: "the signer's account the call cannot pay, and an occupied address",
            db: occupied(system_db(), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), 1_000),
            answer: Answer::OutOfGas,
        },
        Case {
            rules: "the signer's account the call cannot pay, and a value it cannot fund",
            db: system_db(),
            data: data(&carrying),
            gas_limit: leaving(&data(&carrying), 1_000),
            answer: Answer::OutOfGas,
        },
        Case {
            rules: "a forward below the signed gas limit, and an occupied address",
            db: occupied(funded(system_db(), &plain), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), SIGNED - 1),
            answer: too_low(SIGNED - 1),
        },
        Case {
            rules: "a forward the signer's account takes below the signed gas limit, and an \
                    occupied address",
            db: occupied(system_db(), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), new_account + SIGNED - 1),
            answer: too_low(SIGNED - 1),
        },
        Case {
            rules: "a forward below the signed gas limit, and a value the signer cannot fund",
            db: funded(system_db(), &carrying_two),
            data: data(&carrying_two),
            gas_limit: leaving(&data(&carrying_two), SIGNED - 1),
            answer: too_low(SIGNED - 1),
        },
        Case {
            rules:
                "an occupied address, and a forward the creation's charges would take below the \
                    signed gas limit",
            db: occupied(funded(system_db(), &plain), &plain),
            data: data(&plain),
            gas_limit: leaving(&data(&plain), SIGNED),
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
            gas_limit: leaving(&data(&carrying), new_account + SIGNED),
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
    cases.into_iter().for_each(Case::check);
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
/// the call, and ran such a deployment in its sandbox on what the call had.
#[test]
fn test_the_creations_charges_can_refuse_a_call_no_rule_refuses() {
    let plain = signed(SIGNED, 0);
    let small = signed(1_000, 0);
    let data = |deployment: &Deployment| deployment.call_data(LARGE_OVERRIDE);
    let charges = entry(GasId::create_state_gas()) + 2 * record();
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
            gas_limit: leaving(&data(&plain), SIGNED),
            answer: too_low(SIGNED - charges),
        },
    ];
    cases.into_iter().for_each(Case::check);
}
