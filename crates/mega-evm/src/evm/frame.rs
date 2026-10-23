//! Frame results built without running a frame.
//!
//! `MegaETH` answers some frames itself: the depth guard, a transaction stopped by a limit, the
//! system contract interceptors, a `keylessDeploy` call carrying value, and a precompile call gas
//! detention answers from its price without running it. Such a result goes back to the caller like
//! any frame's, through revm's own frame return, which settles it as a frame revm ran:
//!
//! 1. an upfront state-gas charge the calling opcode made for the frame (a new account, a creation)
//!    is priced for refund when the frame failed;
//! 2. the frame's gas merges into the caller's: unused regular gas back on success and revert, the
//!    frame's reservoir added to what the caller kept, and the frame's pools absorbed into the
//!    caller's;
//! 3. the refund is refilled into the caller's reservoir, and the start charge the frame carries —
//!    what its caller paid for the records its start makes — is given back when it failed.
//!
//! The contract for a synthetic result is that it settles exactly like revm's own: its gas carries
//! the reservoir and the shares of the pools the frame inherited ([`untouched_call_gas`],
//! [`untouched_create_gas`]), never `Gas::new(limit)`, which would carry no reservoir and
//! unlimited pools, billing the sender for the whole reservoir and lifting the caller's budgets;
//! and it carries the calling opcode's upfront-charge flags and start charge, so step 1 and step 3
//! refund them. [`synthetic_frame_result`] builds one.

use alloy_primitives::Bytes;
use revm::{
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult,
    },
};

/// The gas of a call result built without running the callee: the call's whole gas limit
/// unspent, the reservoir the call inherited, and the shares of its caller's pools it was given,
/// undrawn.
#[inline]
pub const fn untouched_call_gas(inputs: &CallInputs) -> Gas {
    let mut gas = Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
    gas.tracker_mut().set_pools_remaining(inputs.pool_shares);
    gas
}

/// The gas of a creation result built without running the init code: its whole gas limit
/// unspent, the reservoir the creation inherited, and the shares of its creator's pools it was
/// given, undrawn.
#[inline]
pub const fn untouched_create_gas(inputs: &CreateInputs) -> Gas {
    let mut gas = Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit(), inputs.reservoir());
    gas.tracker_mut().set_pools_remaining(inputs.pool_shares());
    gas
}

/// A result for the frame `input` would start, built without running it: `result` with
/// `output`, the frame's gas untouched, the inherited reservoir and pool shares carried, and the
/// calling opcode's upfront state-gas flags and start charge, as revm builds the results of the
/// frames it answers itself.
pub fn synthetic_frame_result(
    input: &FrameInput,
    result: InstructionResult,
    output: Bytes,
) -> FrameResult {
    match input {
        FrameInput::Call(inputs) => synthetic_call_result(inputs, result, output),
        FrameInput::Create(inputs) => FrameResult::Create(CreateOutcome {
            result: InterpreterResult::new(result, output, untouched_create_gas(inputs)),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
            charged_state_gas_address: inputs.charged_state_gas_address(),
            charged_start: inputs.charged_start(),
        }),
        FrameInput::Empty => unreachable!("a frame input always names a call or a creation"),
    }
}

/// A result for the call `inputs` would start, built without running it, as
/// [`synthetic_frame_result`] builds one: for a mechanism that already holds the call's inputs
/// (a system contract interceptor).
pub fn synthetic_call_result(
    inputs: &CallInputs,
    result: InstructionResult,
    output: Bytes,
) -> FrameResult {
    FrameResult::Call(CallOutcome {
        result: InterpreterResult::new(result, output, untouched_call_gas(inputs)),
        memory_offset: inputs.return_memory_offset.clone(),
        was_precompile_called: false,
        precompile_call_logs: Default::default(),
        charged_new_account_state_gas: inputs.charged_new_account_state_gas,
        charged_state_gas_address: inputs.target_address,
        charged_start: inputs.charged_start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use revm::interpreter::{CallInput, CallScheme, CallValue, PoolAmounts};

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
            charged_start: Default::default(),
            pool_shares: PoolAmounts::UNLIMITED,
            history_allowance: 0,
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
}
