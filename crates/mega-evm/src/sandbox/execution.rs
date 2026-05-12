//! Keyless deploy sandbox execution.
//!
//! Implements Nick's Method deterministic deployment via an isolated sandbox. See the
//! module-level `Spam Protection` section in `sandbox/mod.rs` for the invariants that
//! govern when each path is taken (normal completion / Rex5 preflight reject / Rex5
//! residual-overflow reject / Rex5 sandbox-`validate()` reject).
//!
//! The three Rex5 defense layers referenced by the module doc are implemented here as:
//! `sandbox_runtime_limits` (upfront cap), `sandbox_intrinsic_overflow_error` (preflight),
//! and `merge_and_reject_if_overflow` (post-merge safety net).
//!
//! Rex5 sandbox-`validate()` rejection — the final Mega-side intrinsic / floor gas check
//! inside `MegaHandler::validate` — fires *before* `pre_execution()` debits the signer.
//! In that path the outer keyless-deploy call surfaces as `Revert` with
//! `IKeylessDeploy::InvalidTransaction`. The signer is not charged because no replay
//! barrier (nonce bump or code install) is consumed; the relayer (depth-0 caller) pays
//! for the outer call gas like any other relayer-submitted revert. Pre-Rex5 specs do
//! not return this class of validation error for the same input shape, so the
//! outer-revert path is unreachable on stable specs.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{rc::Rc, vec::Vec};

use alloy_consensus::Transaction as AlloyTransaction;
use alloy_evm::{Database as AlloyDatabase, Evm};
use alloy_primitives::{Address, Bytes, Log, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;
use op_revm::{handler::IsTxError, L1BlockInfo};
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},
        BlockEnv, ContextTr, TxEnv,
    },
    context_interface::Transaction,
    handler::FrameResult,
    interpreter::{CallOutcome, Gas, Host, InstructionResult, InterpreterResult},
    primitives::KECCAK_EMPTY,
    state::EvmState,
    Database as RevmDatabase,
};
use tracing::{error, warn};

use crate::{
    constants, mark_frame_result_as_exceeding_limit, AdditionalLimit, EvmTxRuntimeLimits,
    ExternalEnvTypes, LimitCheck, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, TxRuntimeLimit,
};

use super::{
    state_merge::apply_sandbox_state,
    tx::{calculate_keyless_deploy_address, decode_keyless_tx, recover_signer},
};

use super::{
    error::{encode_error_result, KeylessDeployError},
    state::SandboxDb,
};

