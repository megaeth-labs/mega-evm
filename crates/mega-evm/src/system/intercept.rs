//! Trait and implementations for system contract call interception in `frame_init`.
//!
//! System contracts are intercepted before normal EVM frame initialization.
//! Each interceptor checks whether the call targets its contract address,
//! decodes the ABI input, and either performs a side-effect (returning `None`
//! to continue normal execution) or returns a synthetic [`FrameResult`] to
//! short-circuit frame creation.

use alloy_evm::Database;
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolError};
use revm::{
    handler::FrameResult,
    interpreter::{CallInputs, CallOutcome, Gas, InstructionResult, InterpreterResult},
};

use crate::{
    sandbox::execute_keyless_deploy_call, ExternalEnvTypes, IKeylessDeploy, IMegaAccessControl,
    IMegaLimitControl, IOracle, MegaContext, MegaSpecId, OracleEnv, ACCESS_CONTROL_ADDRESS,
    DISABLED_BY_PARENT_REVERT_DATA, KEYLESS_DEPLOY_ADDRESS, LIMIT_CONTROL_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};

/// The result of a system contract call interception attempt.
///
/// - `None`: The interceptor did not handle this call. The caller should try the next interceptor
///   or proceed with normal frame initialization.
/// - `Some(FrameResult)`: The interceptor handled the call and produced a synthetic result. The
///   caller should return this as `FrameInitResult::Result` and push an empty frame to keep the
///   limit tracker stack balanced.
pub type InterceptResult = Option<FrameResult>;

alloy_sol_types::sol! {
    /// Shared error used by view/control system contract interceptors when value is sent.
    error NonZeroTransfer();
}

/// Trait for intercepting calls to system contracts during `frame_init`.
///
/// Implementors check whether an incoming call matches their contract address and function
/// selectors, then either perform a side-effect or return a synthetic result.
///
/// # Contract
///
/// - The caller guarantees that `call_inputs` comes from a `FrameInput::Call`. Create frames are
///   never dispatched to interceptors.
/// - When the method returns `Some(FrameResult)`, the caller is responsible for calling
///   `additional_limit.push_empty_frame()` to keep the frame tracker stack balanced.
/// - Returning `Some(FrameResult)` produces a **synthetic** result, not a real child EVM frame. As
///   a consequence, `STORAGE_CALL_STIPEND` is not applied on this short-circuit path. If a future
///   interceptor needs full child-frame gas semantics for a value-bearing call, it must extend
///   stipend handling explicitly instead of assuming it happens automatically.
/// - When the method returns `None`, the caller proceeds as if no interception occurred.
pub trait SystemContractInterceptor<DB: Database, ExtEnvs: ExternalEnvTypes> {
    /// Attempts to intercept a call to a system contract.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The EVM context providing access to all `MegaETH` state.
    /// * `call_inputs` - The call inputs (target address, input data, gas limit, caller, etc.).
    /// * `depth` - The frame depth from `FrameInit::depth`, which equals the caller's journal
    ///   depth.
    fn intercept(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        depth: usize,
    ) -> InterceptResult;
}

/// Dispatches system contract interceptors in order.
///
/// Returns `Some(FrameResult)` if any interceptor handled the call, `None` otherwise.
/// The caller is responsible for calling `push_empty_frame()` when `Some` is returned.
pub fn dispatch_system_contract_interceptors<DB: Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    call_inputs: &CallInputs,
    depth: usize,
) -> InterceptResult {
    let spec = ctx.spec;

    // Oracle Hint (Rex2+) — side-effect only, never returns Some.
    if spec.is_enabled(OracleHintInterceptor::ACTIVATION_SPEC) {
        OracleHintInterceptor::intercept(ctx, call_inputs, depth);
    }

    // Keyless Deploy (Rex2+)
    if spec.is_enabled(KeylessDeployInterceptor::ACTIVATION_SPEC) {
        if let Some(result) = KeylessDeployInterceptor::intercept(ctx, call_inputs, depth) {
            return Some(result);
        }
    }

    // Access Control (Rex4+)
    if spec.is_enabled(AccessControlInterceptor::ACTIVATION_SPEC) {
        if let Some(result) = AccessControlInterceptor::intercept(ctx, call_inputs, depth) {
            return Some(result);
        }
    }

    // MegaLimitControl (Rex4+)
    if spec.is_enabled(LimitControlInterceptor::ACTIVATION_SPEC) {
        if let Some(result) = LimitControlInterceptor::intercept(ctx, call_inputs, depth) {
            return Some(result);
        }
    }

    None
}

