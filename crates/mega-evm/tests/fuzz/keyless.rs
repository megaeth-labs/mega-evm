//! Keyless deployments held to their rules.
//!
//! Among every shape a transaction can take, a bounded run draws too few keyless deployments to
//! reach each rule, so this property draws nothing else: a `keylessDeploy` call over a signer and
//! a deploy address the generators set up — the signer's nonce, funds and code, a contract, a
//! balance or a nonce at the deploy address — carrying a transaction that may be encoded as the
//! rules want it or not, signed at a nonce and for a gas limit that may or may not pass.
//!
//! The rules are restated here from the generator's own fields, in the order the spec gives them,
//! so the property holds the engine to the order as well as to each rule.

use std::{collections::BTreeMap, sync::Mutex};

use alloy_primitives::{Address, KECCAK256_EMPTY, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{system::keyless::IKeylessDeploy, MegaLimitExceeded};
use revm::context::result::{ExecutionResult, HaltReason};

use crate::{
    gen::{
        case::{keyless_case, Case},
        tx::{DeployAddress, Encoding, Override, Shape, SignerCode},
        Value,
    },
    harness::{check, fail, print_tally, prop_check, prop_eq},
    render::render_outcome,
};

/// The number of keyless cases the property runs in the bounded mode.
const CASES: u32 = 512;

/// The contract's errors, by selector.
fn error_name(data: &[u8]) -> &'static str {
    macro_rules! names {
        ($($error:ident),* $(,)?) => {
            match data.get(..4) {
                $(Some(selector) if selector == IKeylessDeploy::$error::SELECTOR => {
                    stringify!($error)
                })*
                Some(selector) if selector == MegaLimitExceeded::SELECTOR => "MegaLimitExceeded",
                _ => "an unknown error",
            }
        };
    }
    names!(
        MalformedEncoding,
        NotContractCreation,
        NotPreEIP155,
        NonZeroTxNonce,
        NoEtherTransfer,
        InvalidSignature,
        InsufficientBalance,
        ContractAlreadyExists,
        SignerNonceTooHigh,
        ExecutionReverted,
        ExecutionHalted,
        ParentBudgetExceeded,
        EmptyCodeDeployed,
        NoContractCreated,
        AddressMismatch,
        GasLimitTooLow,
        InsufficientComputeGas,
        InitCodeTooLarge,
        SignerHasCode,
        InternalError,
        InvalidTransaction,
        NotIntercepted,
    )
}

/// The errors the spec says no deployment produces.
const NEVER_PRODUCED: [&str; 7] = [
    "ParentBudgetExceeded",
    "InvalidTransaction",
    "InsufficientComputeGas",
    "InternalError",
    "AddressMismatch",
    "NoContractCreated",
    "NotIntercepted",
];

/// What the generator's fields say of a keyless call, rule by rule.
struct Rules {
    /// The first rule that refuses the call whatever gas it has, in the spec's order: the call's
    /// value, the encoding, the signed nonce, the override against the signed gas limit, the
    /// signature, the signer's nonce, the signer's code. `None` when each admits it.
    refused: Option<&'static str>,
    /// Whether the deploy address holds a contract, which refuses the call once the rules above
    /// and the gas admit it.
    exists: bool,
    /// Whether the signer cannot fund the carried value, which refuses it after that.
    underfunded: bool,
}

fn rules(case: &Case) -> Rules {
    let Shape::Keyless {
        encoding,
        signed_nonce,
        signed_value,
        gas_override,
        signer,
        deploy_address,
        ..
    } = &case.tx.shape
    else {
        unreachable!("a keyless case")
    };
    let refused = if case.tx.value != Value::Zero {
        Some("NoEtherTransfer")
    } else if matches!(encoding, Encoding::Truncated | Encoding::Trailing) {
        Some("MalformedEncoding")
    } else if *encoding == Encoding::NotCreation {
        Some("NotContractCreation")
    } else if *encoding == Encoding::Eip155 {
        Some("NotPreEIP155")
    } else if *signed_nonce != 0 {
        Some("NonZeroTxNonce")
    } else if matches!(gas_override, Override::Zero | Override::Short) {
        Some("GasLimitTooLow")
    } else if *encoding == Encoding::BadSignature {
        Some("InvalidSignature")
    } else if signer.nonce > 1 {
        Some("SignerNonceTooHigh")
    } else if signer.code == SignerCode::Plain {
        Some("SignerHasCode")
    } else {
        None
    };
    let balance = if signer.funded { U256::from(10u64.pow(18)) } else { U256::ZERO };
    Rules {
        refused,
        exists: *deploy_address == DeployAddress::Code,
        underfunded: balance < signed_value.wei(),
    }
}