/// Executes a keyless deploy call and returns the frame result.
///
/// Implements Nick's Method contract deployment:
///
/// 1. Validates the call (no ether transfer).
/// 2. Decodes the pre-EIP-155 transaction from calldata.
/// 3. Validates the gas limit override against the transaction's gas limit.
/// 4. Recovers the signer and calculates the deploy address.
/// 5. Rex5+: preflights known sandbox intrinsic usage against the parent's remaining resource
///    envelope and reverts with `ParentBudgetExceeded` if it would not fit.
/// 6. Executes contract creation in an isolated sandbox environment.
/// 7. On sandbox completion, either merges state (normal path) or rejects without merging (Rex5
///    overflow safety net).
///
/// Must only be called at `depth == 0` (enforced by `evm/execution.rs`); a wrapping contract
/// must not be able to intercept and revert the charge. See the module-level `Spam Protection`
/// section for the full payment invariants across the four Rex5 defense layers (preflight,
/// upfront cap, post-merge safety net, sandbox-`validate()` rejection).
pub fn execute_keyless_deploy_call<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    call_inputs: &revm::interpreter::CallInputs,
    tx_bytes: &Bytes,
    gas_limit_override: U256,
) -> FrameResult {
    let mut gas = Gas::new(call_inputs.gas_limit);
    let return_memory_offset = call_inputs.return_memory_offset.clone();

    // Frame-result constructors. Using macros (rather than closures) so each call site
    // can move `gas` / `return_memory_offset` without borrow-checker conflicts.
    // `make_halt!` delegates to the module-level `oog_frame_result` so the OOG shape is
    // shared with `merge_and_reject_if_overflow`.
    macro_rules! make_halt {
        () => {
            oog_frame_result(gas.limit(), &return_memory_offset)
        };
    }

    macro_rules! make_error {
        ($error:expr) => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(InstructionResult::Revert, encode_error_result($error), gas),
                return_memory_offset,
            ))
        };
    }

    macro_rules! make_success {
        ($gas_used:expr, $deployed_address:expr) => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    IKeylessDeploy::keylessDeployCall::abi_encode_returns(
                        &IKeylessDeploy::keylessDeployReturn {
                            gasUsed: $gas_used,
                            deployedAddress: $deployed_address,
                            errorData: Bytes::new(),
                        },
                    )
                    .into(),
                    gas,
                ),
                return_memory_offset,
            ))
        };
    }

    // Builds a success-style frame result with the error encoded in `errorData`. Used
    // for in-sandbox failures (paired with `apply_sandbox_state` so the signer is charged
    // via merged state).
    macro_rules! make_execution_failure {
        ($gas_used:expr, $error:expr) => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return, // Success, not Revert
                    IKeylessDeploy::keylessDeployCall::abi_encode_returns(
                        &IKeylessDeploy::keylessDeployReturn {
                            gasUsed: $gas_used,
                            deployedAddress: Address::ZERO,
                            errorData: encode_error_result($error).to_vec().into(),
                        },
                    )
                    .into(),
                    gas,
                ),
                return_memory_offset,
            ))
        };
    }

    // Step 1: charge the fixed dispatch overhead (100K covers RLP decoding, sig recovery,
    // state filtering). Rex3+ also records it as compute gas.
    let cost = constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS;
    let has_sufficient_gas = gas.record_cost(cost);
    if !has_sufficient_gas {
        return make_halt!();
    }
    if ctx.spec.is_enabled(MegaSpecId::REX3) {
        let mut limit = ctx.additional_limit.borrow_mut();
        if !limit.record_compute_gas(cost) {
            let crate::LimitCheck::ExceedsLimit { limit, used, frame_local, .. } =
                limit.compute_gas.check_limit()
            else {
                unreachable!()
            };
            return if frame_local {
                // Frame-local: revert; gas returns to caller.
                make_error!(KeylessDeployError::InsufficientComputeGas { limit, used })
            } else {
                // TX-level: halt with OOG, marked as exceeding.
                let mut result = make_halt!();
                mark_frame_result_as_exceeding_limit(
                    &mut result,
                    crate::AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                    Default::default(),
                );
                result
            };
        }
    }

    // Step 2: keyless deploys are fee-free.
    if !call_inputs.value.get().is_zero() {
        return make_error!(KeylessDeployError::NoEtherTransfer);
    }

    // Step 3: decode the keyless transaction (Nick's Method requires nonce = 0).
    let keyless_tx = match decode_keyless_tx(tx_bytes, ctx.spec) {
        Ok(tx) => tx,
        Err(e) => return make_error!(e),
    };
    if keyless_tx.nonce() != 0 {
        return make_error!(KeylessDeployError::NonZeroTxNonce { tx_nonce: keyless_tx.nonce() });
    }

    // Step 4: validate `gasLimitOverride` covers the keyless tx's own gas limit, then
    // (Rex5+) cap it to the parent's remaining gas.
    let tx_gas_limit = keyless_tx.gas_limit();
    let mut gas_limit_override_u64: u64 = gas_limit_override.try_into().unwrap_or(u64::MAX);
    if gas_limit_override_u64 < tx_gas_limit {
        return make_error!(KeylessDeployError::GasLimitTooLow {
            tx_gas_limit,
            provided_gas_limit: gas_limit_override_u64,
        });
    }
    if ctx.spec.is_enabled(MegaSpecId::REX5) {
        gas_limit_override_u64 = gas_limit_override_u64.min(gas.remaining());
    }

    // Step 5: recover the signer and restrict keyless deploys to signer nonce ≤ 1.
    // Allowing 1 keeps deploys possible when the signer previously attempted the raw
    // keyless tx and failed under MegaETH's gas regime.
    let deploy_signer = match recover_signer(&keyless_tx) {
        Ok(addr) => addr,
        Err(e) => return make_error!(e),
    };
    let deploy_address = calculate_keyless_deploy_address(deploy_signer);
    let signer_nonce = match get_account_nonce(ctx, deploy_signer) {
        Ok(nonce) => nonce,
        Err(e) => return make_error!(e),
    };
    if signer_nonce > 1 {
        return make_error!(KeylessDeployError::SignerNonceTooHigh { signer_nonce });
    }

    // Step 6: build the sandbox transaction (nonce forced to 0, raw keyless RLP carried
    // in `enveloped_tx`).
    let sandbox_tx = {
        let tx = TxEnv {
            caller: deploy_signer,
            kind: TxKind::Create,
            data: keyless_tx.input().clone(),
            value: keyless_tx.value(),
            gas_limit: gas_limit_override_u64,
            gas_price: keyless_tx.effective_gas_price(None),
            nonce: 0,
            ..Default::default()
        };
        let mut mega_tx = MegaTransaction::new(tx);
        mega_tx.enveloped_tx = Some(tx_bytes.clone());
        mega_tx
    };

    // Step 7: check the deterministic deploy address isn't already occupied.
    {
        let deploy_account = ctx.journal_mut().database.basic(deploy_address).map_err(|e| {
            error!(
                error = %e,
                deploy_address = ?deploy_address,
                "keyless deploy deploy-address state read failed",
            );
            KeylessDeployError::InternalError
        });
        match deploy_account {
            Ok(Some(info)) if info.code_hash != KECCAK_EMPTY => {
                return make_error!(KeylessDeployError::ContractAlreadyExists);
            }
            Err(e) => return make_error!(e),
            _ => {}
        }
    }

    // Step 8: Rex5+ preflight. Short-circuits before sandbox setup when the sandbox's
    // pre-frame intrinsic alone would exceed the parent's envelope.
    let sandbox_tx_limits =
        ctx.spec.is_enabled(MegaSpecId::REX5).then(|| sandbox_runtime_limits(ctx));
    if let Some(error) = sandbox_intrinsic_overflow_error(ctx.spec, &sandbox_tx, sandbox_tx_limits)
    {
        return make_error!(error);
    }

    // Step 9: Execute sandbox and apply state changes.
    match execute_keyless_deploy_sandbox(ctx, sandbox_tx, sandbox_tx_limits) {
        Ok(SandboxOutcome::Success { state, result, limit_usage }) => {
            // Rex5+ residual-overflow safety net. Skip `apply_sandbox_state` if the merge
            // pushes the parent over a TX-level limit.
            if ctx.spec.is_enabled(MegaSpecId::REX5) {
                if let Some(halt) = merge_and_reject_if_overflow(
                    ctx,
                    &mut gas,
                    limit_usage,
                    result.gas_used,
                    &return_memory_offset,
                ) {
                    return halt;
                }
            }

            if let Err(e) = apply_sandbox_state(ctx, state, deploy_signer) {
                return make_error!(e);
            }

            if result.deploy_address != deploy_address {
                return make_error!(KeylessDeployError::AddressMismatch);
            }

            for log in result.logs {
                ctx.log(log);
            }

            make_success!(result.gas_used, result.deploy_address)
        }
        Ok(SandboxOutcome::Failure { state, error, limit_usage }) => {
            // Extract gas_used from the execution error
            let gas_used = match &error {
                KeylessDeployError::ExecutionReverted { gas_used, .. } |
                KeylessDeployError::ExecutionHalted { gas_used, .. } |
                KeylessDeployError::EmptyCodeDeployed { gas_used } => *gas_used,
                _ => 0, // Shouldn't happen for execution errors
            };

            // Rex5+ residual-overflow safety net — mirrors the Success branch above. A
            // sandbox Failure may still report tx-level persistent usage beyond the cap.
            if ctx.spec.is_enabled(MegaSpecId::REX5) {
                if let Some(halt) = merge_and_reject_if_overflow(
                    ctx,
                    &mut gas,
                    limit_usage,
                    gas_used,
                    &return_memory_offset,
                ) {
                    return halt;
                }
            }

            if let Err(e) = apply_sandbox_state(ctx, state, deploy_signer) {
                return make_error!(e);
            }

            // Return success-style so the merged sandbox state (signer balance deduction
            // = sandbox gas charge) persists; the outer caller sees the failure reason in
            // `errorData`.
            make_execution_failure!(gas_used, error)
        }
        Err(e) => make_error!(e),
    }
}