/// Returns a synthetic revert result when the call carries non-zero transferred value.
///
/// This is used by view-style system contract interceptors to prevent silently accepting
/// value-bearing calls.
fn reject_non_zero_transfer(call_inputs: &CallInputs) -> InterceptResult {
    if call_inputs.transfer_value().is_some_and(|value| !value.is_zero()) {
        return Some(FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Revert,
                Bytes::copy_from_slice(&NonZeroTransfer::SELECTOR),
                Gas::new(call_inputs.gas_limit),
            ),
            call_inputs.return_memory_offset.clone(),
        )));
    }
    None
}

/// Interceptor for oracle hint calls (`IOracle::sendHint`).
///
/// When a call to the oracle contract matches the `sendHint(bytes32,bytes)` selector, the
/// hint is forwarded to the oracle service backend via `OracleEnv::on_hint`.
/// Execution continues normally (no early return).
#[derive(Debug)]
pub struct OracleHintInterceptor;

impl OracleHintInterceptor {
    /// The minimum spec required for this interceptor to be active.
    pub const ACTIVATION_SPEC: MegaSpecId = MegaSpecId::REX2;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> SystemContractInterceptor<DB, ExtEnvs>
    for OracleHintInterceptor
{
    fn intercept(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        _depth: usize,
    ) -> InterceptResult {
        if call_inputs.target_address != ORACLE_CONTRACT_ADDRESS {
            return None;
        }

        let input_bytes = call_inputs.input.bytes(ctx);
        if let Ok(call) = IOracle::sendHintCall::abi_decode(&input_bytes) {
            ctx.oracle_env.borrow().on_hint(call_inputs.caller, call.topic, call.data);
        }

        // Side-effect only — do not short-circuit.
        None
    }
}

/// Interceptor for keyless deploy calls (`IKeylessDeploy::keylessDeploy`).
///
/// Intercepts top-level calls to the keyless deploy contract, decodes the pre-EIP-155
/// transaction, and executes deployment in a sandbox.
/// Only active when sandbox is not disabled (to prevent infinite recursion) and the call
/// is at depth 0.
#[derive(Debug)]
pub struct KeylessDeployInterceptor;

impl KeylessDeployInterceptor {
    /// The minimum spec required for this interceptor to be active.
    pub const ACTIVATION_SPEC: MegaSpecId = MegaSpecId::REX2;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> SystemContractInterceptor<DB, ExtEnvs>
    for KeylessDeployInterceptor
{
    fn intercept(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        depth: usize,
    ) -> InterceptResult {
        // Only intercept at top-level and when sandbox is not disabled.
        if ctx.is_sandbox_disabled() || depth != 0 {
            return None;
        }

        if call_inputs.target_address != KEYLESS_DEPLOY_ADDRESS {
            return None;
        }

        let input_bytes = call_inputs.input.bytes(ctx);
        let call = IKeylessDeploy::keylessDeployCall::abi_decode(&input_bytes).ok()?;

        Some(execute_keyless_deploy_call(
            ctx,
            call_inputs,
            &call.keylessDeploymentTransaction,
            call.gasLimitOverride,
        ))
    }
}

/// Interceptor for `MegaAccessControl` system contract calls.
///
/// Handles three functions:
/// - `disableVolatileDataAccess()`: activates volatile data access restriction at the caller's
///   depth.
/// - `enableVolatileDataAccess()`: re-enables volatile data access. Reverts with
///   `DisabledByParent()` if a parent frame disabled it.
/// - `isVolatileDataAccessDisabled()`: queries whether volatile data access is disabled at the
///   caller's depth.
#[derive(Debug)]
pub struct AccessControlInterceptor;

impl AccessControlInterceptor {
    /// The minimum spec required for this interceptor to be active.
    pub const ACTIVATION_SPEC: MegaSpecId = MegaSpecId::REX4;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> SystemContractInterceptor<DB, ExtEnvs>
    for AccessControlInterceptor
{
    fn intercept(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        depth: usize,
    ) -> InterceptResult {
        if call_inputs.target_address != ACCESS_CONTROL_ADDRESS {
            return None;
        }

        let input_bytes = call_inputs.input.bytes(ctx);
        // depth equals the caller's journal depth (because journal.depth = frame.depth + 1,
        // and the caller's frame.depth = frame_init.depth - 1).
        let caller_journal_depth = depth;

        // disableVolatileDataAccess()
        if IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&input_bytes).is_ok() {
            if let Some(result) = reject_non_zero_transfer(call_inputs) {
                return Some(result);
            }
            ctx.volatile_data_tracker.borrow_mut().disable_access(caller_journal_depth);

            return Some(FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    Bytes::new(),
                    Gas::new(call_inputs.gas_limit),
                ),
                call_inputs.return_memory_offset.clone(),
            )));
        }

