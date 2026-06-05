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
    context::{ContextTr, LocalContextTr},
    handler::FrameResult,
    interpreter::{CallInput, CallInputs, CallOutcome, Gas, InstructionResult, InterpreterResult},
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

/// Reads the first four bytes of a call input without materializing the full payload.
///
/// Returns `None` when the input is shorter than four bytes. For
/// [`CallInput::SharedBuffer`], only the four-byte head slice is borrowed from
/// shared memory — the trailing bytes are never read.
#[inline]
fn peek_selector<CTX: ContextTr>(input: &CallInput, ctx: &CTX) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    match input {
        CallInput::Bytes(bytes) => {
            if bytes.len() < 4 {
                return None;
            }
            out.copy_from_slice(&bytes[..4]);
            Some(out)
        }
        CallInput::SharedBuffer(range) => {
            if range.len() < 4 {
                return None;
            }
            let head = range.start..range.start + 4;
            let slice = ctx.local().shared_memory_buffer_slice(head)?;
            out.copy_from_slice(&slice);
            Some(out)
        }
    }
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
/// - Returning `Some(FrameResult)` produces a **synthetic** result that bypasses
///   `AdditionalLimit::before_frame_init`. The caller only pushes an empty tracking frame for stack
///   alignment; no per-frame gas adjustments (stipend, compute cap, etc.) are applied. If a future
///   interceptor needs full child-frame gas semantics, it must go through the normal frame
///   lifecycle instead of returning a synthetic result.
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
    // The assertion makes the invariant explicit so a future change that returns
    // Some(FrameResult) is caught in debug builds rather than silently ignored.
    if spec.is_enabled(OracleHintInterceptor::ACTIVATION_SPEC) {
        let hint_result = OracleHintInterceptor::intercept(ctx, call_inputs, depth);
        debug_assert!(
            hint_result.is_none(),
            "OracleHintInterceptor must be side-effect only and never short-circuit",
        );
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

        let is_rex5_enabled = ctx.spec.is_enabled(MegaSpecId::REX5);

        // REX5+: zero-gas calls cannot run the canonical Oracle bytecode anyway. Skip the
        // hint forwarding and let the inner frame OOG canonically. Pre-REX5 the
        // interceptor forwarded the hint regardless of `gas_limit`, which let a caller
        // push payloads to the off-chain backend for free while the inner Oracle frame
        // OOG'd. REX5+ closes that side channel.
        if is_rex5_enabled && call_inputs.gas_limit == 0 {
            return None;
        }

        // REX5+: peek the selector first so non-matching dispatch never materializes the
        // (potentially huge) calldata. For `sendHint` the full payload is still needed to
        // decode the `bytes data` argument, but the allocation cost is now paid at most
        // once per legitimate `sendHint` call.
        if is_rex5_enabled {
            let selector = peek_selector(&call_inputs.input, ctx)?;
            if selector != IOracle::sendHintCall::SELECTOR {
                return None;
            }
            let input_bytes = call_inputs.input.bytes(ctx);

            // Meter the materialized payload against the TX data-size budget BEFORE
            // decoding. The host has already paid the materialization cost; charge the
            // raw `input_bytes.len()` so a caller cannot force unmetered host work by
            // appending huge trailing bytes after a valid ABI envelope —
            // `alloy_sol_types::abi_decode` silently accepts trailing junk (pinned by
            // `test_alloy_abi_decode_accepts_selector_plus_trailing_bytes_on_zero_arg_calls`
            // below). This also subsumes the malformed-payload case: bytes are charged
            // whether or not `abi_decode` later succeeds.
            //
            // The hint is a TX-scoped side-channel into the off-chain oracle backend
            // (not consensus state), so we record into the TX intrinsic lane — same as
            // calldata. On overflow: do NOT forward the hint, and do NOT synthesize a
            // result. Returning `None` lets the next `before_frame_init` step observe
            // the freshly-flipped `has_exceeded_limit` and produce the canonical
            // TX-level `OutOfGas` halt via `create_exceeded_limit_result`. The failure
            // shape (and rescued-gas refund) matches every other data-size overflow.
            let within = ctx
                .additional_limit
                .borrow_mut()
                .record_oracle_hint_bytes(input_bytes.len() as u64);
            if !within {
                return None;
            }

            // Trailing junk after a valid ABI envelope is silently dropped by
            // `abi_decode`; a malformed envelope is rejected outright. In either case
            // we have already charged for the host-side materialization above, so we
            // just fall through without forwarding on the rejection path.
            let Ok(call) = IOracle::sendHintCall::abi_decode(&input_bytes) else {
                return None;
            };

            ctx.oracle_env.borrow().on_hint(call_inputs.caller, call.topic, call.data);
            return None;
        }

        // Pre-REX5 path verbatim — preserves replay determinism on stable specs.
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
        if ctx.is_inside_sandbox() || depth != 0 {
            return None;
        }

        if call_inputs.target_address != KEYLESS_DEPLOY_ADDRESS {
            return None;
        }

        // Peek selector before materializing calldata. The full payload is still
        // decoded for the `(bytes, uint64)` args after the selector matches.
        let selector = peek_selector(&call_inputs.input, ctx)?;
        if selector != IKeylessDeploy::keylessDeployCall::SELECTOR {
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

impl AccessControlInterceptor {
    /// Handles `disableVolatileDataAccess()`. Caller is the journal-depth of the frame
    /// that issued the call; see `intercept`'s comment for the mapping rationale.
    ///
    /// Takes `&MegaContext` because the mutation goes through the interior `RefCell`
    /// on `volatile_data_tracker`; the context itself is read immutably.
    fn handle_disable<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        caller_journal_depth: usize,
    ) -> InterceptResult {
        if let Some(result) = reject_non_zero_transfer(call_inputs) {
            return Some(result);
        }
        ctx.volatile_data_tracker.borrow_mut().disable_access(caller_journal_depth);
        Some(FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::new(),
                Gas::new(call_inputs.gas_limit),
            ),
            call_inputs.return_memory_offset.clone(),
        )))
    }

    fn handle_enable<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        caller_journal_depth: usize,
    ) -> InterceptResult {
        if let Some(result) = reject_non_zero_transfer(call_inputs) {
            return Some(result);
        }
        let success = ctx.volatile_data_tracker.borrow_mut().enable_access(caller_journal_depth);
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
        Some(result)
    }

    fn handle_is_disabled<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
        caller_journal_depth: usize,
    ) -> InterceptResult {
        if let Some(result) = reject_non_zero_transfer(call_inputs) {
            return Some(result);
        }
        let disabled =
            ctx.volatile_data_tracker.borrow().volatile_access_disabled(caller_journal_depth);
        let output =
            IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&disabled);
        Some(FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::from(output),
                Gas::new(call_inputs.gas_limit),
            ),
            call_inputs.return_memory_offset.clone(),
        )))
    }
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

        // depth equals the caller's journal depth (because journal.depth = frame.depth + 1,
        // and the caller's frame.depth = frame_init.depth - 1).
        let caller_journal_depth = depth;

        // Admission is selector-match alone; do not add a length check. The historical
        // `abi_decode::<()>` admission accepted selector + arbitrary trailing bytes,
        // and that decision is consensus-visible under stable specs.
        let selector = peek_selector(&call_inputs.input, ctx)?;
        if selector == IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR {
            return Self::handle_disable(ctx, call_inputs, caller_journal_depth);
        }
        if selector == IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR {
            return Self::handle_enable(ctx, call_inputs, caller_journal_depth);
        }
        if selector == IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR {
            return Self::handle_is_disabled(ctx, call_inputs, caller_journal_depth);
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

impl LimitControlInterceptor {
    fn handle_remaining_compute_gas<DB: Database, ExtEnvs: ExternalEnvTypes>(
        ctx: &MegaContext<DB, ExtEnvs>,
        call_inputs: &CallInputs,
    ) -> InterceptResult {
        if let Some(result) = reject_non_zero_transfer(call_inputs) {
            return Some(result);
        }
        let remaining = ctx.additional_limit.borrow().current_call_remaining_compute_gas();
        let output = IMegaLimitControl::remainingComputeGasCall::abi_encode_returns(&remaining);
        Some(FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::from(output),
                Gas::new(call_inputs.gas_limit),
            ),
            call_inputs.return_memory_offset.clone(),
        )))
    }
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

        let selector = peek_selector(&call_inputs.input, ctx)?;
        if selector == IMegaLimitControl::remainingComputeGasCall::SELECTOR {
            return Self::handle_remaining_compute_gas(ctx, call_inputs);
        }
        // Unknown selector — not intercepted.
        None
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256};
    use alloy_sol_types::SolCall;
    use revm::{
        bytecode::opcode::{CALLCODE, MSTORE, RETURN},
        context::tx::TxEnvBuilder,
    };

    use crate::{
        test_utils::{BytecodeBuilder, MemoryDatabase},
        IMegaAccessControl, IMegaLimitControl, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
        LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
    };

    const REMAINING_COMPUTE_GAS_SELECTOR: [u8; 4] =
        IMegaLimitControl::remainingComputeGasCall::SELECTOR;

    /// Pins the `abi_decode`-accepts-trailing-junk behaviour of `alloy-sol-types`'s
    /// `decode_sequence::<()>` for parameterless system-contract methods. The
    /// interceptor dispatch's selector-only admission relies on this: if a future
    /// alloy upgrade rejects trailing bytes, historical replays would diverge from
    /// the original `abi_decode` admission and break replay determinism on stable
    /// specs.
    #[test]
    fn test_alloy_abi_decode_accepts_selector_plus_trailing_bytes_on_zero_arg_calls() {
        let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
        let exact = selector;
        let with_junk: Vec<u8> = selector.iter().copied().chain(core::iter::once(0xff)).collect();
        let with_pad: Vec<u8> = selector.iter().copied().chain([0u8; 32]).collect();

        assert!(
            IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&exact).is_ok(),
            "exact 4-byte selector must decode",
        );
        assert!(
            IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&with_junk).is_ok(),
            "current alloy-sol-types accepts selector+1B junk for empty-tuple decode; if this \
             fails the pre-REX5 dispatch path's admission rule has changed — re-spec the \
             selector probe before landing",
        );
        assert!(
            IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&with_pad).is_ok(),
            "current alloy-sol-types accepts selector+32B padding for empty-tuple decode; if \
             this fails the pre-REX5 dispatch path's admission rule has changed — re-spec the \
             selector probe before landing",
        );
    }

    /// Unit test for the explicit call-scheme guard in `frame_init`.
    ///
    /// A `CALLCODE` to a system contract address with a recognized selector must NOT be
    /// intercepted.
    /// The scheme guard rejects it before any interceptor sees the call.
    /// The call falls through to on-chain bytecode, which reverts with `NotIntercepted()`,
    /// leaving the `CALLCODE` success flag as 0.
    #[test]
    fn test_callcode_scheme_guard_skips_interception() {
        let code = BytecodeBuilder::default()
            .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
            // CALLCODE(gas, addr, value, argsOffset, argsSize, retOffset, retSize)
            .push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(4_u64) // argsSize (selector length)
            .push_number(0_u64) // argsOffset (selector at memory[0])
            .push_number(0_u64) // value
            .push_address(LIMIT_CONTROL_ADDRESS)
            .push_number(100_000_u64) // gas
            .append(CALLCODE) // success flag (0=fail, 1=success) on stack
            .push_number(0_u64)
            .append(MSTORE)
            .push_number(32_u64)
            .push_number(0_u64)
            .append(RETURN)
            .build();

        let caller = alloy_primitives::address!("0000000000000000000000000000000000300000");
        let contract = alloy_primitives::address!("0000000000000000000000000000000000300001");

        let mut db = MemoryDatabase::default()
            .account_balance(caller, U256::from(1_000_000))
            .account_code(contract, code)
            .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE);

        let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = MegaEvm::new(context);
        let tx = TxEnvBuilder::default()
            .caller(caller)
            .call(contract)
            .gas_limit(100_000_000)
            .build_fill();
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());

        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
        assert!(result.result.is_success(), "outer tx should succeed");

        let output = result.result.output().expect("should have output");
        let success_flag = U256::from_be_slice(output);
        assert_eq!(
            success_flag,
            U256::ZERO,
            "CALLCODE to system contract must not be intercepted — scheme guard must reject it"
        );
    }
}