/// Result of sandbox execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxResult {
    gas_used: u64,
    deploy_address: Address,
    logs: Vec<Log>,
}

/// Executes the contract creation in a sandbox environment.
///
/// Uses a type-erased `SandboxDb` to prevent infinite type instantiation.
///
/// # Arguments
///
/// * `ctx` - The parent context to execute in
/// * `sandbox_tx` - The transaction to execute, with `enveloped_tx` set to the original raw keyless
///   deploy transaction bytes
pub(crate) fn execute_keyless_deploy_sandbox<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_tx: MegaTransaction,
    sandbox_tx_limits: Option<EvmTxRuntimeLimits>,
) -> Result<SandboxOutcome, KeylessDeployError> {
    let deploy_signer = sandbox_tx.caller();
    let gas_limit = sandbox_tx.gas_limit();
    let gas_price = sandbox_tx.gas_price();
    let value = sandbox_tx.value();

    // Extract values we need BEFORE borrowing the journal
    let mega_spec = ctx.mega_spec();
    let block = ctx.block().clone();
    let chain = ctx.chain().clone();

    // REX4+: Clone Rc references to parent's external envs before `journal_mut()` mutably
    // borrows `ctx`, after which its fields are no longer accessible.
    let shared_external_envs = mega_spec
        .is_enabled(MegaSpecId::REX4)
        .then(|| (Rc::clone(&ctx.salt_env), Rc::clone(&ctx.oracle_env)));

    let journal = ctx.journal_mut();

    // Create type-erased sandbox database with split borrows:
    // - Immutable reference to journal state (for cached accounts)
    // - Mutable reference to underlying database (for cache misses)
    // Override the signer's nonce to 0 for keyless deploy (Nick's Method requires nonce=0)
    let mut sandbox_db = SandboxDb::new(&journal.inner.state, &mut journal.database)
        .with_nonce_override(deploy_signer);

    // Check signer balance
    let signer_account = sandbox_db
        .basic(deploy_signer)
        .map_err(|e| {
            error!(
                error = %e,
                deploy_signer = ?deploy_signer,
                "keyless deploy signer balance read failed",
            );
            KeylessDeployError::InternalError
        })?
        .unwrap_or_default();

    // Ensure signer has enough balance to cover gas cost and value
    let gas_cost = U256::from(gas_limit) * U256::from(gas_price);
    let total_cost = gas_cost.checked_add(value).ok_or(KeylessDeployError::InsufficientBalance)?;
    if signer_account.balance < total_cost {
        return Err(KeylessDeployError::InsufficientBalance);
    }

    // Execute sandbox - using type-erased SandboxDb prevents infinite type instantiation.
    // The two branches differ only in which MegaContext constructor to use: REX4+ shares
    // the parent's salt and oracle envs, while pre-REX4 creates an EmptyExternalEnv for
    // backward compatibility. Everything after context construction is identical and is
    // factored into `run_sandbox_ctx`.
    if let Some((salt_env, oracle_env)) = shared_external_envs {
        let sandbox_ctx = MegaContext::<_, ExtEnvs>::new_with_shared_ext_envs(
            sandbox_db, mega_spec, salt_env, oracle_env,
        );
        run_sandbox_ctx(sandbox_ctx, sandbox_tx, sandbox_tx_limits, block, chain)
    } else {
        let sandbox_ctx = MegaContext::new(sandbox_db, mega_spec);
        run_sandbox_ctx(sandbox_ctx, sandbox_tx, sandbox_tx_limits, block, chain)
    }
}

