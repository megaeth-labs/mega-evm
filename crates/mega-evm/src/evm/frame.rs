//! Frame results built without running a frame.
//!
//! `MegaETH` answers some frames itself: the depth guard, a transaction stopped by a limit, the
//! system contract interceptors, a `keylessDeploy` call carrying value, and a precompile call gas
//! detention answers from its price without running it. Such a result goes back to the caller like
//! any frame's, through revm's own frame return, which settles it as a frame revm ran:
//!
//! 1. an upfront state-gas charge the calling opcode made for the frame (a new account, a creation)
//!    is priced for refund when the frame failed;
//! 2. the frame's gas merges into the caller's: unused regular gas back on success and revert, and
//!    the frame's reservoir in place of the caller's;
//! 3. the refund is refilled into the caller's reservoir;
//! 4. revm settles the frame's start: the engine gives the caller back the part of its reservoir it
//!    kept, and its start ledger merges the frame's budgets into its caller's and gives back the
//!    start charge — what its caller paid for the records its start makes — when it failed, by the
//!    result's status, as for a frame revm ran.
//!
//! The contract for a synthetic result is that it settles exactly like revm's own: its gas carries
//! the reservoir the frame inherited ([`untouched_call_gas`], [`untouched_create_gas`]), never
//! `Gas::new(limit)`, which would carry no reservoir, billing the sender for the whole reservoir;
//! its gas gives back where the execution's configuration says, as the gas of every frame revm
//! builds does: into the reservoir alone under Satin's give-backs, as EIP-8037 has it under the
//! neutral configuration's (`Cfg::reservoir_only_give_backs`), so an inspector that charges and
//! gives back on the result's gas sees the same rule whoever built the result; and it carries the
//! calling opcode's upfront-charge flags, so step 1 refunds them. The budgets and the start charge
//! are the start ledger's, which the engine's own answers settle on by status, so nothing of them
//! is carried. [`synthetic_frame_result`] builds one.

use alloy_primitives::Bytes;
use revm::{
    context::Cfg,
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult, OutcomeMark,
    },
};

/// The gas of a frame built under `cfg` with `limit` of regular gas unspent and `reservoir`: its
/// give-backs go where the configuration says (`Cfg::reservoir_only_give_backs`), as they do for
/// the gas revm builds for every frame.
#[inline]
pub(crate) fn frame_gas(cfg: &impl Cfg, limit: u64, reservoir: u64) -> Gas {
    let mut gas = Gas::new_with_regular_gas_and_reservoir(limit, reservoir);
    gas.set_reservoir_only_give_backs(cfg.reservoir_only_give_backs());
    gas
}

/// The gas of a call result built under `cfg` without running the callee: the call's whole gas
/// limit unspent, the reservoir the call inherited, and the configuration's give-back mode.
#[inline]
pub fn untouched_call_gas(cfg: &impl Cfg, inputs: &CallInputs) -> Gas {
    frame_gas(cfg, inputs.gas_limit, inputs.reservoir)
}

/// The gas of a creation result built under `cfg` without running the init code: its whole gas
/// limit unspent, the reservoir the creation inherited, and the configuration's give-back mode.
#[inline]
pub fn untouched_create_gas(cfg: &impl Cfg, inputs: &CreateInputs) -> Gas {
    frame_gas(cfg, inputs.gas_limit(), inputs.reservoir())
}

/// A result for the frame `input` would start, built under `cfg` without running it: `result`
/// with `output`, the frame's gas untouched, the inherited reservoir carried, the configuration's
/// give-back mode, and the calling opcode's upfront state-gas flags, as revm builds the results
/// of the frames it answers itself.
pub fn synthetic_frame_result(
    cfg: &impl Cfg,
    input: &FrameInput,
    result: InstructionResult,
    output: Bytes,
) -> FrameResult {
    match input {
        FrameInput::Call(inputs) => synthetic_call_result(cfg, inputs, result, output),
        FrameInput::Create(inputs) => FrameResult::Create(CreateOutcome {
            result: InterpreterResult::new(result, output, untouched_create_gas(cfg, inputs)),
            address: None,
            charged_create_state_gas: inputs.charged_create_state_gas(),
            charged_state_gas_address: inputs.charged_state_gas_address(),
            start_mark: OutcomeMark::NONE,
        }),
        FrameInput::Empty => unreachable!("a frame input always names a call or a creation"),
    }
}

