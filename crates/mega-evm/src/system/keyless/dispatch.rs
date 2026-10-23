//! The dispatch of `KeylessDeploy` calls: which calls are keyless deployments, what such a call
//! pays before its creation starts, the rules it is held to, and the creation it starts.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::boxed::Box;

use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::{
    context::{Cfg, ContextError, ContextTr, JournalTr},
    context_interface::{
        cfg::{
            gas::{PoolAmounts, StartCharge},
            GasId, StateGasCharge, StateGasSite,
        },
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
    charge_write_records_history, synthetic_call_result,
    system::{
        intercept::peek_selector,
        keyless::{
            calculate_keyless_deploy_address, decode_keyless_tx, encode_error_result,
            recover_signer, IKeylessDeploy, KeylessDeployError, KEYLESS_DEPLOY_ADDRESS,
            KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_CODE_HASH, KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
    },
    untouched_call_gas, write_record_history_gas, ExternalEnvTypes, MegaContext,
    VolatileDataAccess,
};

/// What a `keylessDeploy` call's creation is settled into the call with, kept on the context while
/// the creation runs.
#[derive(Debug)]
pub(crate) struct Started {
    /// The address the creation deploys at.
    pub(super) deploy_address: Address,
    /// The signer, and its nonce before the creation's start bumped it.
    pub(super) signer: Address,
    pub(super) signer_nonce: u64,
    /// Whether the call was charged for the signer's account, which the creation's start creates:
    /// not when the signer had one. The charge is priced again where it is given back
    /// ([`signer_account`]).
    pub(super) signer_account_charged: bool,
    /// Whether the deposit of empty code at the deploy address was refused, which reverts the
    /// creation ([`refuses_empty_deposit`](super::settle::refuses_empty_deposit)).
    pub(super) empty_deposit_refused: bool,
}

/// Whether `frame_init` starts a `keylessDeploy` call the dispatch takes: a `CALL` at depth 0 to
/// [`KEYLESS_DEPLOY_ADDRESS`] carrying the `keylessDeploy` selector.
///
/// Only a transaction deploys, so the deployment's answer is never something an inner caller can
/// read off its own return data. A call a contract makes, and a selector the contract does not
/// declare, run the deployed bytecode: a `keylessDeploy` call reaches the method body and its
/// `NotIntercepted()`, and an undeclared selector finds no function and no fallback, so it reverts
/// with empty data.
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
/// made by hand, its first by [`start`]. The dispatch is by address and selector, whatever the
/// address holds, so where the state holds no contract code the frame is built on the contract's
/// own. A chain Satin runs on holds it, as Rex2 deployed it, and Satin's activation block checks
/// it.
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
    ctx.detention.hold(gas.tracker_mut());
    let Err(refusal) = prepare(ctx, inputs, &mut gas)? else {
        unreachable!("a call carrying value is refused at rule 2")
    };
    Ok(Some(refusal.answer(inputs, gas)))
}

/// The first action of a `keylessDeploy` call's frame, which runs no instruction: charges its gas
/// and holds it to the rules, in the order below, then starts the creation it stands for. It is
/// made once revm built the frame and gas detention held it for its first run: a refusal ends the
/// frame before it runs, and the creation is set on the frame's interpreter, which returns it when
/// the frame runs. The call's answer is made when the creation returned into it
/// ([`answer`](super::settle::answer)).
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
/// 11. The deploy address holds no code (`ContractAlreadyExists()`), read without its code, so the
///     address is in the transaction's state and in a witness without its bytecode.
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
/// (`CreateScheme::Custom`), as the call's child, on its forward and on its share of the reservoir
/// the call's charges left and of each of the call's pools, from which the call drew nothing.
/// The call's frame runs as the signer, whose nonce the creation's start writes; the record of
/// that nonce is the call's, counted once the creation returned
/// ([`settle`](super::settle::settle)). The call's journal checkpoint was taken when revm built its
/// frame, so a call that is stopped takes that write back with the rest.
pub(crate) fn start<DB: Database, ExtEnvs: ExternalEnvTypes>(
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
    // The creation's shares of the call's reservoir and pools are taken out of the call, as a
    // `CREATE` opcode's are: the creation's return adds what it left back to what the call kept.
    let shares = gas.tracker_mut().forward_to_child(ctx.gas_params());
    let mut create = CreateInputs::new(
        deployment.signer,
        CreateScheme::Custom { address: deployment.deploy_address },
        deployment.value,
        deployment.init_code,
        deployment.gas_limit,
        shares.reservoir,
    );
    create.set_charged_create_state_gas(deployment.charged_create_state_gas);
    create.set_charged_state_gas_address(deployment.deploy_address);
    create
        .set_charged_start(StartCharge::new(deployment.on_frame_record_charge, PoolAmounts::ZERO));
    create.set_pool_shares(shares.pools);
    ctx.keyless_creation = Some(Started {
        deploy_address: deployment.deploy_address,
        signer: deployment.signer,
        signer_nonce: deployment.signer_nonce,
        signer_account_charged: deployment.signer_account_charged,
        empty_deposit_refused: false,
    });
    Ok(InterpreterAction::NewFrame(FrameInput::Create(Box::new(create))))
}

/// A creation the call passed every rule for and paid the start of.
struct Deployment {
    signer: Address,
    signer_nonce: u64,
    /// Whether the call was charged for the signer's account: the signer has none.
    signer_account_charged: bool,
    deploy_address: Address,
    value: U256,
    init_code: Bytes,
    /// The gas forwarded to the creation, already taken off the call's gas.
    gas_limit: u64,
    /// Whether the call was charged the created account.
    charged_create_state_gas: bool,
    /// What the call paid for the record the creation's start makes on the creation, the created
    /// account's: zero when the transaction pays no history. The signer's nonce record, which the
    /// call paid for too, is the call's own ([`signer_record_charge`]).
    on_frame_record_charge: u64,
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
    let journal = ctx.journal_mut();
    let signer_info = if checks_code {
        journal.load_account_with_code(signer)?
    } else {
        journal.load_account(signer)?
    }
    .data
    .info
    .clone();
    // A signer that is the block beneficiary: the rules read the beneficiary's account through
    // the journal, where the Host marks nothing, and the creation runs for it as a `CREATE` runs
    // in a frame of it, which a read of the account started. The call is the transaction's own
    // frame, at depth 0.
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
    let signer_account_charged = signer_info.is_empty();
    if signer_account_charged && !gas.record_state_cost(state_gas(ctx, signer_account(signer))?) {
        return Ok(Err(Refusal::OutOfGas));
    }
    forward!();
    if ctx.journal_mut().load_account(deploy_address)?.data.info.code_hash != KECCAK_EMPTY {
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
    let records = ctx.accounts().creation_records(signer, Some(deploy_address), tx.value);
    let on_frame_record_charge = if ctx.prices_history() {
        let Some((on_frame, _)) = charge_write_records_history(
            gas.tracker_mut(),
            records.on_frame().added,
            u64::from(records.creator),
        ) else {
            return Ok(Err(Refusal::OutOfGas));
        };
        on_frame
    } else {
        0
    };

    // The forward is not the call's own work: it draws the part detention withholds first, as a
    // `CREATE` opcode's forward does.
    let gas_limit = forward!();
    let forwarded = gas.record_withheld_first_cost(gas_limit);
    debug_assert!(forwarded, "the forward is capped to what the call has left");

    Ok(Ok(Deployment {
        signer,
        signer_nonce: signer_info.nonce,
        signer_account_charged,
        deploy_address,
        value: tx.value,
        init_code: tx.input.clone(),
        gas_limit,
        charged_create_state_gas,
        on_frame_record_charge,
    }))
}

/// The state gas charge for the account of `signer`, which the creation's start creates when the
/// signer has none: one new account, at the signer's account. The call is charged it before the
/// creation starts, and given it back, priced again at the same site, when the start never
/// created the account; the price is the same, since a bucket's multiplier is read once per
/// transaction.
pub(super) fn signer_account(signer: Address) -> StateGasCharge {
    StateGasCharge::one(GasId::new_account_state_gas(), StateGasSite::account(signer))
}

/// The history the call paid for the signer's nonce record when the creation's start makes one —
/// the bump changes an account the transaction had not changed — and the transaction pays
/// history: one write record's. It is given back, priced again, when the record goes
/// ([`take_back_bump`](super::settle)).
pub(super) fn signer_record_charge<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
) -> u64 {
    if !ctx.prices_history() {
        return 0;
    }
    // The call charged this price, so it has one.
    write_record_history_gas(1).unwrap_or_default()
}

/// Prices `charge` through the SALT pricing hook. A failed lookup is the error the hook recorded,
/// which fails the transaction.
pub(super) fn state_gas<DB: Database, ExtEnvs: ExternalEnvTypes>(
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