/// Applies the shared sandbox-context configuration, runs the sandbox tx, and returns the
/// processed outcome. Factored out of `execute_keyless_deploy_sandbox` so the REX4+ and
/// pre-REX4 branches only differ in the `MegaContext` constructor they call.
fn run_sandbox_ctx<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    sandbox_ctx: MegaContext<DB, ExtEnvs>,
    sandbox_tx: MegaTransaction,
    sandbox_tx_limits: Option<EvmTxRuntimeLimits>,
    block: BlockEnv,
    chain: L1BlockInfo,
) -> Result<SandboxOutcome, KeylessDeployError> {
    let sandbox_ctx = match sandbox_tx_limits {
        Some(limits) => sandbox_ctx.with_tx_runtime_limits(limits),
        None => sandbox_ctx,
    };
    let sandbox_ctx = sandbox_ctx.with_block(block).with_chain(chain).with_sandbox_disabled(true);
    let mut sandbox_evm = MegaEvm::new(sandbox_ctx);
    let result = sandbox_evm.transact_raw(sandbox_tx);
    let limit_usage = sandbox_evm.ctx.additional_limit.borrow().get_usage();
    process_sandbox_transact_result(result, limit_usage)
}

/// Outcome of sandbox execution, including state for merging on failure.
#[derive(Debug)]
pub enum SandboxOutcome {
    /// Successful execution with the resulting state and return data.
    Success {
        /// Sandbox state to merge into the parent context.
        state: EvmState,
        /// Execution result details.
        result: SandboxResult,
        /// Resource usage from the sandbox's additional limit trackers.
        limit_usage: LimitUsage,
    },
    /// Failed execution with the resulting state and error.
    Failure {
        /// Sandbox state to merge into the parent context.
        state: EvmState,
        /// Error returned by sandbox execution.
        error: KeylessDeployError,
        /// Resource usage from the sandbox's additional limit trackers.
        limit_usage: LimitUsage,
    },
}

