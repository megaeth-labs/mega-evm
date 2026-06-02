//! Keyless deploy sandbox execution.
//!
//! Implements Nick's Method deterministic deployment via an isolated sandbox. See the
//! module-level `Spam Protection` section in `sandbox/mod.rs` for the invariants that
//! govern when each path is taken (normal completion / Rex5 preflight reject / Rex5
//! residual-overflow reject / Rex5 sandbox-`validate()` reject).
//!
//! The Rex5 defense layers are implemented here as: `sandbox_runtime_limits` (upfront
//! cap), `sandbox_intrinsic_overflow_error` (preflight), and `reject_if_tx_limit_overflow`
//! (post-merge safety net).
//!
//! Rex5+ runs the sandbox transaction as an OP deposit-like transaction
//! (`gas_price = 0` plus `deposit.source_hash` set, via
//! `build_fee_free_sandbox_deposit_tx`). The deposit-style sandbox tx never debits
//! the inner signer for gas, never credits coinbase or any OP fee vault, and is
//! covered by the outer transaction's own fee model: `gas_limit_override` is
//! pre-debited from the outer `Gas` counter before the sandbox runs and
//! `refund_unused_sandbox_gas` returns the unused portion on exit, mirroring revm's
//! standard message-call accounting. The signer's only state change is the
//! `make_create_frame` nonce bump that serves as the replay barrier.
//!
//! A Rex5 sandbox-`validate()` rejection — the final Mega-side intrinsic / floor
//! gas check inside `MegaHandler::validate` — surfaces in two distinct ways:
//! op-revm's deposit `catch_error` converts tx-validation errors into
//! `Ok(Halt(FailedDeposit, gas_used = gas_limit))`; `process_sandbox_transact_result`
//! maps that back to `KeylessDeployError::InvalidTransaction` so the outer
//! keyless-deploy call surfaces as `Revert`, the sandbox state (including the
//! deposit-`catch_error` nonce bump) is dropped, and `apply_sandbox_post_accounting`
//! is never invoked. Pre-Rex5 specs run the sandbox as a non-deposit transaction:
//! sandbox `pre_execution` debits the signer at the signed gas price, and
//! validation errors propagate as standard `Err(InvalidTransaction)`.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{rc::Rc, vec::Vec};

use alloy_consensus::{Signed, Transaction as AlloyTransaction, TxLegacy};
use alloy_evm::{Database as AlloyDatabase, Evm};
use alloy_primitives::{Address, Bytes, Log, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;
use op_revm::{handler::IsTxError, L1BlockInfo};
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},
        BlockEnv, Cfg, ContextTr, TxEnv,
    },
    context_interface::Transaction,
    handler::FrameResult,
    interpreter::{CallOutcome, Gas, Host, InstructionResult, InterpreterResult},
    primitives::KECCAK_EMPTY,
    state::{AccountInfo, EvmState},
    Database as RevmDatabase,
};
use tracing::{error, warn};