/// A `keylessDeploy` call follows its rules, in their order, and settles its signer:
///
/// - a call a rule refuses whatever its gas reverts with that rule's error, the first in the spec's
///   order, unless it ran out of gas before or a limit stopped the transaction;
/// - a call those rules admit reverts only with `GasLimitTooLow`, when what it has left no longer
///   covers the signed gas limit, with `ContractAlreadyExists` when the deploy address holds a
///   contract, with `InsufficientBalance` when the signer cannot fund the carried value, or with
///   the stop of its own frame budget;
/// - a call that succeeds started its creation, so no rule refused it, and answers the deploy
///   address with no error when the address holds code afterwards, and otherwise the zero address
///   with `ExecutionReverted`, `ExecutionHalted` or `EmptyCodeDeployed`, the address then holding
///   the code it held before;
/// - the signer's nonce is 1 after a call that succeeds, from 0 by the creation's bump and from 1
///   by taking the bump back, or above when the signer delegates, since its code may then spend
///   nonces of its own; and it is unchanged after a call that does not succeed;
/// - the errors the spec says no deployment produces never appear, in the revert data or in the
///   answer.
///
/// The tally of outcomes is printed, so a run shows which rules the generators reached.
#[test]
fn test_property_a_keyless_deployment_follows_its_rules() {
    let tally: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());
    check("keyless_deployment_follows_its_rules", CASES, keyless_case, |case| {
        let count = |class: String| *tally.lock().unwrap().entry(class).or_default() += 1;
        let Shape::Keyless { signer, .. } = &case.tx.shape else { unreachable!("a keyless case") };
        let Ok(outcome) = case.execute() else {
            count("the transaction is refused".to_string());
            return Ok(());
        };
        let rendered = render_outcome(&outcome);
        let rules = rules(case);
        let deployment = case.tx.deployment().expect("a keyless case");
        let nonce_of = |address: Option<Address>| {
            address.and_then(|address| outcome.state.get(&address)).map(|a| a.info.nonce)
        };
        // The code the deploy address holds once the transaction's state is committed: an account
        // its constructor destroyed is removed then, whatever code the creation deposited.
        let code_at_deploy_address = deployment
            .deploy_address()
            .and_then(|address| outcome.state.get(&address))
            .map(|account| match account.is_selfdestructed() {
                true => KECCAK256_EMPTY,
                false => account.info.code_hash,
            });

        if !outcome.result.is_success() {
            prop_check!(
                nonce_of(deployment.signer).is_none_or(|nonce| nonce == u64::from(signer.nonce)),
                "a call that does not succeed leaves the signer's nonce\n{rendered}"
            );
        }
        match &outcome.result {
            ExecutionResult::Halt { reason, .. } => {
                prop_check!(
                    matches!(reason, mega_evm::MegaHaltReason::Base(HaltReason::OutOfGas(_))),
                    "a keyless call halts only out of gas: {reason:?}\n{rendered}"
                );
                count("the call runs out of gas".to_string());
            }
            ExecutionResult::Revert { .. } if outcome.limit_exceeded.is_some() => {
                count("a limit stops the transaction".to_string());
            }
            ExecutionResult::Revert { output, .. } => {
                let error = error_name(output);
                prop_check!(
                    !NEVER_PRODUCED.contains(&error) && error != "an unknown error",
                    "the call reverts with {error}\n{rendered}"
                );
                match rules.refused {
                    Some(rule) => prop_eq!(
                        error,
                        rule,
                        "the call is refused by the first rule in order\n{rendered}"
                    ),
                    None => prop_check!(
                        error == "GasLimitTooLow" ||
                            error == "MegaLimitExceeded" ||
                            (error == "ContractAlreadyExists" && rules.exists) ||
                            (error == "InsufficientBalance" && rules.underfunded),
                        "the call is refused {error}, which no rule gives it\n{rendered}"
                    ),
                }
                count(format!("refused {error}"));
            }
            ExecutionResult::Success { output, .. } => {
                prop_eq!(
                    (rules.refused, rules.exists, rules.underfunded),
                    (None, false, false),
                    "a call a rule refuses succeeded\n{rendered}"
                );
                let Ok(answer) =
                    IKeylessDeploy::keylessDeployCall::abi_decode_returns(output.data())
                else {
                    return Err(fail(format!("the answer does not decode\n{rendered}")));
                };
                let deploy_address = deployment.deploy_address().expect("a signer was recovered");
                if answer.deployedAddress == Address::ZERO {
                    let error = error_name(&answer.errorData);
                    prop_check!(
                        matches!(
                            error,
                            "ExecutionReverted" | "ExecutionHalted" | "EmptyCodeDeployed"
                        ),
                        "a failed deployment answers {error}\n{rendered}"
                    );
                    prop_check!(
                        code_at_deploy_address.is_none_or(|hash| hash == KECCAK256_EMPTY),
                        "a failed deployment leaves no code\n{rendered}"
                    );
                    count(format!("the creation fails: {error}"));
                } else {
                    prop_eq!(answer.deployedAddress, deploy_address, "the address\n{rendered}");
                    prop_check!(
                        answer.errorData.is_empty(),
                        "a deployment answers no error\n{rendered}"
                    );
                    prop_check!(
                        code_at_deploy_address.is_some_and(|hash| hash != KECCAK256_EMPTY),
                        "a deployment leaves code at the deploy address\n{rendered}"
                    );
                    count("deployed".to_string());
                }
                let nonce = nonce_of(deployment.signer);
                if matches!(signer.code, SignerCode::Delegates(_)) {
                    prop_check!(
                        nonce.is_some_and(|nonce| nonce >= 1),
                        "a delegating signer's nonce is at least 1\n{rendered}"
                    );
                    if nonce.is_some_and(|nonce| nonce > 1) {
                        count("  the signer's own code spent a nonce".to_string());
                    }
                } else {
                    prop_eq!(nonce, Some(1), "the signer's nonce is 1\n{rendered}");
                }
                if signer.nonce == 1 && nonce == Some(1) {
                    count("  the creation's bump is taken back from nonce 1".to_string());
                }
            }
        }
        Ok(())
    });
    print_tally("keyless_deployment_follows_its_rules", tally.into_inner().unwrap());
}