/// Processes the result of sandbox EVM execution into a [`SandboxOutcome`].
///
/// Handles all execution result variants (success, revert, halt) and transact errors.
///
/// `transact_raw` errors are split into two `KeylessDeployError` variants by selector
/// so relayer-side decoders can tell them apart:
/// - `InvalidTransaction` when `IsTxError::is_tx_error()` returns `true`.
/// - `InternalError` for everything else (DB I/O, header validation, `EVMError::Custom`).
fn process_sandbox_transact_result<E: core::fmt::Display + IsTxError>(
    result: Result<ResultAndState<MegaHaltReason>, E>,
    limit_usage: LimitUsage,
) -> Result<SandboxOutcome, KeylessDeployError> {
    match result {
        Ok(ResultAndState { result: exec_result, state: sandbox_state }) => match exec_result {
            ExecutionResult::Success { gas_used, output, logs, .. } => {
                if let revm::context::result::Output::Create(bytecode, Some(created_addr)) = output
                {
                    // Empty deployed bytecode is treated as a sandbox failure so the deploy
                    // address never gets a barrier installed for an empty contract. Without
                    // this check the same signed keyless tx could be charged on every
                    // submission while leaving the deploy slot re-usable.
                    if bytecode.is_empty() {
                        return Ok(SandboxOutcome::Failure {
                            state: sandbox_state,
                            error: KeylessDeployError::EmptyCodeDeployed { gas_used },
                            limit_usage,
                        });
                    }
                    Ok(SandboxOutcome::Success {
                        state: sandbox_state,
                        result: SandboxResult { deploy_address: created_addr, gas_used, logs },
                        limit_usage,
                    })
                } else {
                    // Contract creation didn't return an address - should never happen
                    // but we return an error instead of panicking to avoid crashing the node
                    Err(KeylessDeployError::NoContractCreated)
                }
            }
            ExecutionResult::Revert { gas_used, output } => Ok(SandboxOutcome::Failure {
                state: sandbox_state,
                error: KeylessDeployError::ExecutionReverted { gas_used, output },
                limit_usage,
            }),
            ExecutionResult::Halt { gas_used, reason } => {
                // The halt `reason` is dropped on the ABI wire (the Solidity error
                // carries only `gasUsed`) and `decode_error_result` synthesizes a
                // placeholder on the way back, so node-side `tracing` is the only
                // place the real cause is observable.
                warn!(
                    reason = ?reason,
                    gas_used,
                    "keyless deploy sandbox halted",
                );
                Ok(SandboxOutcome::Failure {
                    state: sandbox_state,
                    error: KeylessDeployError::ExecutionHalted { gas_used, reason },
                    limit_usage,
                })
            }
        },
        // Split tx-validation rejections (selector `InvalidTransaction`) from genuine
        // internal failures (selector `InternalError`) so relayer-side decoders can tell
        // them apart. Both are selector-only on the wire — see `encode_error_result`.
        Err(e) if e.is_tx_error() => {
            warn!(
                error = %e,
                "keyless deploy sandbox transaction rejected during validation",
            );
            Err(KeylessDeployError::InvalidTransaction)
        }
        Err(e) => {
            error!(
                error = %e,
                "keyless deploy sandbox failed with internal error",
            );
            Err(KeylessDeployError::InternalError)
        }
    }
}

