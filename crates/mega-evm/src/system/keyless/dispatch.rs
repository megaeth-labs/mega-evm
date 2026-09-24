//! The dispatch of `KeylessDeploy` calls: which calls are keyless deployments, what such a call
//! pays before its creation starts, the rules it is held to, and the creation it is rewritten
//! into.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::ops::Range;
use std::boxed::Box;

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::{
    context::{Cfg, ContextError, ContextTr, JournalTr, Transaction},
    context_interface::{
        cfg::{GasId, StateGasCharge, StateGasSite},
        journaled_state::JournalCheckpoint,
        Host,
    },
    handler::FrameResult,
    interpreter::{
        interpreter_action::FrameInit, CallInputs, CallScheme, CreateInputs, CreateScheme,
        FrameInput, Gas, InstructionResult,
    },
    primitives::KECCAK_EMPTY,
};

use crate::{
    limit::FrameStartRecords,
    synthetic_call_result,
    system::{
        intercept::peek_selector,
        keyless::{
            calculate_keyless_deploy_address, decode_keyless_tx, encode_error_result,
            recover_signer, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
            KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
    },
    untouched_call_gas, write_record_history_gas, ExternalEnvTypes, JournalInspectTr, MegaContext,
};

/// What the keyless rewrite made of a frame about to start.
#[derive(Debug)]
pub(crate) enum Rewrite {
    /// The frame is not a keyless deployment; it starts as it is.
    NotKeyless,
    /// The frame is now the creation the `keylessDeploy` call stands for. The call's lane is
    /// pushed and its [`KeylessCall`] is on the context, waiting for the creation's result.
    Rewritten,
    /// The call is answered without a frame: a rule refused it, or it could not pay for what its
    /// creation's start makes. Nothing was written and no lane was pushed for it.
    Answered(FrameResult),
}

/// A `keylessDeploy` call rewritten into its creation, from the rewrite until the creation's
/// result is settled into it ([`settle`](super::settle)).
///
/// The call is a frame no code runs in: its lane is pushed and its journal checkpoint taken as
/// any frame's, and its gas holds what it paid before its creation started. The creation is its
/// child, and returns into it as a creation returns into the frame whose `CREATE` started it.
#[derive(Debug)]
pub(crate) struct KeylessCall {
    /// The call's gas: the gas and the reservoir the transaction's frame was forwarded, less the
    /// overhead, the upfront charges and the gas forwarded to the creation.
    pub(super) gas: Gas,
    /// The journal checkpoint taken before the creation's start wrote anything.
    pub(super) checkpoint: JournalCheckpoint,
    /// The address the creation deploys at.
    pub(super) deploy_address: Address,
    /// The signer, and its nonce before the creation's start bumped it.
    pub(super) signer: Address,
    pub(super) signer_nonce: u64,
    /// What the call was charged for the signer's account, which the creation's start creates:
    /// zero when the signer had one.
    pub(super) signer_account_charge: u64,
    /// The history the call paid for the signer's nonce record: zero when the signer is the
    /// transaction's sender, which makes no record, or when the transaction pays no history.
    pub(super) signer_record_charge: u64,
    /// Whether the creation moves value out of the signer's account.
    pub(super) moves_value: bool,
    /// The call's return range.
    pub(super) memory_offset: Range<usize>,
    /// Whether the transaction's own start charged the call a new account.
    pub(super) charged_new_account_state_gas: bool,
    /// The history the creation's lane gave back for the records the creation did not keep.
    pub(super) returned_history: u64,
}

/// Turns a `keylessDeploy` call at the transaction's own frame into the creation it stands for,
/// or answers it; any other frame is [`Rewrite::NotKeyless`].
///
/// # What is dispatched
///
/// A `CALL` to [`KEYLESS_DEPLOY_ADDRESS`] whose first four bytes are the `keylessDeploy`
/// selector, at depth 0: only a transaction deploys, so the deployment's answer is never
/// something an inner caller can read off its own return data. A call a contract makes, and a
/// selector the contract does not declare, run the deployed bytecode: a `keylessDeploy` call
/// reaches the method body and its `NotIntercepted()`, and an undeclared selector finds no
/// function and no fallback, so it reverts with empty data. A transaction the latch already
/// stopped is not dispatched either: its frame is answered with the stop.
///
/// # What the call pays, and what it is held to
///
/// In this order, each refusal an answer with the call's gas as it stands:
///
/// 1. [`KEYLESS_DEPLOY_OVERHEAD_GAS`] of regular gas, whatever the deployment does; a call that
///    cannot pay it is answered out of gas.
/// 2. The call carries no value (`NoEtherTransfer()`).
/// 3. Its arguments decode, and so does the transaction they carry: a signed legacy creation, no
///    chain id, no trailing bytes (`MalformedEncoding()`, `NotContractCreation()`,
///    `NotPreEIP155()`), signed at nonce 0 (`NonZeroTxNonce`).
/// 4. The init code is within the configured initcode size limit (`InitCodeTooLarge`).
/// 5. `gasLimitOverride` covers the signed gas limit (`GasLimitTooLow`).
/// 6. The signer can be recovered (`InvalidSignature()`).
/// 7. The signer's nonce is at most 1 (`SignerNonceTooHigh`).
/// 8. Unless EIP-3607 is disabled, the signer has no code other than an EIP-7702 delegation
///    (`SignerHasCode()`).
/// 9. The deploy address holds no code (`ContractAlreadyExists()`), read cold and without its code,
///    so the address is in the transaction's state and in a witness without its bytecode.
/// 10. The signer can fund the transaction's value (`InsufficientBalance()`).
/// 11. What the `CREATE` opcode would charge its frame for the creation's start: the signer's
///     account when the creation's nonce bump is what creates it, the created account when the
///     deploy address is empty — both state gas, priced by the SALT bucket they land in — and the
///     write records of the two accounts, as history. A call that cannot pay is answered out of
///     gas.
/// 12. The gas the creation is forwarded is `gasLimitOverride`, capped to what the call has left,
///     and must still cover the signed gas limit (`GasLimitTooLow`).
///
/// A refusal writes nothing, so the answer takes back every charge but the overhead with it. A
/// database read or a SALT lookup that fails fails the transaction with its cause, as it does at
/// every other site.
///
/// # The creation
///
/// The creation is started as the signer's, at the signer's Nick's-Method address
/// (`CreateScheme::Custom`), at depth 1 below the call: the call's lane is pushed first, as the
/// transaction's own frame's is, and the creation is its child from there on — its share of the
/// data-size and KV budgets, its records, its forward and the reservoir it inherits, the
/// state-gas limit, the latch. The call's journal checkpoint is taken before the creation's
/// start writes the signer's nonce, so a call that is stopped takes that write back with the
/// rest.
#[inline]
pub(crate) fn rewrite<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &mut FrameInit,
) -> Result<Rewrite, ContextError<DB::Error>> {
    match &frame_init.frame_input {
        FrameInput::Call(inputs) if is_dispatched(ctx, inputs, frame_init.depth) => {
            rewrite_dispatched(ctx, frame_init)
        }
        _ => Ok(Rewrite::NotKeyless),
    }
}

