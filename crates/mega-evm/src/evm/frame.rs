//! Frame results built without running a frame, and how they settle.
//!
//! `MegaETH` answers some frames itself: the depth guard, a transaction stopped by a limit, and,
//! when they land, the system contract interceptors, precompile normalisation and native keyless
//! deployment. Such a result goes back to the caller like any frame's, and the caller settles it
//! the way revm settles a frame it ran:
//!
//! 1. an upfront state-gas charge the calling opcode made for the frame (a new account, a creation)
//!    is priced for refund when the frame failed;
//! 2. [`handle_reservoir_remaining_gas`] merges the frame's gas into the caller's: unused regular
//!    gas back on success and revert, the frame's reservoir adopted as the caller's own;
//! 3. the refund is refilled into the caller's reservoir.
//!
//! The contract for a synthetic result is that it settles exactly like revm's own: its gas carries
//! the reservoir the frame inherited ([`untouched_call_gas`], [`with_pools_of`]), never
//! `Gas::new(limit)`, which would carry none and bill the sender for the whole reservoir; and it
//! carries the calling opcode's upfront-charge flags, so step 1 refunds the charge.
//! [`synthetic_frame_result`] builds one; [`settle_frame_result`] is the settlement, for a
//! mechanism that settles a result into a running frame itself.

use alloy_primitives::Bytes;
use revm::{
    context::{result::FromStringError, ContextTr},
    context_interface::{cfg::gas::GasTracker, context::take_error, Host},
    handler::{handle_reservoir_remaining_gas, FrameResult},
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult,
    },
};

/// The gas of a call result built without running the callee: the call's whole gas limit
/// unspent, and the reservoir the call inherited.
#[inline]
pub const fn untouched_call_gas(inputs: &CallInputs) -> Gas {
    Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir)
}

/// The gas of a creation result built without running the init code: its whole gas limit
/// unspent, and the reservoir the creation inherited.
#[inline]
pub fn untouched_create_gas(inputs: &CreateInputs) -> Gas {
    Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit(), inputs.reservoir())
}

/// `gas_limit` of fresh regular gas carrying `source`'s EIP-8037 pools: its reservoir, the state
/// and history gas it charged, and the part of those charges that spilled onto regular gas.
///
/// For a result whose regular gas is rebuilt (a precompile's output normalised to the gas limit
/// the caller forwarded): the caller keeps the frame's state and history charges on success and
/// rolls them back on failure, so a rebuilt `Gas` that dropped them would hand the caller an
/// empty reservoir and charges the frame never made.
#[inline]
pub const fn with_pools_of(gas_limit: u64, source: &Gas) -> Gas {
    let mut gas = Gas::new(gas_limit);
    gas.set_reservoir(source.reservoir());
    gas.set_state_gas_spent(source.state_gas_spent());
    gas.set_state_gas_spilled(source.state_gas_spilled());
    gas.set_history_gas_spent(source.history_gas_spent());
    gas
}

/// A result for the frame `input` would start, built without running it: `result` with
/// `output`, the frame's gas untouched and the inherited reservoir carried, and the calling
/// opcode's upfront state-gas flags, as revm builds the results of the frames it answers itself.
pub fn synthetic_frame_result(
    input: &FrameInput,
    result: InstructionResult,
    output: Bytes,
) -> FrameResult {
    match input {
        FrameInput::Call(inputs) => FrameResult::Call(CallOutcome {
            result: InterpreterResult::new(result, output, untouched_call_gas(inputs)),
            memory_offset: inputs.return_memory_offset.clone(),
            was_precompile_called: false,
            precompile_call_logs: Default::default(),
            charged_new_account_state_gas: inputs.charged_new_account_state_gas,
            charged_state_gas_address: inputs.target_address,
        }),
        FrameInput::Create(inputs) => FrameResult::Create(CreateOutcome {
            result: InterpreterResult::new(result, output, untouched_create_gas(inputs)),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
            charged_state_gas_address: inputs.charged_state_gas_address(),
        }),
        FrameInput::Empty => unreachable!("a frame input always names a call or a creation"),
    }
}