/// Rex5+ residual-overflow safety net: merges sandbox usage into the parent tracker and
/// rejects the outer call if the merge violates a non-frame-local TX limit.
///
/// State must NOT be merged on reject — the outer call halts and only pre-execution state
/// should survive. The outer caller absorbs the sandbox's EVM gas via `record_cost`,
/// remaining outer gas is rescued, and the outer frame returns as `OutOfGas` marked as
/// exceeding.
///
/// Returns `Some(FrameResult)` when the outer handler must short-circuit, `None` when the
/// merge fits and execution may continue.
///
/// This path is the last line of defense: `sandbox_runtime_limits` (upfront cap) and
/// `sandbox_intrinsic_overflow_error` (preflight) handle the common cases. Residual fires
/// on single-opcode overshoot at TX-level checks, or on a future pre-frame accounting path
/// that was not added to `AdditionalLimit::intrinsic_check_for_tx`.
fn merge_and_reject_if_overflow<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    gas: &mut Gas,
    limit_usage: LimitUsage,
    sandbox_gas_used: u64,
    return_memory_offset: &core::ops::Range<usize>,
) -> Option<FrameResult> {
    let mut limit = ctx.additional_limit.borrow_mut();
    limit.merge_usage(limit_usage);
    let limit_check = limit.check_limit();
    if !limit_check.exceeded_limit() || limit_check.is_frame_local() {
        return None;
    }
    if !gas.record_cost(sandbox_gas_used) {
        // Outer caller did not have enough gas to absorb the sandbox's EVM gas.
        // Return a plain OutOfGas halt without rescue: the caller already owed this.
        return Some(oog_frame_result(gas.limit(), return_memory_offset));
    }
    limit.rescue_gas(gas);
    let mut result = oog_frame_result(gas.limit(), return_memory_offset);
    mark_frame_result_as_exceeding_limit(
        &mut result,
        crate::AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
        Default::default(),
    );
    Some(result)
}

/// Single source of truth for the `OutOfGas` halt `FrameResult` shape (empty return
/// data, all gas consumed). Callers that need the exceeding-limit marker apply
/// `mark_frame_result_as_exceeding_limit` on the returned value.
fn oog_frame_result(gas_limit: u64, return_memory_offset: &core::ops::Range<usize>) -> FrameResult {
    FrameResult::Call(CallOutcome::new(
        InterpreterResult::new(
            InstructionResult::OutOfGas,
            Bytes::new(),
            Gas::new_spent(gas_limit),
        ),
        return_memory_offset.clone(),
    ))
}

