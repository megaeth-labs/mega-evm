//! The dispatch of `KeylessDeploy` calls: which calls are keyless deployments, what such a call
//! pays before its creation starts, the rules it is held to, and the creation it starts.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::boxed::Box;

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::{
    context::{Cfg, ContextError, ContextTr, Transaction},
    context_interface::{
        cfg::{GasId, StateGasCharge, StateGasSite},
        Host,
    },
    handler::{EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallInputs, CallScheme,
        CreateInputs, CreateScheme, FrameInput, Gas, InstructionResult, InterpreterAction,
    },
    primitives::KECCAK_EMPTY,
    state::Bytecode,
};

use crate::{
    limit::FrameStartRecords,
    synthetic_call_result,
    system::{
        intercept::peek_selector,
        keyless::{
            calculate_keyless_deploy_address, decode_keyless_tx, encode_error_result,
            recover_signer, settle, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
            KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_CODE_HASH, KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
    },
    untouched_call_gas, write_record_history_gas, ExternalEnvTypes, JournalInspectTr, MegaContext,
    VolatileDataAccess,
};

/// Where a `keylessDeploy` call's frame stands, from its start to its answer: what its first run
/// and its resume need of each other, kept on the context while the frame is on revm's stack.
#[derive(Debug)]
pub(crate) enum KeylessFrame {
    /// revm built the call's frame, which has not run.
    Built,
    /// The call's first run started its creation, which runs.
    Deploying(Started),
    /// The creation returned into the call, which answers on its resume.
    Returned(settle::Returned),
}

/// What a `keylessDeploy` call's creation is settled into the call with.
#[derive(Debug)]
pub(crate) struct Started {
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
}

/// Whether `frame_init` starts a `keylessDeploy` call the dispatch takes: a `CALL` at depth 0 to
/// [`KEYLESS_DEPLOY_ADDRESS`] carrying the `keylessDeploy` selector.
///
/// Only a transaction deploys, so the deployment's answer is never something an inner caller can
/// read off its own return data. A call a contract makes, and a selector the contract does not
/// declare, run the deployed bytecode: a `keylessDeploy` call reaches the method body and its
/// `NotIntercepted()`, and an undeclared selector finds no function and no fallback, so it reverts
/// with empty data. A transaction the latch already stopped never reaches the dispatch: its frame
/// is answered with the stop before.
///
/// The depth is tested first: every frame below the transaction's own leaves after one
/// comparison.
#[inline]
pub(crate) fn is_dispatched<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    frame_init: &FrameInit,
) -> bool {
    let FrameInput::Call(inputs) = &frame_init.frame_input else { return false };
    frame_init.depth == 0 &&
        inputs.target_address == KEYLESS_DEPLOY_ADDRESS &&
        inputs.scheme == CallScheme::Call &&
        peek_selector(&inputs.input, ctx) == Some(IKeylessDeploy::keylessDeployCall::SELECTOR)
}

/// Readies the frame of a call [`is_dispatched`] took for revm to build, or answers it.
///
/// A call carrying value is answered before revm builds its frame, which would move the value and
/// journal its transfer log: it pays the overhead, and is refused `NoEtherTransfer()` — rules 1
/// and 2 of [`start`] — on its gas as its caller forwarded it, held by gas detention as the frame
/// would be.
///
/// Any other call is built as a call to the contract, whose code it never runs: its actions are
/// made by [`run`]. The dispatch is by address and selector, whatever the address holds, so where
/// the state holds no contract code the frame is built on the contract's own. A chain holds it
/// from the fork that deploys it on, before any transaction.
#[inline(never)]
pub(crate) fn ready<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame_init: &mut FrameInit,
) -> Result<Option<FrameResult>, ContextError<DB::Error>> {
    let FrameInput::Call(inputs) = &mut frame_init.frame_input else { return Ok(None) };
    if inputs.call_value().is_zero() {
        if inputs.known_bytecode.1.is_empty() {
            inputs.known_bytecode =
                (KEYLESS_DEPLOY_CODE_HASH, Bytecode::new_raw(KEYLESS_DEPLOY_CODE));
        }
        return Ok(None);
    }
    let mut gas = untouched_call_gas(inputs);
    ctx.detention.hold(&mut gas);
    let Err(refusal) = prepare(ctx, inputs, &mut gas)? else {
        unreachable!("a call carrying value is refused at rule 2")
    };
    Ok(Some(refusal.answer(inputs, gas)))
}

/// Whether `frame` is a `keylessDeploy` call's, whose actions [`run`] makes in place of an
/// interpreter: the transaction's own frame, while the dispatch keeps its state.
#[inline]
pub(crate) fn runs<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    frame: &EthFrame<EthInterpreter>,
) -> bool {
    frame.depth == 0 && ctx.keyless_frame.is_some()
}