/// Settles `result`'s gas into `caller_gas` exactly as revm's frame return does: the calling
/// opcode's upfront state-gas charge priced for refund, [`handle_reservoir_remaining_gas`], then
/// the refund refilled into the caller's reservoir.
///
/// `ctx` prices the refund through the same hook the charge went through; a failed lookup is
/// the error the hook recorded.
pub fn settle_frame_result<CTX, ERROR>(
    ctx: &mut CTX,
    caller_gas: &mut GasTracker,
    result: &mut FrameResult,
) -> Result<(), ERROR>
where
    CTX: ContextTr + Host,
    ERROR: From<<CTX::Db as revm::Database>::Error> + FromStringError,
{
    take_error::<ERROR, _>(ctx.error())?;
    let refund = match result.refundable_state_gas_charge() {
        Some(charge) => match ctx.state_gas_charge(charge) {
            Some(refund) => Some(refund),
            None => {
                take_error::<ERROR, _>(ctx.error())?;
                return Err(ERROR::from_string("state gas price lookup failed".into()));
            }
        },
        None => None,
    };
    let instruction_result = result.instruction_result();
    handle_reservoir_remaining_gas(instruction_result, caller_gas, result.gas_mut().tracker_mut());
    if let Some(refund) = refund {
        caller_gas.refill_reservoir(refund);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use revm::interpreter::{CallInput, CallScheme, CallValue};

    const TARGET: Address = address!("0000000000000000000000000000000000001234");

    fn call_inputs(gas_limit: u64, reservoir: u64, charged: bool) -> CallInputs {
        CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 3..7,
            gas_limit,
            bytecode_address: TARGET,
            known_bytecode: Default::default(),
            target_address: TARGET,
            caller: Address::ZERO,
            value: CallValue::Transfer(U256::from(1)),
            scheme: CallScheme::Call,
            is_static: false,
            reservoir,
            charged_new_account_state_gas: charged,
        }
    }

    /// The untouched gas is the gas limit unspent and the inherited reservoir, not a fresh
    /// `Gas::new(limit)`.
    #[test]
    fn test_untouched_call_gas_carries_the_reservoir() {
        let gas = untouched_call_gas(&call_inputs(50_000, 7_000, false));
        assert_eq!(gas.limit(), 50_000);
        assert_eq!(gas.remaining(), 50_000);
        assert_eq!(gas.reservoir(), 7_000);
        assert_ne!(gas, Gas::new(50_000));
    }

    /// Rebuilding the regular gas keeps every EIP-8037 pool of the source.
    #[test]
    fn test_with_pools_of_keeps_the_pools() {
        let mut source = Gas::new_with_regular_gas_and_reservoir(100, 1_000);
        assert!(source.record_state_cost(1_050));
        assert!(source.record_history_cost(20));
        let rebuilt = with_pools_of(70, &source);
        assert_eq!(rebuilt.limit(), 70);
        assert_eq!(rebuilt.remaining(), 70);
        assert_eq!(rebuilt.reservoir(), source.reservoir());
        assert_eq!(rebuilt.state_gas_spent(), 1_050);
        assert_eq!(rebuilt.state_gas_spilled(), source.state_gas_spilled());
        assert_eq!(rebuilt.history_gas_spent(), 20);
    }

    /// A synthetic call result carries the return range, the untouched gas and the upfront
    /// charge flags of its inputs.
    #[test]
    fn test_synthetic_call_result_carries_the_inputs() {
        let input = FrameInput::Call(Box::new(call_inputs(9_000, 11, true)));
        let FrameResult::Call(outcome) = synthetic_frame_result(
            &input,
            InstructionResult::CallTooDeep,
            Bytes::from_static(b"x"),
        ) else {
            panic!("a call input gives a call result");
        };
        assert_eq!(outcome.result.result, InstructionResult::CallTooDeep);
        assert_eq!(outcome.result.output, Bytes::from_static(b"x"));
        assert_eq!(outcome.result.gas, Gas::new_with_regular_gas_and_reservoir(9_000, 11));
        assert_eq!(outcome.memory_offset, 3..7);
        assert!(outcome.charged_new_account_state_gas);
        assert_eq!(outcome.charged_state_gas_address, TARGET);
        assert!(!outcome.was_precompile_called);
    }

    /// Settling a failed synthetic call into its caller returns the forwarded gas, keeps the
    /// caller's reservoir, and refunds the new-account charge the calling opcode made, as revm's
    /// frame return does.
    #[test]
    fn test_settle_frame_result_refunds_the_upfront_charge() {
        use revm::{
            context::{CfgEnv, Context},
            context_interface::cfg::{GasId, GasParams},
            database::EmptyDB,
            primitives::hardfork::SpecId,
            MainContext,
        };
        let mut cfg = CfgEnv::new_with_spec(SpecId::AMSTERDAM);
        cfg.enable_amsterdam_eip8037 = true;
        cfg.gas_params = GasParams::new_spec(SpecId::AMSTERDAM);
        let new_account = cfg.gas_params.get(GasId::new_account_state_gas());
        assert!(new_account > 0);
        let mut ctx = Context::mainnet().with_db(EmptyDB::default()).with_cfg(cfg);

        // The caller forwarded 9,000 gas and charged a new account from its 1M reservoir.
        let reservoir = 1_000_000;
        let mut caller = GasTracker::new(100_000, 100_000 - 9_000, reservoir);
        assert!(caller.record_state_cost(new_account));
        let input = FrameInput::Call(Box::new(call_inputs(9_000, caller.reservoir(), true)));
        let mut result =
            synthetic_frame_result(&input, InstructionResult::CallTooDeep, Bytes::new());

        settle_frame_result::<_, revm::context::result::EVMError<core::convert::Infallible>>(
            &mut ctx,
            &mut caller,
            &mut result,
        )
        .unwrap();
        assert_eq!(caller.remaining(), 100_000, "the forwarded gas comes back");
        assert_eq!(caller.reservoir(), reservoir, "the charge is refunded to the reservoir");
        assert_eq!(caller.state_gas_spent(), 0);

        // A successful synthetic result keeps the charge: the account exists.
        let mut caller = GasTracker::new(100_000, 100_000 - 9_000, reservoir);
        assert!(caller.record_state_cost(new_account));
        let input = FrameInput::Call(Box::new(call_inputs(9_000, caller.reservoir(), true)));
        let mut result = synthetic_frame_result(&input, InstructionResult::Stop, Bytes::new());
        settle_frame_result::<_, revm::context::result::EVMError<core::convert::Infallible>>(
            &mut ctx,
            &mut caller,
            &mut result,
        )
        .unwrap();
        assert_eq!(caller.remaining(), 100_000);
        assert_eq!(caller.reservoir(), reservoir - new_account);
        assert_eq!(caller.state_gas_spent(), new_account as i64);
    }
}