/// Rex5+ preflight: rejects the outer call when the sandbox's known pre-frame intrinsic
/// usage alone would not fit inside the parent's remaining envelope.
///
/// Running such a sandbox is guaranteed to fail internally, so this short-circuits before
/// sandbox EVM construction, signer balance DB read, and halt/rescue teardown — using a
/// lightweight trial-run via `AdditionalLimit::intrinsic_check_for_tx`.
///
/// Returns `Some(KeylessDeployError::ParentBudgetExceeded { .. })` when the outer call
/// should revert, or `None` when the sandbox is permitted to start.
fn sandbox_intrinsic_overflow_error(
    spec: MegaSpecId,
    sandbox_tx: &MegaTransaction,
    sandbox_tx_limits: Option<EvmTxRuntimeLimits>,
) -> Option<KeylessDeployError> {
    let limits = sandbox_tx_limits?;
    match AdditionalLimit::intrinsic_check_for_tx(spec, sandbox_tx, limits) {
        LimitCheck::ExceedsLimit { kind, limit, used, .. } => {
            Some(KeylessDeployError::ParentBudgetExceeded { kind, limit, used })
        }
        LimitCheck::WithinLimit => None,
    }
}

/// Derives the sandbox's TX runtime limits from the parent's remaining budgets.
///
/// Tightens the four resource dimensions (compute gas, data size, KV updates, state growth)
/// to the parent's frame-local remaining capacity. The base is the parent's active
/// `EvmTxRuntimeLimits` (not spec defaults), so any custom detention caps
/// (`block_env_access_compute_gas_limit`, `oracle_access_compute_gas_limit`) are
/// preserved.
///
/// Intrinsic usage is NOT pre-subtracted here — the sandbox's own tracker records it
/// during transaction startup, so double-subtracting would leave the sandbox
/// under-budgeted. Pre-frame intrinsic overflow is instead caught by
/// `sandbox_intrinsic_overflow_error`, and any future TX-level persistent contribution
/// added before the first sandbox frame MUST be mirrored in
/// `AdditionalLimit::intrinsic_check_for_tx` to keep the preflight estimator sound.
fn sandbox_runtime_limits<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
) -> EvmTxRuntimeLimits {
    let parent_limit = ctx.additional_limit.borrow();
    let limits = parent_limit.limits;

    limits
        .with_tx_compute_gas_limit(parent_limit.current_call_remaining_compute_gas())
        .with_tx_data_size_limit(parent_limit.current_call_remaining_data_size())
        .with_tx_kv_updates_limit(parent_limit.current_call_remaining_kv_updates())
        .with_tx_state_growth_limit(parent_limit.current_call_remaining_state_growth())
}