        // enableVolatileDataAccess()
        if IMegaAccessControl::enableVolatileDataAccessCall::abi_decode(&input_bytes).is_ok() {
            if let Some(result) = reject_non_zero_transfer(call_inputs) {
                return Some(result);
            }
            let success =
                ctx.volatile_data_tracker.borrow_mut().enable_access(caller_journal_depth);

            let result = if success {
                FrameResult::Call(CallOutcome::new(
                    InterpreterResult::new(
                        InstructionResult::Return,
                        Bytes::new(),
                        Gas::new(call_inputs.gas_limit),
                    ),
                    call_inputs.return_memory_offset.clone(),
                ))
            } else {
                FrameResult::Call(CallOutcome::new(
                    InterpreterResult::new(
                        InstructionResult::Revert,
                        Bytes::copy_from_slice(&DISABLED_BY_PARENT_REVERT_DATA),
                        Gas::new(call_inputs.gas_limit),
                    ),
                    call_inputs.return_memory_offset.clone(),
                ))
            };
            return Some(result);
        }

        // isVolatileDataAccessDisabled()
        if IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_decode(&input_bytes).is_ok() {
            if let Some(result) = reject_non_zero_transfer(call_inputs) {
                return Some(result);
            }
            let disabled =
                ctx.volatile_data_tracker.borrow().volatile_access_disabled(caller_journal_depth);

            let output =
                IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&disabled);

            return Some(FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    Bytes::from(output),
                    Gas::new(call_inputs.gas_limit),
                ),
                call_inputs.return_memory_offset.clone(),
            )));
        }

        // Unknown selector — not intercepted.
        None
    }
}

/// Interceptor for `MegaLimitControl` system contract calls.
///
/// Handles:
/// - `remainingComputeGas()`: returns remaining compute gas of the current call.
#[derive(Debug)]
pub struct LimitControlInterceptor;

impl LimitControlInterceptor {
    /// The minimum spec required for this interceptor to be active.
    pub const ACTIVATION_SPEC: MegaSpecId = MegaSpecId::REX4;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> SystemContractInterceptor<DB, ExtEnvs>
    for LimitControlInterceptor
{
    fn intercept(
        ctx: &mut MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        _depth: usize,
    ) -> InterceptResult {
        if call_inputs.target_address != LIMIT_CONTROL_ADDRESS {
            return None;
        }

        let input_bytes = call_inputs.input.bytes(ctx);
        if IMegaLimitControl::remainingComputeGasCall::abi_decode(&input_bytes).is_ok() {
            if let Some(result) = reject_non_zero_transfer(call_inputs) {
                return Some(result);
            }
            let remaining = ctx.additional_limit.borrow().current_call_remaining_compute_gas();
            let output = IMegaLimitControl::remainingComputeGasCall::abi_encode_returns(&remaining);

            return Some(FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    Bytes::from(output),
                    Gas::new(call_inputs.gas_limit),
                ),
                call_inputs.return_memory_offset.clone(),
            )));
        }

        // Unknown selector — not intercepted.
        None
    }
}