/// The next action of a `keylessDeploy` call's frame, made by hand: on its first run, the refusal
/// or the creation [`start`] makes; on its resume, once its creation returned into it, its answer
/// ([`settle::answer`]).
///
/// A frame with a stop to return — the latch, or its own budget, which the nonce record a failed
/// creation leaves the call can cross — returns it before this runs, as any frame does.
#[inline(never)]
pub(crate) fn run<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &mut EthFrame<EthInterpreter>,
) -> Result<InterpreterAction, ContextError<DB::Error>> {
    match ctx.keyless_frame.take() {
        Some(KeylessFrame::Built) => start(ctx, frame),
        Some(KeylessFrame::Returned(returned)) => {
            Ok(settle::answer(ctx, &returned, frame.interpreter.gas))
        }
        state => unreachable!("a keylessDeploy call runs when built or returned into: {state:?}"),
    }
}

/// The first run of a `keylessDeploy` call's frame: charges its gas and holds it to the rules, in
/// the order below, then starts the creation it stands for.
///
/// # What the call pays, and what it is held to
///
/// In this order, each refusal the frame's return with its gas as it stands:
///
/// 1. [`KEYLESS_DEPLOY_OVERHEAD_GAS`] of regular gas, whatever the deployment does: decoding the
///    transaction and recovering its signer. A call that cannot pay it runs out of gas.
/// 2. The call carries no value (`NoEtherTransfer()`). A call carrying value is refused before revm
///    builds its frame ([`ready`]).
/// 3. Its arguments decode, and so does the transaction they carry: a signed legacy creation, no
///    chain id, no trailing bytes (`MalformedEncoding()`, `NotContractCreation()`,
///    `NotPreEIP155()`), signed at nonce 0 (`NonZeroTxNonce`).
/// 4. The init code is within the configured initcode size limit (`InitCodeTooLarge`).
/// 5. `gasLimitOverride` covers the signed gas limit (`GasLimitTooLow`).
/// 6. The signer can be recovered (`InvalidSignature()`).
/// 7. The signer's nonce is at most 1 (`SignerNonceTooHigh`). A signer that is the block
///    beneficiary makes this read a read of the beneficiary's account, which gas detention caps as
///    it caps a sender that is the beneficiary.
/// 8. Unless EIP-3607 is disabled, the signer has no code other than an EIP-7702 delegation
///    (`SignerHasCode()`).
/// 9. The signer's account, when the creation's nonce bump is what creates it: state gas, priced by
///    the signer's SALT bucket. A call that cannot pay runs out of gas.
/// 10. `gasLimitOverride`, capped to what the call has left, still covers the signed gas limit
///     (`GasLimitTooLow`).
/// 11. The deploy address holds no code (`ContractAlreadyExists()`), read cold and without its
///     code, so the address is in the transaction's state and in a witness without its bytecode.
/// 12. The signer can fund the transaction's value (`InsufficientBalance()`).
/// 13. What the `CREATE` opcode charges its frame for the creation's start, in the opcode's order:
///     its regular gas — the schedule's `create` entry and EIP-3860's cost per word of init code —
///     the created account when the deploy address is empty — state gas, priced by the deploy
///     address's SALT bucket — and the write records of the two accounts, as history. A call that
///     cannot pay runs out of gas.
/// 14. The gas the creation is forwarded is `gasLimitOverride`, capped to what the call has left,
///     and must still cover the signed gas limit (`GasLimitTooLow`).
///
/// The order is the legacy engine's, so a call several rules refuse is refused with the error the
/// legacy engine reported: it charged the signer's account at step 9 and re-checked the forward
/// right after, before the deploy address and the balance. The charges of step 13 are this
/// engine's, made after every rule, and step 14 holds the forward to them.
///
/// A refusal writes nothing, so its return takes back the state and history gas the call was
/// charged; the regular gas it spent stays spent, as a frame's does — the overhead, and, for the
/// refusal at step 14, the `CREATE` opcode's regular gas. A database read or a SALT lookup that
/// fails fails the transaction with its cause, as it does at every other site.
///
/// # The creation
///
/// The creation is started as the signer's, at the signer's Nick's-Method address
/// (`CreateScheme::Custom`), as the call's child: its share of the data-size and KV budgets, its
/// records, its forward and the reservoir it inherits, the state-gas limit, the latch. The call's
/// lane runs as the signer, whose nonce the creation's start writes. The call's journal checkpoint
/// was taken when revm built its frame, so a call that is stopped takes that write back with the
/// rest.
fn start<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    frame: &mut EthFrame<EthInterpreter>,
) -> Result<InterpreterAction, ContextError<DB::Error>> {
    let FrameInput::Call(inputs) = &frame.input else {
        unreachable!("a keylessDeploy call's frame is a call")
    };
    let gas = &mut frame.interpreter.gas;
    let deployment = match prepare(ctx, inputs, gas)? {
        Ok(deployment) => deployment,
        Err(refusal) => {
            let (result, output) = refusal.result(gas);
            return Ok(InterpreterAction::new_return(result, output, *gas));
        }
    };
    ctx.additional_limit.set_frame_creator(deployment.signer);
    if let Some((on_lane, caller)) = deployment.record_charges {
        ctx.additional_limit.stage_frame_charge(deployment.records, on_lane, caller);
    }
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
    ctx.keyless_frame = Some(KeylessFrame::Deploying(Started {
        deploy_address: deployment.deploy_address,
        signer: deployment.signer,
        signer_nonce: deployment.signer_nonce,
        signer_account_charge: deployment.signer_account_charge,
        signer_record_charge: deployment.record_charges.map_or(0, |(_, caller)| caller),
        moves_value: !deployment.value.is_zero(),
    }));
    Ok(InterpreterAction::NewFrame(FrameInput::Create(Box::new(create))))
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

/// Why a call is refused.
enum Refusal {
    /// A rule refused it: a revert with the rule's error.
    Rule(KeylessDeployError),
    /// It could not pay: an out-of-gas that spends what it has.
    OutOfGas,
}

impl Refusal {
    /// What the call returns, on `gas`: a revert with the rule's error, or an out-of-gas, which
    /// spends it all.
    fn result(self, gas: &mut Gas) -> (InstructionResult, Bytes) {
        match self {
            Self::Rule(error) => (InstructionResult::Revert, encode_error_result(error)),
            Self::OutOfGas => {
                gas.spend_all();
                (InstructionResult::OutOfGas, Bytes::new())
            }
        }
    }

    /// The answer to the call `inputs` start, carrying `gas`: the call's gas as the refusal left
    /// it, with the reservoir it inherited.
    fn answer(self, inputs: &CallInputs, mut gas: Gas) -> FrameResult {
        let (result, output) = self.result(&mut gas);
        let mut answer = synthetic_call_result(inputs, result, output);
        *answer.gas_mut() = gas;
        answer
    }
}

/// Holds the call `inputs` to the rules and charges `gas` for what the creation's start makes, in
/// the order [`start`] lists; the deployment to start, or why the call is refused.
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
    // The forward: `gasLimitOverride`, capped to what the call has left, which must still cover
    // the signed gas limit.
    macro_rules! forward {
        () => {{
            let gas_limit = override_gas_limit.min(gas.remaining());
            if gas_limit < tx.gas_limit {
                refuse!(KeylessDeployError::GasLimitTooLow {
                    tx_gas_limit: tx.gas_limit,
                    provided_gas_limit: gas_limit,
                });
            }
            gas_limit
        }};
    }
    let signer = match recover_signer(&signed) {
        Ok(signer) => signer,
        Err(error) => refuse!(error),
    };
    let deploy_address = calculate_keyless_deploy_address(signer);

    let checks_code = !ctx.cfg().is_eip3607_disabled();
    let signer_info = ctx.journal_mut().inspect_account(signer, checks_code)?.info.clone();
    // A signer that is the block beneficiary: the rules read the beneficiary's account through
    // the journal, where the Host marks nothing, and the creation runs for it as a `CREATE` runs
    // in a frame of it, which a read of the account started.
    if signer == ctx.block().beneficiary {
        ctx.detention.read_by_frame(VolatileDataAccess::BENEFICIARY_BALANCE, gas);
    }
    if signer_info.nonce > 1 {
        refuse!(KeylessDeployError::SignerNonceTooHigh { signer_nonce: signer_info.nonce });
    }
    if checks_code &&
        signer_info.code.as_ref().is_some_and(|code| !code.is_empty() && !code.is_eip7702())
    {
        refuse!(KeylessDeployError::SignerHasCode);
    }

    // The signer's nonce bump is what creates an empty signer's account: a caller the `CREATE`
    // opcode starts from exists already. It is charged where the legacy engine charged it, and
    // the forward checked right after, before the deploy address and the balance are.
    let mut signer_account_charge = 0;
    if signer_info.is_empty() {
        let charge =
            StateGasCharge::one(GasId::new_account_state_gas(), StateGasSite::account(signer));
        signer_account_charge = state_gas(ctx, charge)?;
        if !gas.record_state_cost(signer_account_charge) {
            return Ok(Err(Refusal::OutOfGas));
        }
    }
    forward!();
    if ctx.journal_mut().inspect_account_code_hash(deploy_address)? != KECCAK_EMPTY {
        refuse!(KeylessDeployError::ContractAlreadyExists);
    }
    if signer_info.balance < tx.value {
        refuse!(KeylessDeployError::InsufficientBalance);
    }

    // What the `CREATE` opcode charges its frame for the creation's start, in the opcode's order
    // and after every rule: its regular gas, the created account, then the records.
    let params = ctx.cfg().gas_params();
    let regular = params.create_cost().saturating_add(params.initcode_cost(tx.input.len()));
    if !gas.record_regular_cost(regular) {
        return Ok(Err(Refusal::OutOfGas));
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

    // The forward is not the call's own work: it draws the part detention withholds first, as a
    // `CREATE` opcode's forward does.
    let gas_limit = forward!();
    let forwarded = gas.record_withheld_first_cost(gas_limit);
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