/// Reads an account nonce from the journal cache first, then falls back to the backing database.
fn get_account_nonce<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    address: Address,
) -> Result<u64, KeylessDeployError> {
    let journal = ctx.journal_mut();
    if let Some(acc) = journal.state.get(&address) {
        return Ok(acc.info.nonce);
    }
    Ok(journal
        .database
        .basic(address)
        .map_err(|e| {
            error!(
                error = %e,
                address = ?address,
                "keyless deploy nonce read failed",
            );
            KeylessDeployError::InternalError
        })?
        .map(|info| info.nonce)
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::result::Output;

    /// Test error type that lets us drive `process_sandbox_transact_result`'s `Err` arms
    /// directly without standing up a full sandbox EVM. The two arms differ only by
    /// `IsTxError::is_tx_error()`, so a single struct with a configurable flag covers
    /// both selector mappings.
    struct FakeTxErr {
        is_tx: bool,
        msg: &'static str,
    }

    impl core::fmt::Display for FakeTxErr {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str(self.msg)
        }
    }

    impl IsTxError for FakeTxErr {
        fn is_tx_error(&self) -> bool {
            self.is_tx
        }
    }

    /// The sandbox's create transaction always asks for `TxKind::Create`, but if revm
    /// ever surfaces a `Success` with `Output::Call` for that input shape the merge code
    /// would otherwise unwrap into an address-less success. Pin the defensive
    /// `NoContractCreated` mapping.
    #[test]
    fn test_process_result_call_output_maps_to_no_contract_created() {
        let result: Result<ResultAndState<MegaHaltReason>, FakeTxErr> = Ok(ResultAndState {
            result: ExecutionResult::Success {
                reason: revm::context::result::SuccessReason::Stop,
                gas_used: 1,
                gas_refunded: 0,
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state: EvmState::default(),
        });
        let out = process_sandbox_transact_result(result, LimitUsage::default());
        assert!(matches!(out, Err(KeylessDeployError::NoContractCreated)), "unexpected: {out:?}");
    }

    /// `Output::Create(_, None)` — Create succeeded but revm did not return an address.
    /// Same defensive `NoContractCreated` shape as the Call branch.
    #[test]
    fn test_process_result_create_without_address_maps_to_no_contract_created() {
        let result: Result<ResultAndState<MegaHaltReason>, FakeTxErr> = Ok(ResultAndState {
            result: ExecutionResult::Success {
                reason: revm::context::result::SuccessReason::Stop,
                gas_used: 1,
                gas_refunded: 0,
                logs: Vec::new(),
                output: Output::Create(Bytes::from_static(&[0x60, 0x00]), None),
            },
            state: EvmState::default(),
        });
        let out = process_sandbox_transact_result(result, LimitUsage::default());
        assert!(matches!(out, Err(KeylessDeployError::NoContractCreated)), "unexpected: {out:?}");
    }

    /// `transact_raw` error where `is_tx_error()` is `false` (DB I/O, header validation,
    /// `EVMError::Custom`, etc.) MUST map to the selector-only `InternalError`. Pinned
    /// because a regression that re-collapses this into `InvalidTransaction` would
    /// silently re-classify infrastructure failures as user-input rejections.
    #[test]
    fn test_process_result_non_tx_error_maps_to_internal_error() {
        let result: Result<ResultAndState<MegaHaltReason>, FakeTxErr> =
            Err(FakeTxErr { is_tx: false, msg: "db blew up" });
        let out = process_sandbox_transact_result(result, LimitUsage::default());
        assert!(matches!(out, Err(KeylessDeployError::InternalError)), "unexpected: {out:?}");
    }

    /// Mirror of the above for `is_tx_error() == true`: MUST map to
    /// `InvalidTransaction` with its dedicated selector.
    #[test]
    fn test_process_result_tx_error_maps_to_invalid_transaction() {
        let result: Result<ResultAndState<MegaHaltReason>, FakeTxErr> =
            Err(FakeTxErr { is_tx: true, msg: "intrinsic gas too low" });
        let out = process_sandbox_transact_result(result, LimitUsage::default());
        assert!(matches!(out, Err(KeylessDeployError::InvalidTransaction)), "unexpected: {out:?}");
    }

    /// `get_account_nonce`: cache miss path on a failing DB MUST map the underlying
    /// `DBError` to selector-only `InternalError`. This is the only sandbox-internal
    /// call site that reads a nonce via the parent journal database directly (rather
    /// than via the journal cache or `SandboxDb` fallthrough), so the `error!` `map_err`
    /// is the only place the failure can be classified.
    #[test]
    fn test_get_account_nonce_db_error_maps_to_internal_error() {
        use crate::{
            test_utils::{ErrorInjectingDatabase, MemoryDatabase},
            EmptyExternalEnv,
        };
        use alloy_primitives::address;

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        db.fail_on_account = Some(signer);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(db, MegaSpecId::REX5);
        // Cache is empty → cache miss → DB fallback → injected error.
        let out = get_account_nonce(&mut ctx, signer);
        assert!(matches!(out, Err(KeylessDeployError::InternalError)), "unexpected: {out:?}");
    }
}