/// A result for the call `inputs` would start, built under `cfg` without running it, as
/// [`synthetic_frame_result`] builds one: for a mechanism that already holds the call's inputs
/// (a system contract interceptor).
pub fn synthetic_call_result(
    cfg: &impl Cfg,
    inputs: &CallInputs,
    result: InstructionResult,
    output: Bytes,
) -> FrameResult {
    FrameResult::Call(CallOutcome {
        result: InterpreterResult::new(result, output, untouched_call_gas(cfg, inputs)),
        memory_offset: inputs.return_memory_offset.clone(),
        was_precompile_called: false,
        precompile_call_logs: Default::default(),
        charged_new_account_state_gas: inputs.charged_new_account_state_gas,
        charged_state_gas_address: inputs.target_address,
        start_mark: OutcomeMark::NONE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, U256};
    use revm::{
        context::CfgEnv,
        interpreter::{CallInput, CallScheme, CallValue},
    };

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

    /// A configuration whose give-backs credit the reservoir alone when `reservoir_only` is set,
    /// as Satin's do, and as EIP-8037 has them otherwise.
    fn cfg(reservoir_only: bool) -> CfgEnv {
        let mut cfg = CfgEnv::default();
        cfg.reservoir_only_give_backs = reservoir_only;
        cfg
    }

    /// The untouched gas is the gas limit unspent and the inherited reservoir, not a fresh
    /// `Gas::new(limit)`.
    #[test]
    fn test_untouched_call_gas_carries_the_reservoir() {
        let gas = untouched_call_gas(&cfg(false), &call_inputs(50_000, 7_000, false));
        assert_eq!(gas.limit(), 50_000);
        assert_eq!(gas.remaining(), 50_000);
        assert_eq!(gas.reservoir(), 7_000);
        assert_ne!(gas, Gas::new(50_000));
    }

    /// The gas of every synthetic result, a call's and a creation's, gives back as the
    /// configuration it is built under says: a history charge regular gas paid comes back into
    /// the reservoir alone under give-backs to the reservoir alone, and into regular gas under
    /// EIP-8037's own.
    #[test]
    fn test_untouched_gas_gives_back_as_the_configuration_says() {
        let create = CreateInputs::new(
            Address::ZERO,
            revm::interpreter::CreateScheme::Create,
            U256::ZERO,
            Bytes::new(),
            50_000,
            0,
        );
        for reservoir_only in [true, false] {
            let cfg = cfg(reservoir_only);
            let call = FrameInput::Call(Box::new(call_inputs(50_000, 0, false)));
            let results = [
                untouched_call_gas(&cfg, &call_inputs(50_000, 0, false)),
                untouched_create_gas(&cfg, &create),
                *synthetic_frame_result(&cfg, &call, InstructionResult::Stop, Bytes::new()).gas(),
                *synthetic_frame_result(
                    &cfg,
                    &FrameInput::Create(Box::new(create.clone())),
                    InstructionResult::Stop,
                    Bytes::new(),
                )
                .gas(),
            ];
            for mut gas in results {
                assert_eq!(gas.reservoir_only_give_backs(), reservoir_only);
                assert!(gas.record_history_cost(1_000), "regular gas pays it: no reservoir");
                gas.refill_history(1_000);
                let expected = if reservoir_only { (49_000, 1_000) } else { (50_000, 0) };
                assert_eq!((gas.remaining(), gas.reservoir()), expected, "{reservoir_only}");
            }
        }
    }

    /// A synthetic call result carries the return range, the untouched gas and the upfront
    /// charge flags of its inputs.
    #[test]
    fn test_synthetic_call_result_carries_the_inputs() {
        let input = FrameInput::Call(Box::new(call_inputs(9_000, 11, true)));
        let FrameResult::Call(outcome) = synthetic_frame_result(
            &cfg(false),
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