/// [`rewrite`] of a call [`is_dispatched`] took. Kept out of line: every frame asks whether it is
/// a keyless deployment, one transaction in a great many is.
#[inline(never)]
fn rewrite_dispatched<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &mut FrameInit,
) -> Result<Rewrite, ContextError<DB::Error>> {
    let FrameInput::Call(inputs) = &frame_init.frame_input else { return Ok(Rewrite::NotKeyless) };
    let mut gas = untouched_call_gas(inputs);
    let deployment = match prepare(ctx, inputs, &mut gas)? {
        Ok(deployment) => deployment,
        Err(refusal) => return Ok(Rewrite::Answered(refusal.answer(inputs, gas))),
    };
    let memory_offset = inputs.return_memory_offset.clone();
    let charged_new_account_state_gas = inputs.charged_new_account_state_gas;

    // The call's own lane, as the transaction's frame pushes it. A call carrying no value makes
    // no record, so its start crosses nothing the body did not.
    let _ = ctx.additional_limit.on_frame_init(&frame_init.frame_input, frame_init.depth);
    ctx.additional_limit.set_frame_creator(deployment.signer);
    ctx.additional_limit.note_caller_state_gas(gas.state_gas_spent());
    if let Some((on_lane, caller)) = deployment.record_charges {
        ctx.additional_limit.stage_frame_charge(deployment.records, on_lane, caller);
    }
    let checkpoint = ctx.journal_mut().checkpoint();

    let mut create = CreateInputs::new(
        deployment.signer,
        CreateScheme::Custom { address: deployment.deploy_address },
        deployment.value,
        deployment.init_code,
        deployment.gas_limit,
        gas.reservoir(),
    );
    create.set_charged_create_state_gas(deployment.charged_create_state_gas);
    create.set_charged_state_gas_address(deployment.deploy_address);
    ctx.keyless_call = Some(KeylessCall {
        gas,
        checkpoint,
        deploy_address: deployment.deploy_address,
        signer: deployment.signer,
        signer_nonce: deployment.signer_nonce,
        signer_account_charge: deployment.signer_account_charge,
        signer_record_charge: deployment.record_charges.map_or(0, |(_, caller)| caller),
        moves_value: !deployment.value.is_zero(),
        memory_offset,
        charged_new_account_state_gas,
        returned_history: 0,
    });
    frame_init.frame_input = FrameInput::Create(Box::new(create));
    frame_init.depth += 1;
    Ok(Rewrite::Rewritten)
}