use crate::{
    constants, mark_frame_result_as_exceeding_limit, AdditionalLimit, EvmTxRuntimeLimits,
    ExternalEnvTypes, JournalInspectTr, LimitCheck, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, TxRuntimeLimit, VolatileDataAccess,
    SANDBOX_TX_SOURCE_HASH,
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
/// 3. Rex5+: pre-checks `keyless_tx.input().len() <= cfg().max_initcode_size()` — op-revm's deposit
///    path bypasses revm's `validate_env`, so the sandbox enforces the configured initcode size
///    limit itself.
/// 4. Validates the gas limit override against the transaction's gas limit.
/// 5. Recovers the signer and calculates the deploy address.
/// 6. Rex5+: pre-charges `new_account_storage_gas(deploy_signer)` against the outer Gas counter
///    when the parent-visible signer is unmaterialized (alongside `KEYLESS_DEPLOY_OVERHEAD_GAS`,
///    retained on every sandbox outcome — see `charge_caller_materialization_pre_sandbox`), then
///    preflights known sandbox intrinsic usage against the parent's remaining resource envelope and
///    reverts with `ParentBudgetExceeded` if it would not fit.
/// 7. Executes contract creation in an isolated sandbox environment. Rex5+ runs the sandbox tx as
///    an OP deposit-like, fee-free transaction with `gas_limit_override` pre-debited from the outer
///    Gas counter.
/// 8. On sandbox completion, refunds the unused portion of the reservation to the outer Gas counter
///    and either merges state (normal path) or rejects without merging (Rex5 overflow safety net),
///    via `apply_sandbox_post_accounting`.
///
/// Must only be called at `depth == 0` (enforced by `evm/execution.rs`); a wrapping contract
/// must not be able to intercept and revert the charge. See the module-level `Spam Protection`
/// section for the full payment invariants across the Rex5 defense layers (preflight,
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
    // shared with `reject_if_tx_limit_overflow`.
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

    // Builds a success-style frame result with the error encoded in `errorData`. Used for
    // in-sandbox failures (paired with `apply_sandbox_state` so the consumed replay barrier
    // — the signer nonce bump from `make_create_frame` — is merged into the parent).
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

    // Rex5+: enforce the configured init code size limit. Pre-Rex5 specs run the sandbox
    // through op-revm's non-deposit path, where revm's `validate_env` enforces it for us.
    if ctx.spec.is_enabled(MegaSpecId::REX5) {
        let max = ctx.cfg().max_initcode_size();
        let size = keyless_tx.input().len();
        if size > max {
            return make_error!(KeylessDeployError::InitCodeTooLarge {
                size: size as u64,
                max: max as u64,
            });
        }
    }

    // Step 4: validate `gasLimitOverride` covers the keyless tx's own gas limit. The
    // Rex5+ cap is deferred to step 4b below so it observes the materialization charge.
    let tx_gas_limit = keyless_tx.gas_limit();
    let mut gas_limit_override_u64: u64 = gas_limit_override.try_into().unwrap_or(u64::MAX);
    if gas_limit_override_u64 < tx_gas_limit {
        return make_error!(KeylessDeployError::GasLimitTooLow {
            tx_gas_limit,
            provided_gas_limit: gas_limit_override_u64,
        });
    }

    // Step 5: recover the signer and restrict keyless deploys to signer nonce ≤ 1.
    // Allowing 1 keeps deploys possible when the signer previously attempted the raw
    // keyless tx and failed under MegaETH's gas regime.
    let deploy_signer = match recover_signer(&keyless_tx) {
        Ok(addr) => addr,
        Err(e) => return make_error!(e),
    };
    let deploy_address = calculate_keyless_deploy_address(deploy_signer);

    // Rex5+ reads the deploy signer's parent-state account info once via a cold-marked
    // `inspect_account`, then drives the upcoming nonce check, EIP-3607 enforcement, and
    // materialization charge from that single source. The helper itself gates `load_code`
    // on `cfg.disable_eip3607` to preserve the canonical revm
    // `validate_account_nonce_and_code` no-`code_by_hash` short-circuit. Pre-Rex5 keeps
    // the raw-DB `get_account_nonce` path unchanged to preserve stable-spec semantics
    // (no journal cache write for the signer).
    let rex5_signer_info = if ctx.spec.is_enabled(MegaSpecId::REX5) {
        match inspect_signer_parent_state(ctx, deploy_signer) {
            Ok(info) => Some(info),
            Err(e) => return make_error!(e),
        }
    } else {
        None
    };

    let signer_nonce = if let Some(info) = &rex5_signer_info {
        info.nonce
    } else {
        match get_account_nonce(ctx, deploy_signer) {
            Ok(nonce) => nonce,
            Err(e) => return make_error!(e),
        }
    };
    if signer_nonce > 1 {
        return make_error!(KeylessDeployError::SignerNonceTooHigh { signer_nonce });
    }

    if let Some(signer_info) = &rex5_signer_info {
        // Rex5+: re-enforce the EIP-3607 caller-with-code check that the deposit-style
        // sandbox would otherwise bypass via op-revm's
        // `validate_against_state_and_deduct_caller`.
        if let Err(e) = validate_signer_code(ctx, signer_info) {
            return make_error!(e);
        }

        // Rex5+: pre-charge deploy-signer materialization gas (and record the state-growth
        // event) before the sandbox is constructed, alongside `KEYLESS_DEPLOY_OVERHEAD_GAS`.
        // Upfront timing makes the charge symmetric across all sandbox outcomes (success,
        // in-sandbox failure, sandbox-validate reject) and prevents the relayer from sizing
        // the outer budget to exclude materialization.
        match charge_caller_materialization_pre_sandbox(
            ctx,
            &mut gas,
            deploy_signer,
            signer_info,
            &return_memory_offset,
        ) {
            Err(e) => return make_error!(e),
            Ok(Some(halt)) => return halt,
            Ok(None) => {}
        }
    }

    // Step 4b: Rex5+ cap of `gas_limit_override` to the outer's remaining gas. Runs after
    // materialization has been debited so the reservation in step 8b only covers the
    // sandbox envelope itself.
    //
    // After the cap, re-enforce the signer's "must execute with at least `tx_gas_limit`"
    // guarantee. The relayer can shrink the outer envelope so that `gas.remaining()`
    // drops below the keyless `tx_gas_limit`, in which case the cap brings the override
    // below the signed minimum. Rejecting here with the same `GasLimitTooLow` shape
    // surfaces the failure cleanly instead of letting the sandbox OOG silently. The
    // pre-cap check above (Step 4) is retained so a relayer passing
    // `override < tx_gas_limit` outright still fails before the signer-recovery /
    // materialization work.
    if ctx.spec.is_enabled(MegaSpecId::REX5) {
        gas_limit_override_u64 = gas_limit_override_u64.min(gas.remaining());
        if gas_limit_override_u64 < tx_gas_limit {
            return make_error!(KeylessDeployError::GasLimitTooLow {
                tx_gas_limit,
                provided_gas_limit: gas_limit_override_u64,
            });
        }
    }

    // Step 6: build the sandbox transaction (nonce forced to 0, raw keyless RLP carried
    // in `enveloped_tx`). Rex5+ runs the sandbox tx as an OP deposit-like transaction
    // (gas_price=0, source_hash set) so caller balance is never debited for gas; pre-Rex5
    // keeps the original signed gas price.
    let sandbox_tx = if ctx.spec.is_enabled(MegaSpecId::REX5) {
        build_fee_free_sandbox_deposit_tx(
            deploy_signer,
            &keyless_tx,
            tx_bytes,
            gas_limit_override_u64,
        )
    } else {
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

    // Step 8b: Rex5+ pre-debit the sandbox's gas reservation from the outer gas counter,
    // mirroring revm's standard message-call shape (pre-debit on entry, refund unused on
    // exit). The unconditional success follows from step 4b's
    // `gas_limit_override.min(gas.remaining())` cap — there is no intervening gas
    // movement between the cap and this debit.
    if ctx.spec.is_enabled(MegaSpecId::REX5) {
        let ok = gas.record_cost(gas_limit_override_u64);
        debug_assert!(
            ok,
            "Rex5+ sandbox pre-debit must succeed: gas_limit_override is capped to gas.remaining()",
        );
    }

    // Step 9: Execute sandbox and apply state changes.
    match execute_keyless_deploy_sandbox(ctx, sandbox_tx, sandbox_tx_limits) {
        SandboxOutcome::Completed { state, completion, limit_usage, volatile_accesses } => {
            let gas_used = completion.gas_used();

            if ctx.spec.is_enabled(MegaSpecId::REX5) {
                if let Some(halt) = apply_sandbox_post_accounting(
                    ctx,
                    &mut gas,
                    limit_usage,
                    volatile_accesses,
                    gas_limit_override_u64,
                    gas_used,
                    &return_memory_offset,
                ) {
                    return halt;
                }
            }

            if let Err(e) = apply_sandbox_state(ctx, state, deploy_signer) {
                return make_error!(e);
            }

            // Dispatch ABI return shape. `Deployed` and `EmptyCode` both forward
            // constructor logs into the parent receipt (run-to-completion EVM side
            // effect); `ExecutionFailed` (Revert / Halt) does not, because revm's
            // own frame accounting already rolled the failed frame's logs back.
            match completion {
                SandboxCompletion::Deployed { gas_used, deploy_address: deployed, logs } => {
                    if deployed != deploy_address {
                        return make_error!(KeylessDeployError::AddressMismatch);
                    }
                    for log in logs {
                        ctx.log(log);
                    }
                    make_success!(gas_used, deployed)
                }
                SandboxCompletion::EmptyCode { gas_used, logs } => {
                    for log in logs {
                        ctx.log(log);
                    }
                    make_execution_failure!(
                        gas_used,
                        KeylessDeployError::EmptyCodeDeployed { gas_used }
                    )
                }
                SandboxCompletion::ExecutionFailed { gas_used, error } => {
                    // Success-style outer return so the merged sandbox state (signer
                    // nonce bump from `make_create_frame`, plus any committed sandbox
                    // writes) persists; outer caller sees the failure reason in
                    // `errorData`.
                    make_execution_failure!(gas_used, error)
                }
            }
        }
        SandboxOutcome::Rejected(e) => {
            // Sandbox bailed before producing a frame — typically a validate-reject
            // (`FailedDeposit` → `InvalidTransaction`) or an internal error. Refund
            // the full reservation; the materialization charge already applied
            // upfront is intentionally retained, mirroring the upfront
            // `KEYLESS_DEPLOY_OVERHEAD_GAS` charge.
            if ctx.spec.is_enabled(MegaSpecId::REX5) {
                gas.erase_cost(gas_limit_override_u64);
            }
            make_error!(e)
        }
    }
}

/// Builds a sandbox transaction that runs as an OP deposit-like transaction.
///
/// Rex5+ only: `source_hash` makes op-revm treat the tx as a deposit (skips L1/operator
/// fee, `validate_env`, balance / nonce check, and `reward_beneficiary` distribution);
/// `gas_price = 0` keeps the deposit-path caller balance escrow at zero so the inner
/// signer is never charged for sandbox gas. `deposit.mint` is explicitly `None` to
/// guarantee no ETH is minted to the signer.
fn build_fee_free_sandbox_deposit_tx(
    deploy_signer: Address,
    keyless_tx: &Signed<TxLegacy>,
    raw_tx_bytes: &Bytes,
    gas_limit: u64,
) -> MegaTransaction {
    let tx = TxEnv {
        caller: deploy_signer,
        kind: TxKind::Create,
        data: keyless_tx.input().clone(),
        value: keyless_tx.value(),
        gas_limit,
        gas_price: 0,
        nonce: 0,
        ..Default::default()
    };
    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(raw_tx_bytes.clone());
    mega_tx.deposit.source_hash = SANDBOX_TX_SOURCE_HASH;
    mega_tx.deposit.mint = None;
    mega_tx
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
) -> SandboxOutcome {
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

    // Deliberately do not merge `DynamicGasCost.accessed_bucket_ids` back into the
    // parent. Sandbox and parent share the same immutable-in-block `SaltEnv`, while
    // the sandbox's dynamic-gas cache is intentionally local to that context.
    // Revisit if `SaltEnv` becomes block-mutable or bucket ids become a primary
    // parallel-EVM conflict signal.
    let journal = ctx.journal_mut();

    // Create type-erased sandbox database with split borrows:
    // - Immutable reference to journal state (for cached accounts)
    // - Mutable reference to underlying database (for cache misses)
    // Override the signer's nonce to 0 for keyless deploy (Nick's Method requires nonce=0)
    let mut sandbox_db = SandboxDb::new(&journal.inner.state, &mut journal.database)
        .with_nonce_override(deploy_signer);

    // Check signer balance
    let signer_account = match sandbox_db.basic(deploy_signer) {
        Ok(info) => info.unwrap_or_default(),
        Err(e) => {
            error!(
                error = %e,
                deploy_signer = ?deploy_signer,
                "keyless deploy signer balance read failed",
            );
            return SandboxOutcome::Rejected(KeylessDeployError::InternalError);
        }
    };

    // Ensure signer can cover the transfer (and pre-Rex5 also the gas escrow).
    // Rex5+ runs the sandbox as a fee-free deposit-like tx (gas_price=0), so the only
    // remaining requirement is enough balance for `value`. Pre-Rex5 keeps the
    // `gas_cost + value` check, matching its non-deposit sandbox path.
    let total_cost = if mega_spec.is_enabled(MegaSpecId::REX5) {
        value
    } else {
        let gas_cost = U256::from(gas_limit) * U256::from(gas_price);
        match gas_cost.checked_add(value) {
            Some(total) => total,
            None => return SandboxOutcome::Rejected(KeylessDeployError::InsufficientBalance),
        }
    };
    if signer_account.balance < total_cost {
        return SandboxOutcome::Rejected(KeylessDeployError::InsufficientBalance);
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
) -> SandboxOutcome {
    let sandbox_ctx = match sandbox_tx_limits {
        Some(limits) => sandbox_ctx.with_tx_runtime_limits(limits),
        None => sandbox_ctx,
    };
    let sandbox_ctx = sandbox_ctx.with_block(block).with_chain(chain).with_inside_sandbox(true);
    let is_rex5_enabled = sandbox_ctx.mega_spec().is_enabled(MegaSpecId::REX5);
    let mut sandbox_evm = MegaEvm::new(sandbox_ctx);
    let result = sandbox_evm.transact_raw(sandbox_tx);
    let limit_usage = sandbox_evm.ctx.additional_limit.borrow().get_usage();
    let volatile_accesses =
        sandbox_evm.ctx.volatile_data_tracker.borrow().get_volatile_data_accessed();
    process_sandbox_transact_result(result, limit_usage, volatile_accesses, is_rex5_enabled)
}

/// Outcome of sandbox execution.
///
/// Splits the two questions the caller actually has to answer separately:
///
/// - **`Completed` vs `Rejected`** — did the sandbox EVM run far enough to produce merge-able side
///   effects (state / resource usage / volatile-access footprint / logs)? `Rejected` means the
///   sandbox bailed before producing a frame (validate-reject or internal error); nothing should be
///   applied to the parent context.
/// - **`SandboxCompletion::{Deployed, EmptyCode, ExecutionFailed}`** — when the sandbox did
///   complete, what wire shape should the outer caller report?
///
/// Logs naturally live on the completion variants where the EVM ran to a
/// non-reverted exit (`Deployed` and `EmptyCode`). `ExecutionFailed` covers
/// Revert / Halt — revm's own frame accounting already rolled those logs back,
/// so there is nothing to forward.
#[derive(Debug)]
pub enum SandboxOutcome {
    /// Sandbox EVM ran to a frame exit (success, empty-code success, revert, or
    /// halt). State, resource usage, and volatile-access footprint are
    /// merge-able into the parent context.
    Completed {
        /// Sandbox state to merge into the parent context.
        state: EvmState,
        /// Wire-shape dispatch for what the outer caller should report.
        completion: SandboxCompletion,
        /// Resource usage from the sandbox's additional limit trackers.
        limit_usage: LimitUsage,
        /// Volatile-access footprint to merge into the parent after sandbox return.
        volatile_accesses: VolatileDataAccess,
    },
    /// Sandbox bailed before producing a frame (validate-reject `InvalidTransaction`
    /// or `InternalError`). No state, resource usage, or volatile-access footprint
    /// applies — the outer caller must refund the full pre-debited reservation.
    Rejected(KeylessDeployError),
}

/// Wire-shape dispatch for a completed sandbox execution.
///
/// `Deployed` and `EmptyCode` both surface as success-shape outer returns: the
/// outer caller forwards their logs into the parent receipt before encoding the
/// ABI response. `EmptyCodeDeployed` is an ABI-layer "deploy did not stick"
/// signal — it is *not* an EVM execution failure. Pre-REX5 collapses
/// `EmptyCode` into `ExecutionFailed { error: EmptyCodeDeployed }` so logs are
/// dropped, preserving the frozen replay behavior.
///
/// `ExecutionFailed` covers Revert / Halt. revm rolled the frame's logs back
/// inside the sandbox; there is nothing to forward to the parent.
#[derive(Debug)]
pub enum SandboxCompletion {
    /// Inner CREATE succeeded with non-empty runtime bytecode.
    Deployed {
        /// Gas consumed by the sandbox EVM execution.
        gas_used: u64,
        /// Address at which the contract was deployed.
        deploy_address: Address,
        /// Logs emitted by the constructor; forwarded into the parent receipt.
        logs: Vec<Log>,
    },
    /// Inner CREATE succeeded but returned empty runtime bytecode.
    ///
    /// REX5+ only. Pre-REX5 routes empty-code through `ExecutionFailed` with
    /// `EmptyCodeDeployed` so logs are dropped, preserving the frozen replay
    /// behavior.
    EmptyCode {
        /// Gas consumed by the sandbox EVM execution.
        gas_used: u64,
        /// Logs emitted by the constructor; forwarded into the parent receipt
        /// per the keyless-deploy spec's "merged state includes ... logs when
        /// applicable" rule.
        logs: Vec<Log>,
    },
    /// Sandbox EVM execution did not result in a successful deploy reachable
    /// by the parent.
    ///
    /// Covers two cases that share an ABI wire shape but have different
    /// underlying reasons for not forwarding logs:
    /// - `ExecutionResult::Revert` and `ExecutionResult::Halt` — revm already rolled the failed
    ///   frame's logs back inside the sandbox; nothing exists to forward.
    /// - Pre-REX5 `EmptyCodeDeployed` — the constructor ran successfully and emitted logs, but
    ///   pre-REX5 deliberately drops them at this layer for replay parity. REX5+ uses
    ///   [`SandboxCompletion::EmptyCode`] to forward them instead.
    ExecutionFailed {
        /// Gas consumed by the sandbox EVM execution before the failure.
        gas_used: u64,
        /// ABI-encoded failure reason.
        error: KeylessDeployError,
    },
}

impl SandboxCompletion {
    /// Returns the gas consumed by the sandbox EVM. Used by the outer caller to
    /// compute the unused reservation to refund.
    pub(crate) fn gas_used(&self) -> u64 {
        match self {
            Self::Deployed { gas_used, .. } |
            Self::EmptyCode { gas_used, .. } |
            Self::ExecutionFailed { gas_used, .. } => *gas_used,
        }
    }
}

/// Processes the result of sandbox EVM execution into a [`SandboxOutcome`].
///
/// Handles all execution result variants (success, revert, halt) and transact errors.
///
/// `transact_raw` errors are split into two `KeylessDeployError` variants by selector
/// so relayer-side decoders can tell them apart:
/// - `InvalidTransaction` when `IsTxError::is_tx_error()` returns `true`.
/// - `InternalError` for everything else (DB I/O, header validation, `EVMError::Custom`).
///
/// `is_rex5_enabled` gates the empty-code branch: REX5+ produces
/// [`SandboxCompletion::EmptyCode`] (which forwards the constructor's emitted logs into
/// the parent receipt per the keyless-deploy spec), pre-REX5 collapses to
/// [`SandboxCompletion::ExecutionFailed`] with `EmptyCodeDeployed` so logs are dropped
/// for replay parity.
fn process_sandbox_transact_result<E: core::fmt::Display + IsTxError>(
    result: Result<ResultAndState<MegaHaltReason>, E>,
    limit_usage: LimitUsage,
    volatile_accesses: VolatileDataAccess,
    is_rex5_enabled: bool,
) -> SandboxOutcome {
    let completed = |state, completion| SandboxOutcome::Completed {
        state,
        completion,
        limit_usage,
        volatile_accesses,
    };

    match result {
        Ok(ResultAndState { result: exec_result, state: sandbox_state }) => match exec_result {
            ExecutionResult::Success { gas_used, output, logs, .. } => {
                let revm::context::result::Output::Create(bytecode, Some(created_addr)) = output
                else {
                    // Contract creation didn't return an address — should never happen
                    // (the sandbox tx always asks for `TxKind::Create`), but we return
                    // a Rejected outcome instead of panicking to avoid crashing the node.
                    return SandboxOutcome::Rejected(KeylessDeployError::NoContractCreated);
                };
                if bytecode.is_empty() {
                    // REX5+: surface as `EmptyCode` so the outer caller forwards the
                    // constructor's logs into the parent receipt before returning
                    // success-style `EmptyCodeDeployed` errorData. Pre-REX5 collapses
                    // back to `ExecutionFailed { EmptyCodeDeployed }` so logs are
                    // dropped, preserving the frozen replay behavior.
                    if is_rex5_enabled {
                        return completed(
                            sandbox_state,
                            SandboxCompletion::EmptyCode { gas_used, logs },
                        );
                    }
                    return completed(
                        sandbox_state,
                        SandboxCompletion::ExecutionFailed {
                            gas_used,
                            error: KeylessDeployError::EmptyCodeDeployed { gas_used },
                        },
                    );
                }
                completed(
                    sandbox_state,
                    SandboxCompletion::Deployed { gas_used, deploy_address: created_addr, logs },
                )
            }
            ExecutionResult::Revert { gas_used, output } => completed(
                sandbox_state,
                SandboxCompletion::ExecutionFailed {
                    gas_used,
                    error: KeylessDeployError::ExecutionReverted { gas_used, output },
                },
            ),
            ExecutionResult::Halt { gas_used, reason } => {
                // `FailedDeposit` on this path means op-revm's deposit `catch_error` wrapped a
                // sandbox tx-validation failure into a synthetic halt. Runtime halts under the
                // deposit-style sandbox tx are intercepted in `MegaHandler::execution_result`
                // and surface here with their real halt reason instead, so they fall through to
                // the normal `ExecutionHalted` failure branch and consume the Nick's-Method
                // replay barrier as expected. Validation rejects must NOT consume the barrier,
                // so we drop the discarded sandbox state and surface as Rejected.
                if matches!(reason, MegaHaltReason::Base(op_revm::OpHaltReason::FailedDeposit)) {
                    warn!(gas_used, "keyless deploy sandbox failed deposit (validation-reject)",);
                    return SandboxOutcome::Rejected(KeylessDeployError::InvalidTransaction);
                }
                // The halt `reason` is dropped on the ABI wire (the Solidity error
                // carries only `gasUsed`) and `decode_error_result` synthesizes a
                // placeholder on the way back, so node-side `tracing` is the only
                // place the real cause is observable.
                warn!(
                    reason = ?reason,
                    gas_used,
                    "keyless deploy sandbox halted",
                );
                completed(
                    sandbox_state,
                    SandboxCompletion::ExecutionFailed {
                        gas_used,
                        error: KeylessDeployError::ExecutionHalted { gas_used, reason },
                    },
                )
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
            SandboxOutcome::Rejected(KeylessDeployError::InvalidTransaction)
        }
        Err(e) => {
            error!(
                error = %e,
                "keyless deploy sandbox failed with internal error",
            );
            SandboxOutcome::Rejected(KeylessDeployError::InternalError)
        }
    }
}

/// Rex5+ post-sandbox accounting. Order is load-bearing:
///   (a) merge multidim usage         — always; survives any later halt
///   (b) merge volatile bitmap        — always; survives any later halt
///   (c) refund unused sandbox gas    — always; pure `erase_cost`, never halts
///   (d) reject if tx-level overflow  — may halt with exceeding mark
///
/// (a) and (b) run before the halt-deciding helper so the parent transaction's
/// reported footprint reflects what the sandbox actually accessed even on halt
/// paths. The caller invokes `apply_sandbox_state` only after this returns
/// `None`, following the standard "halted transactions commit only
/// pre-execution state" convention.
///
/// `reservation` is the amount pre-debited from the outer `gas` before the
/// sandbox ran (step 8b in `execute_keyless_deploy_call`); `sandbox_gas_used` is
/// the amount the sandbox actually consumed (read from
/// [`SandboxCompletion::gas_used`]). Their difference is the unused reservation
/// that must be returned to the outer frame — mirroring revm's standard CALL
/// semantics.
///
/// Caller-side spec gating keeps this path Rex5-only.
fn apply_sandbox_post_accounting<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    gas: &mut Gas,
    limit_usage: LimitUsage,
    volatile_accesses: VolatileDataAccess,
    reservation: u64,
    sandbox_gas_used: u64,
    return_memory_offset: &core::ops::Range<usize>,
) -> Option<FrameResult> {
    merge_sandbox_limit_usage(ctx, limit_usage);
    ctx.volatile_data_tracker.borrow_mut().merge_accesses_from_bitmap(volatile_accesses);
    refund_unused_sandbox_gas(gas, reservation, sandbox_gas_used);
    reject_if_tx_limit_overflow(ctx, gas, return_memory_offset)
}

/// If the deploy signer is unmaterialized in the parent journal-visible state,
/// pre-charges `new_account_storage_gas(deploy_signer)` against the outer Gas
/// counter and records a deposit-caller state-growth event. Mirrors the upfront
/// `KEYLESS_DEPLOY_OVERHEAD_GAS` charge in step 1: paid before the sandbox runs,
/// retained even when the sandbox is never constructed (e.g., sandbox-validate
/// reject), so the relayer cannot squeeze the sandbox into a budget that excludes
/// the parent's materialization-side state-growth invariant.
///
/// `signer_info` is the parent-state `AccountInfo` loaded by
/// `inspect_signer_parent_state`. Treating absent/empty alike via `is_empty()`
/// (rather than re-reading the DB) keeps the materialization decision aligned with
/// what the upstream `make_create_frame` will observe for the same signer.
///
/// Returns:
/// - `Ok(None)` when the signer was already materialized, or the charge fit and the resulting
///   state-growth event was within the parent's TX-level budget;
/// - `Ok(Some(halt))` when either the outer Gas could not absorb the charge (plain OOG), or
///   recording the state-growth event tripped a non-frame-local TX-level limit (canonical
///   exceeding-limit OOG, with parent gas rescued — see `reject_if_tx_limit_overflow`);
/// - `Err(InternalError)` when the dynamic-gas computation fails.
fn charge_caller_materialization_pre_sandbox<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    gas: &mut Gas,
    deploy_signer: Address,
    signer_info: &AccountInfo,
    return_memory_offset: &core::ops::Range<usize>,
) -> Result<Option<FrameResult>, KeylessDeployError> {
    if !signer_info.is_empty() {
        return Ok(None);
    }

    // Call `DynamicGasCost::new_account_gas` directly rather than the `HostExt` helper:
    // the helper stashes the SALT/env error into `ctx.error()`, which the outer transaction's
    // own `execution_result` would later drain into an EVM-level `Err(Custom)` — breaking
    // the spec contract that this failure surfaces as a `Revert(InternalError())` selector
    // and leaves the outer transaction otherwise well-formed.
    let caller_storage_gas =
        ctx.dynamic_storage_gas_cost.borrow_mut().new_account_gas(deploy_signer).map_err(|e| {
            error!(
                error = %e,
                deploy_signer = ?deploy_signer,
                "pre-sandbox caller storage gas computation failed",
            );
            KeylessDeployError::InternalError
        })?;
    if !gas.record_cost(caller_storage_gas) {
        return Ok(Some(oog_frame_result(gas.limit(), return_memory_offset)));
    }
    // `record_deposit_caller_creation` can latch `has_exceeded_limit` to a non-frame-local
    // `StateGrowthLimitExceeded`. Convert it to the canonical exceeding-limit OOG halt
    // (with rescued gas) here — interceptor short-circuit bypasses `after_frame_run`'s
    // standard `try_rescue_gas` hook, so any subsequent Revert-shaped error in this
    // function would let `before_frame_return_result::<true>` convert it to a Halt
    // without ever rescuing the parent gas.
    ctx.additional_limit.borrow_mut().record_deposit_caller_creation();
    if let Some(halt) = reject_if_tx_limit_overflow(ctx, gas, return_memory_offset) {
        return Ok(Some(halt));
    }
    Ok(None)
}

/// Merges the sandbox's multidim usage into the parent's trackers.
///
/// This merge is intentionally not undone on later halt paths, because
/// block-level multidim counters survive halts.
fn merge_sandbox_limit_usage<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    limit_usage: LimitUsage,
) {
    ctx.additional_limit.borrow_mut().merge_usage(limit_usage);
}