/// Whether `inputs` is a `keylessDeploy` call the rewrite takes: a `CALL` at `depth` 0 to
/// [`KEYLESS_DEPLOY_ADDRESS`] carrying the `keylessDeploy` selector, in a transaction the latch
/// has not stopped.
///
/// The depth is tested first: every frame below the transaction's own leaves after one
/// comparison.
#[inline]
fn is_dispatched<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
    depth: usize,
) -> bool {
    depth == 0 &&
        inputs.target_address == KEYLESS_DEPLOY_ADDRESS &&
        inputs.scheme == CallScheme::Call &&
        ctx.additional_limit.latched().is_none() &&
        peek_selector(&inputs.input, ctx) == Some(IKeylessDeploy::keylessDeployCall::SELECTOR)
}

/// A creation the call passed every rule for and paid the start of.
struct Deployment {
    signer: Address,
    signer_nonce: u64,
    /// What the call was charged for the signer's account, when the signer has none.
    signer_account_charge: u64,
    deploy_address: Address,
    value: U256,
    init_code: Bytes,
    /// The gas forwarded to the creation, already taken off the call's gas.
    gas_limit: u64,
    /// Whether the call was charged the created account.
    charged_create_state_gas: bool,
    /// The records the creation's start makes.
    records: FrameStartRecords,
    /// What the call paid for them: the created account's, and the signer's. `None` when the
    /// transaction pays no history.
    record_charges: Option<(u64, u64)>,
}

/// Why a call is answered instead of rewritten.
enum Refusal {
    /// A rule refused it: a revert with the rule's error.
    Rule(KeylessDeployError),
    /// It could not pay: an out-of-gas that spends what it has.
    OutOfGas,
}

impl Refusal {
    /// The answer to the call `inputs` start, carrying `gas`: the call's gas as the refusal left
    /// it, with the reservoir it inherited.
    fn answer(self, inputs: &CallInputs, mut gas: Gas) -> FrameResult {
        let (result, output) = match self {
            Self::Rule(error) => (InstructionResult::Revert, encode_error_result(error)),
            Self::OutOfGas => {
                gas.spend_all();
                (InstructionResult::OutOfGas, Bytes::new())
            }
        };
        let mut answer = synthetic_call_result(inputs, result, output);
        *answer.gas_mut() = gas;
        answer
    }
}

/// Holds the call `inputs` to the rules and charges `gas` for what the creation's start makes, in
/// the order [`rewrite`] lists; the deployment to start, or why the call is answered.
fn prepare<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    inputs: &CallInputs,
    gas: &mut Gas,
) -> Result<Result<Deployment, Refusal>, ContextError<DB::Error>> {
    macro_rules! refuse {
        ($error:expr) => {
            return Ok(Err(Refusal::Rule($error)))
        };
    }
    if !gas.record_regular_cost(KEYLESS_DEPLOY_OVERHEAD_GAS) {
        return Ok(Err(Refusal::OutOfGas));
    }
    if !inputs.call_value().is_zero() {
        refuse!(KeylessDeployError::NoEtherTransfer);
    }
    let input = inputs.input.bytes(ctx);
    let Ok(call) = IKeylessDeploy::keylessDeployCall::abi_decode(&input) else {
        refuse!(KeylessDeployError::MalformedEncoding);
    };
    let signed = match decode_keyless_tx(&call.keylessDeploymentTransaction) {
        Ok(signed) => signed,
        Err(error) => refuse!(error),
    };
    let tx = signed.tx();
    if tx.nonce != 0 {
        refuse!(KeylessDeployError::NonZeroTxNonce { tx_nonce: tx.nonce });
    }
    let max = ctx.cfg().max_initcode_size();
    if tx.input.len() > max {
        refuse!(KeylessDeployError::InitCodeTooLarge {
            size: tx.input.len() as u64,
            max: max as u64,
        });
    }
    let override_gas_limit = u64::try_from(call.gasLimitOverride).unwrap_or(u64::MAX);
    if override_gas_limit < tx.gas_limit {
        refuse!(KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: tx.gas_limit,
            provided_gas_limit: override_gas_limit,
        });
    }
    let signer = match recover_signer(&signed) {
        Ok(signer) => signer,
        Err(error) => refuse!(error),
    };
    let deploy_address = calculate_keyless_deploy_address(signer);

    let checks_code = !ctx.cfg().is_eip3607_disabled();
    let signer_info = ctx.journal_mut().inspect_account(signer, checks_code)?.info.clone();
    if signer_info.nonce > 1 {
        refuse!(KeylessDeployError::SignerNonceTooHigh { signer_nonce: signer_info.nonce });
    }
    if checks_code &&
        signer_info.code.as_ref().is_some_and(|code| !code.is_empty() && !code.is_eip7702())
    {
        refuse!(KeylessDeployError::SignerHasCode);
    }
    if ctx.journal_mut().inspect_account_code_hash(deploy_address)? != KECCAK_EMPTY {
        refuse!(KeylessDeployError::ContractAlreadyExists);
    }
    if signer_info.balance < tx.value {
        refuse!(KeylessDeployError::InsufficientBalance);
    }

    // What the `CREATE` opcode charges its frame for the creation's start. The signer's nonce bump
    // is what creates an empty signer's account; a caller the opcode starts from exists already.
    let mut signer_account_charge = 0;
    if signer_info.is_empty() {
        let charge =
            StateGasCharge::one(GasId::new_account_state_gas(), StateGasSite::account(signer));
        signer_account_charge = state_gas(ctx, charge)?;
        if !gas.record_state_cost(signer_account_charge) {
            return Ok(Err(Refusal::OutOfGas));
        }
    }
    let charged_create_state_gas =
        ctx.journal_ref().state.get(&deploy_address).is_none_or(|account| account.info.is_empty());
    if charged_create_state_gas {
        let charge =
            StateGasCharge::one(GasId::create_state_gas(), StateGasSite::account(deploy_address));
        if !gas.record_state_cost(state_gas(ctx, charge)?) {
            return Ok(Err(Refusal::OutOfGas));
        }
    }
    let records = FrameStartRecords { on_lane: 1, caller: signer != ctx.tx().caller() };
    let record_charges = if ctx.prices_history() {
        let (Some(on_lane), Some(caller)) = (
            write_record_history_gas(records.on_lane),
            write_record_history_gas(u64::from(records.caller)),
        ) else {
            return Ok(Err(Refusal::OutOfGas));
        };
        if !on_lane.checked_add(caller).is_some_and(|cost| gas.record_history_cost(cost)) {
            return Ok(Err(Refusal::OutOfGas));
        }
        Some((on_lane, caller))
    } else {
        None
    };

    let gas_limit = override_gas_limit.min(gas.remaining());
    if gas_limit < tx.gas_limit {
        refuse!(KeylessDeployError::GasLimitTooLow {
            tx_gas_limit: tx.gas_limit,
            provided_gas_limit: gas_limit,
        });
    }
    let forwarded = gas.record_regular_cost(gas_limit);
    debug_assert!(forwarded, "the forward is capped to what the call has left");

    Ok(Ok(Deployment {
        signer,
        signer_nonce: signer_info.nonce,
        signer_account_charge,
        deploy_address,
        value: tx.value,
        init_code: tx.input.clone(),
        gas_limit,
        charged_create_state_gas,
        records,
        record_charges,
    }))
}

/// Prices `charge` through the SALT pricing hook. A failed lookup is the error the hook recorded,
/// which fails the transaction.
fn state_gas<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    charge: StateGasCharge,
) -> Result<u64, ContextError<DB::Error>> {
    if let Some(price) = ctx.state_gas_charge(charge) {
        return Ok(price);
    }
    Err(core::mem::replace(ctx.error(), Ok(()))
        .err()
        .unwrap_or_else(|| ContextError::Custom("state gas price lookup failed".into())))
}