/// Returns the unused portion of the sandbox's pre-debited gas reservation to the
/// outer frame. Mirrors revm's standard CALL semantics: pre-debit on entry, refund
/// `reservation - used` on exit. Pure `Gas::erase_cost`, never halts.
fn refund_unused_sandbox_gas(gas: &mut Gas, reservation: u64, sandbox_gas_used: u64) {
    // Sandbox cannot consume more than the reservation it was given (revm's frame
    // accounting enforces `gas_used ≤ gas_limit`), but `saturating_sub` keeps the
    // arithmetic defensive against any future change in how `gas_used` is reported.
    let unused = reservation.saturating_sub(sandbox_gas_used);
    if unused > 0 {
        gas.erase_cost(unused);
    }
}

/// Rejects the outer call when already-merged sandbox usage pushes the parent
/// over a non-frame-local TX-level limit.
///
/// Returns `Some(FrameResult)` when the outer handler must short-circuit
/// (rescue remaining gas, return as exceeding-limit OOG), `None` when the
/// merged usage fits.
///
/// State must not be merged after this returns `Some(_)`; the footprint
/// side effects already applied by `apply_sandbox_post_accounting` remain.
fn reject_if_tx_limit_overflow<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    gas: &Gas,
    return_memory_offset: &core::ops::Range<usize>,
) -> Option<FrameResult> {
    let mut limit = ctx.additional_limit.borrow_mut();
    let limit_check = limit.check_limit();
    if !limit_check.exceeded_limit() || limit_check.is_frame_local() {
        return None;
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
        // `intrinsic_check_for_tx` builds a fresh trial tracker that never has `Exempt` stamped,
        // so only the two real outcomes are reachable here.
        LimitCheck::WithinLimit | LimitCheck::Exempt => None,
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

/// Loads the deploy signer's parent-state `AccountInfo` via `JournalInspectTr::inspect_account`
/// (cold-marked) for Rex5+.
///
/// The single lookup feeds the nonce check, the EIP-3607 caller-with-code check, and the
/// materialization `is_empty()` check from one source — keeping the "cold-marked, no
/// EIP-2929 pollution" contract documented in one place.
///
/// `load_code` is derived from `!cfg.disable_eip3607`: EIP-3607 needs the bytecode to
/// distinguish a real contract from an EIP-7702 delegation designation, but when
/// `disable_eip3607 = true` the canonical revm `validate_account_nonce_and_code`
/// short-circuits without touching `code_by_hash`. The nonce check reads `info.nonce`
/// and the materialization check reads `info.is_empty()` (which uses `code_hash`, not the
/// hydrated bytes), so neither requires the bytecode to be loaded.
///
/// Edge case — the `disable_eip3607 = true` no-`code_by_hash` short-circuit holds only when
/// the signer is not already present in the parent journal cache as a lazy-code occupied
/// entry. `inspect_account`'s occupied branch unconditionally invokes `code_by_hash` on
/// `code_hash != KECCAK_EMPTY && info.code.is_none()` regardless of `load_code` — this
/// hydration is load-bearing for the pre-REX5 EIP-7702 detection path in
/// `inspect_account_delegated` (see `evm/host.rs`) and changing it would shift behavior on
/// frozen specs. In the normal third-party relayer keyless-deploy flow the signer is
/// recovered from the inner RLP at step 5 of `execute_keyless_deploy_call` and the outer
/// EVM has not touched it before that, so the signer's journal entry is vacant when this
/// helper runs. If an outer transaction or replay harness has already cached the signer
/// (e.g., the relayer happens to be the signer, or a witness / stateless harness pre-fills
/// the journal with lazy-code entries), the occupied-entry caveat above applies.
fn inspect_signer_parent_state<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    address: Address,
) -> Result<AccountInfo, KeylessDeployError> {
    let load_code = !ctx.cfg().disable_eip3607;
    let account = ctx.journal_mut().inspect_account(address, load_code).map_err(|e| {
        error!(
            error = %e,
            address = ?address,
            "keyless deploy signer parent-state inspection failed",
        );
        KeylessDeployError::InternalError
    })?;
    Ok(account.info.clone())
}

/// Enforces the EIP-3607 caller-with-code rule against the provided parent-state info,
/// honoring `cfg.disable_eip3607` exactly like the canonical revm path.
///
/// The op-revm deposit path skips this check in `validate_against_state_and_deduct_caller`,
/// so the sandbox enforces it itself for Rex5+ to keep the no-contract-sender invariant.
/// The lookup itself lives in `inspect_signer_parent_state`; this function only inspects
/// the resulting info.
fn validate_signer_code<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &MegaContext<DB, ExtEnvs>,
    signer_info: &AccountInfo,
) -> Result<(), KeylessDeployError> {
    if ctx.cfg().disable_eip3607 {
        return Ok(());
    }
    if let Some(code) = &signer_info.code {
        if !code.is_empty() && !code.is_eip7702() {
            return Err(KeylessDeployError::SignerHasCode);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::result::Output;

    /// Test error type that lets us drive `process_sandbox_transact_result`'s
    /// `Err` arms (which map to `SandboxOutcome::Rejected`) directly without
    /// standing up a full sandbox EVM. The two arms differ only by
    /// `IsTxError::is_tx_error()`, so a single struct with a configurable flag
    /// covers both selector mappings.
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
        let out = process_sandbox_transact_result(
            result,
            LimitUsage::default(),
            VolatileDataAccess::empty(),
            true,
        );
        assert!(
            matches!(out, SandboxOutcome::Rejected(KeylessDeployError::NoContractCreated)),
            "unexpected: {out:?}",
        );
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
        let out = process_sandbox_transact_result(
            result,
            LimitUsage::default(),
            VolatileDataAccess::empty(),
            true,
        );
        assert!(
            matches!(out, SandboxOutcome::Rejected(KeylessDeployError::NoContractCreated)),
            "unexpected: {out:?}",
        );
    }

    /// `transact_raw` error where `is_tx_error()` is `false` (DB I/O, header validation,
    /// `EVMError::Custom`, etc.) MUST map to the selector-only `InternalError`. Pinned
    /// because a regression that re-collapses this into `InvalidTransaction` would
    /// silently re-classify infrastructure failures as user-input rejections.
    #[test]
    fn test_process_result_non_tx_error_maps_to_internal_error() {
        let result: Result<ResultAndState<MegaHaltReason>, FakeTxErr> =
            Err(FakeTxErr { is_tx: false, msg: "db blew up" });
        let out = process_sandbox_transact_result(
            result,
            LimitUsage::default(),
            VolatileDataAccess::empty(),
            true,
        );
        assert!(
            matches!(out, SandboxOutcome::Rejected(KeylessDeployError::InternalError)),
            "unexpected: {out:?}",
        );
    }

    /// Mirror of the above for `is_tx_error() == true`: MUST map to
    /// `InvalidTransaction` with its dedicated selector.
    #[test]
    fn test_process_result_tx_error_maps_to_invalid_transaction() {
        let result: Result<ResultAndState<MegaHaltReason>, FakeTxErr> =
            Err(FakeTxErr { is_tx: true, msg: "intrinsic gas too low" });
        let out = process_sandbox_transact_result(
            result,
            LimitUsage::default(),
            VolatileDataAccess::empty(),
            true,
        );
        assert!(
            matches!(out, SandboxOutcome::Rejected(KeylessDeployError::InvalidTransaction)),
            "unexpected: {out:?}",
        );
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

    /// `get_account_nonce` MUST take the journal-cache branch (without re-reading the
    /// database) when the address is already present in `journal.inner.state`. Pinned
    /// because the cache-hit branch and the DB-fallback branch return identical values on
    /// the happy path; only an end-to-end harness or a direct unit assertion can tell them
    /// apart.
    #[test]
    fn test_get_account_nonce_returns_cached_nonce_without_touching_database() {
        use crate::{
            test_utils::{ErrorInjectingDatabase, MemoryDatabase},
            EmptyExternalEnv,
        };
        use alloy_primitives::address;
        use revm::{primitives::KECCAK_EMPTY, state::AccountInfo};

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        // DB fails on every `basic(signer)` call. The cache-hit branch is the only way
        // `get_account_nonce` can return successfully — if the DB-fallback branch is taken,
        // the test fails with the injected error.
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        db.fail_on_account = Some(signer);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(db, MegaSpecId::REX5);
        // Seed the journal cache directly so `get_account_nonce` finds the signer in
        // `journal.inner.state` and short-circuits before reaching the DB fallback.
        let cached_info =
            AccountInfo { nonce: 7, balance: U256::ZERO, code_hash: KECCAK_EMPTY, code: None };
        ctx.journal_mut().inner.state.insert(signer, cached_info.into());

        let nonce = get_account_nonce(&mut ctx, signer).expect("cache hit must not error");
        assert_eq!(nonce, 7, "cache-hit nonce must reflect the seeded journal entry");
    }

    /// `inspect_signer_parent_state` MUST surface an `inspect_account` DB failure as
    /// `Err(KeylessDeployError::InternalError)`. This is the only Rex5+ pre-sandbox call
    /// site that performs the parent-state read — every downstream check (`signer_nonce`,
    /// `validate_signer_code`, materialization) inspects the returned `AccountInfo`
    /// without touching the DB again, so the `error!` `map_err` here is the only place
    /// the failure can be classified.
    #[test]
    fn test_inspect_signer_parent_state_db_error_maps_to_internal_error() {
        use crate::{
            test_utils::{ErrorInjectingDatabase, MemoryDatabase},
            EmptyExternalEnv,
        };
        use alloy_primitives::address;

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        db.fail_on_account = Some(signer);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(db, MegaSpecId::REX5);
        let out = inspect_signer_parent_state(&mut ctx, signer);
        assert!(
            matches!(out, Err(KeylessDeployError::InternalError)),
            "inspect_account DB error must map to InternalError; got {out:?}",
        );
    }

    /// `inspect_signer_parent_state` MUST NOT invoke `code_by_hash` when
    /// `cfg.disable_eip3607 = true` AND the signer is not yet present in the parent
    /// journal cache — i.e., the vacant-branch path used by the normal keyless-deploy flow
    /// when the signer is not pre-cached in the parent journal (signer recovered from inner
    /// RLP at step 5 of `execute_keyless_deploy_call`, after which only this helper
    /// inspects it).
    ///
    /// Pin the contract that the canonical revm `validate_account_nonce_and_code`
    /// short-circuits without hydrating the signer's bytecode on the vacant path; without
    /// this, a DB whose `code_by_hash` fails turns an EIP-3607-disabled precheck into
    /// `InternalError`. The pre-cached lazy-code occupied path is a documented edge case
    /// (see `inspect_signer_parent_state` docstring) — `inspect_account`'s occupied branch
    /// unconditionally hydrates and that behavior is load-bearing for stable specs.
    ///
    /// The workspace's `MemoryDatabase` eagerly inlines `AccountInfo.code` inside `basic()`,
    /// so the in-tree integration tests cannot exercise this lazy-code shape — hence a
    /// dedicated `revm::Database` impl here that mirrors reth's
    /// `StateProviderDatabase::basic` contract (`code: None`, real `code_hash`).
    #[test]
    fn test_inspect_signer_parent_state_skips_code_by_hash_on_vacant_when_eip3607_disabled() {
        use alloy_primitives::{address, keccak256, B256};
        use revm::{bytecode::Bytecode, primitives::Bytes as PrimitivesBytes};

        #[derive(Debug, Default)]
        struct LazyCodeSignerDb {
            signer: Address,
            signer_info: AccountInfo,
        }

        impl RevmDatabase for LazyCodeSignerDb {
            type Error = core::convert::Infallible;

            fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
                if address == self.signer {
                    // Mirror reth's `StateProviderDatabase::basic`: return AccountInfo with
                    // `code: None` even when `code_hash` references real on-chain bytecode.
                    Ok(Some(self.signer_info.clone()))
                } else {
                    Ok(None)
                }
            }

            fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
                panic!(
                    "code_by_hash must not be called on the vacant signer path when \
                     disable_eip3607 = true (canonical validate_account_nonce_and_code \
                     short-circuit)",
                );
            }

            fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
                Ok(U256::ZERO)
            }

            fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
                Ok(B256::ZERO)
            }
        }

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let bytecode = PrimitivesBytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
        let code_hash = keccak256(&bytecode);
        let signer_info = AccountInfo { nonce: 0, balance: U256::ZERO, code_hash, code: None };
        let db = LazyCodeSignerDb { signer, signer_info };

        let mut ctx = MegaContext::<_, crate::EmptyExternalEnv>::new(db, MegaSpecId::REX5);
        ctx.modify_cfg(|cfg| cfg.disable_eip3607 = true);

        let info = inspect_signer_parent_state(&mut ctx, signer)
            .expect("lookup must succeed without hydrating code");
        assert_eq!(info.code_hash, code_hash, "code_hash must propagate from the DB");
        assert!(
            info.code.is_none(),
            "disable_eip3607 = true must leave info.code as None on the vacant branch",
        );
    }

    /// `validate_signer_code` MUST be a no-op when `cfg.disable_eip3607 = true`, even
    /// when the provided `AccountInfo` has bytecode that would otherwise trip the
    /// caller-with-code check. Mirrors the canonical revm `validate_account_nonce_and_code`
    /// semantics and pins the early-return contract.
    #[test]
    fn test_validate_signer_code_disable_eip3607_short_circuits() {
        use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
        use revm::{
            bytecode::Bytecode,
            primitives::{keccak256, Bytes as PrimitivesBytes},
        };

        let code_bytes = PrimitivesBytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
        let code_hash = keccak256(&code_bytes);
        let signer_info = AccountInfo {
            nonce: 0,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(code_bytes)),
        };

        let mut ctx =
            MegaContext::<_, EmptyExternalEnv>::new(MemoryDatabase::default(), MegaSpecId::REX5);
        ctx.modify_cfg(|cfg| cfg.disable_eip3607 = true);

        let out = validate_signer_code(&ctx, &signer_info);
        assert!(
            matches!(out, Ok(())),
            "disable_eip3607 must short-circuit before the code inspection; got {out:?}",
        );
    }

    /// `charge_caller_materialization_pre_sandbox` MUST surface a SALT/dynamic-gas failure
    /// as `Err(KeylessDeployError::InternalError)` without writing to `ctx.error()`.
    ///
    /// The contract is load-bearing: op-revm's `Handler::execution_result` drains
    /// `ctx.error()` and converts any non-empty `ContextError` into an EVM-level
    /// `Err(Custom(...))`. Routing a SALT failure through the
    /// `HostExt::new_account_storage_gas` helper — which stashes the error into
    /// `ctx.error()` — would promote an interceptor-level failure into a whole-transaction
    /// execution error and break the `InternalError()` selector contract. The
    /// pre-sandbox charge path must therefore call `DynamicGasCost::new_account_gas`
    /// directly and keep the error local.
    #[test]
    fn test_charge_caller_materialization_salt_failure_returns_internal_error_without_ctx_pollution(
    ) {
        use crate::{test_utils::MemoryDatabase, BucketId, OracleEnv, SaltEnv};
        use alloy_primitives::{address, B256};
        use core::fmt::{self, Display, Formatter};

        // Minimal `SaltEnv` whose `get_bucket_capacity` always fails. This drives
        // `DynamicGasCost::new_account_gas` (called from
        // `charge_caller_materialization_pre_sandbox`) to return `Err`, exercising the
        // failure-handling path under test.
        #[derive(Debug, Clone, Copy)]
        struct AlwaysFailSalt;

        #[derive(Debug, Clone)]
        struct InjectedSaltError;
        impl Display for InjectedSaltError {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("injected SALT failure")
            }
        }

        impl SaltEnv for AlwaysFailSalt {
            type Error = InjectedSaltError;
            fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
                Err(InjectedSaltError)
            }
            fn bucket_id_for_account(_account: Address) -> BucketId {
                0
            }
            fn bucket_id_for_slot(_address: Address, _key: U256) -> BucketId {
                0
            }
        }

        #[derive(Debug, Clone, Copy)]
        struct NoopOracle;
        impl OracleEnv for NoopOracle {
            fn get_oracle_storage(&self, _slot: U256) -> Option<U256> {
                None
            }
            fn on_hint(&self, _from: Address, _topic: B256, _data: Bytes) {}
        }

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let db = MemoryDatabase::default();
        let envs: crate::ExternalEnvs<(AlwaysFailSalt, NoopOracle)> =
            crate::ExternalEnvs { salt_env: AlwaysFailSalt, oracle_env: NoopOracle };
        let mut ctx = MegaContext::new(db, MegaSpecId::REX5).with_external_envs(envs);

        let mut gas = Gas::new(10_000_000);
        let return_memory_offset = 0..0;

        // Signer is unmaterialized (default `AccountInfo` is `is_empty()` == true)
        // → the materialization branch fires.
        let signer_info = AccountInfo::default();
        let out = charge_caller_materialization_pre_sandbox(
            &ctx,
            &mut gas,
            signer,
            &signer_info,
            &return_memory_offset,
        );
        assert!(
            matches!(out, Err(KeylessDeployError::InternalError)),
            "SALT failure must surface as InternalError, got {out:?}",
        );

        // The whole point of the fix: ctx.error() must NOT have been polluted, so the outer
        // transaction's `execution_result` doesn't drain it into `Err(Custom)`.
        let ctx_error = core::mem::replace(ctx.error(), Ok(()));
        assert!(
            matches!(ctx_error, Ok(())),
            "ctx.error() must remain Ok after a SALT failure in the pre-sandbox charge; \
             got {ctx_error:?}",
        );

        // record_deposit_caller_creation must NOT have fired either.
        assert_eq!(
            ctx.additional_limit.borrow().get_usage().state_growth,
            0,
            "deposit-caller state-growth must not be recorded when the storage gas computation fails",
        );
    }
}
