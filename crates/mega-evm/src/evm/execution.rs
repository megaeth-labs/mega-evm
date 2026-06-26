#[cfg(not(feature = "std"))]
use alloc as std;
use std::{collections::BTreeMap, string::ToString};

use alloy_evm::{precompiles::PrecompilesMap, Database};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use delegate::delegate;
use op_revm::{
    handler::{IsTxError, OpHandler},
    transaction::deposit::DEPOSIT_TRANSACTION_TYPE,
    OpHaltReason, OpTransactionError,
};
use revm::{
    context::{
        result::{ExecutionResult, FromStringError, InvalidTransaction},
        transaction::{AuthorizationTr, TransactionType},
        Cfg, ContextError, ContextTr, FrameStack, JournalTr, LocalContextTr, Transaction,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::InstructionProvider,
        post_execution::output as post_execution_output,
        pre_execution::validate_account_nonce_and_code,
        EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr, Handler,
        ItemOrResult,
    },
    inspector::{
        handler::{frame_end, frame_start},
        inspect_instructions, InspectorEvmTr, InspectorFrame, InspectorHandler,
    },
    interpreter::{
        gas::get_tokens_in_calldata, interpreter::EthInterpreter, interpreter_action::FrameInit,
        CallOutcome, CallScheme, CreateOutcome, FrameInput, Gas, InitialAndFloorGas,
        InstructionResult, InterpreterAction, InterpreterResult,
    },
    primitives::CALL_STACK_LIMIT,
    Inspector, Journal,
};

use crate::{
    constants, dispatch_system_contract_interceptors, is_deposit_like_transaction,
    is_mega_system_transaction_with, sent_from_system_address, ExternalEnvTypes, HostExt,
    JournalInspectTr, MegaContext, MegaEvm, MegaHaltReason, MegaInstructions, MegaSpecId,
    MegaTransactionError, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
};

/// Revm handler for `MegaETH`. It internally wraps the [`op_revm::handler::OpHandler`] and inherits
/// most functionalities from Optimism.
#[allow(missing_debug_implementations)]
pub struct MegaHandler<EVM, ERROR, FRAME> {
    op: OpHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> MegaHandler<EVM, ERROR, FRAME> {
    /// Create a new `MegaethHandler`.
    pub fn new() -> Self {
        Self { op: OpHandler::new() }
    }
}

impl<EVM, ERROR, FRAME> Default for MegaHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB, EVM, ERROR, FRAME, ExtEnvs> MegaHandler<EVM, ERROR, FRAME>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    EVM: EvmTr<Context = MegaContext<DB, ExtEnvs>>,
    ERROR: FromStringError + From<InvalidTransaction>,
{
    /// The hook to be called in `revm::handler::Handler::run_without_catch_error` and
    /// `revm::handler::InspectorHandler::inspect_run_without_catch_error`.
    ///
    /// Promotes a legacy `system_address` transaction into the OP deposit-style path so it
    /// bypasses signature, nonce, and fee validation. REX5+ restores nonce and chain-id
    /// checks before the promotion (the deposit path otherwise drops them, leaving the
    /// transaction replayable). Pre-REX5 specs preserve the original behavior so existing
    /// chain replay is unaffected.
    #[inline]
    fn before_run(&self, evm: &mut EVM) -> Result<(), ERROR> {
        let ctx = evm.ctx_mut();
        let spec = ctx.spec;
        if spec.is_enabled(MegaSpecId::MINI_REX) {
            let system_address = ctx.system_address;
            let is_rex5_enabled = spec.is_enabled(MegaSpecId::REX5);
            // Honor the same `CfgEnv` toggles as the canonical revm validate path.
            // Ordinary txs are already filtered by the upstream validate path before
            // reaching this promotion logic, so keeping system txs aligned here does
            // not introduce a separate replay-only escape hatch.
            let cfg = ctx.cfg();
            let cfg_chain_id = cfg.chain_id;
            let tx_chain_id_check = cfg.tx_chain_id_check;
            let disable_nonce_check = cfg.disable_nonce_check;
            let disable_eip3607 = cfg.disable_eip3607;
            let tx = ctx.tx();

            if sent_from_system_address(tx, system_address) {
                // Whitelist rejection has no canonical `InvalidTransaction` variant; keep the
                // existing string-error shape pre-REX5 callers already expect.
                if !is_mega_system_transaction_with(tx, system_address) {
                    return Err(FromStringError::from_string(
                        "Mega system transaction callee is not in the whitelist".to_string(),
                    ));
                }

                if is_rex5_enabled {
                    if tx_chain_id_check {
                        match tx.chain_id() {
                            None => return Err(InvalidTransaction::MissingChainId.into()),
                            Some(cid) if cid != cfg_chain_id => {
                                return Err(InvalidTransaction::InvalidChainId.into());
                            }
                            Some(_) => {}
                        }
                    }

                    // Inspect without warming so validation does not mutate the EIP-2929
                    // access list. The journal cache still lets consecutive in-block system
                    // txs observe committed nonce bumps.
                    let tx_nonce = tx.nonce();
                    // EIP-3607 reads `info.code`; pass `load_code = true` so the
                    // `code_by_hash` is paid here rather than silently bypassing the
                    // guard against a lazy-code DB.
                    let state_account = ctx
                        .journal_mut()
                        .inspect_account(system_address, true)
                        .map_err(|e| -> ERROR {
                            FromStringError::from_string(format!(
                                "Mega system transaction state read failed: {e:?}"
                            ))
                        })?;
                    validate_account_nonce_and_code(
                        &mut state_account.info,
                        tx_nonce,
                        disable_eip3607,
                        disable_nonce_check,
                    )?;
                }

                // Mark the tx as deposit-style for `OpHandler` and force gas_price to 0
                // so fee / L1 / operator / beneficiary accounting all degenerate to no-ops.
                let tx = &mut ctx.inner.tx;
                tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
                tx.base.gas_price = 0;
            }
        }

        ctx.on_new_tx();
        Ok(())
    }

    /// The hook to be called in `revm::handler::Handler::execution` and
    /// `revm::inspector::InspectorHandler::inspect_execution` to check if the initial gas exceeds
    /// the tx gas limit, if so, we halt with out of gas.
    #[inline]
    fn before_execution(
        &self,
        evm: &mut EVM,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<Option<FrameResult>, ERROR> {
        // Check if the initial gas exceeds the tx gas limit, if so, we halt with out of gas
        let ctx = evm.ctx();
        let tx = ctx.tx();
        if tx.gas_limit() < init_and_floor_gas.initial_gas {
            // If not sufficient gas, we halt with out of gas
            let oog_frame_result = gen_oog_frame_result(tx.kind(), tx.gas_limit());
            return Ok(Some(oog_frame_result));
        }
        Ok(None)
    }
}

impl<DB, EVM, ERROR, FRAME, ExtEnvs> MegaHandler<EVM, ERROR, FRAME>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
    EVM: EvmTr<Context = MegaContext<DB, ExtEnvs>>,
    ERROR: From<DB::Error> + FromStringError,
{
    /// Records REX5 state growth for EIP-7702 authorizations that create authority accounts.
    ///
    /// The scan mirrors revm's auth-list validation order but stops before mutating delegation
    /// bytecode. `before_tx_start()` cannot do this because it has no journal/DB access.
    /// Any overflow is latched into `AdditionalLimit::has_exceeded_limit` and converted into the
    /// normal execution failure when the first frame is initialized.
    #[inline]
    fn record_eip7702_authority_state_growth(&self, evm: &mut EVM) -> Result<(), ERROR> {
        let ctx = evm.ctx_mut();
        if !ctx.spec.is_enabled(MegaSpecId::REX5) || ctx.tx().tx_type() != TransactionType::Eip7702
        {
            return Ok(());
        }

        let chain_id = ctx.cfg().chain_id;
        let authority_creations = {
            let (tx, journal) = ctx.tx_journal_mut();
            let mut authority_creations = 0;
            // Transaction-local simulated auth-list state, mirroring revm's sequential
            // processing when the same authority appears multiple times in one tx.
            // A `BTreeMap` keeps per-authorization lookup/update at O(log N) instead of
            // the O(N) linear scan a `Vec` would need, bounding the whole pass at
            // O(N log N) (an attacker could otherwise drive O(N²) node CPU with ~1200
            // unique authorities in one tx). The map is only ever keyed, never iterated for
            // output, so the produced `authority_creations` count is unchanged.
            let mut simulated_authorities = BTreeMap::<Address, u64>::new();
            for authorization in tx.authorization_list() {
                let auth_chain_id = authorization.chain_id();
                if !auth_chain_id.is_zero() && auth_chain_id != U256::from(chain_id) {
                    continue;
                }
                if authorization.nonce() == u64::MAX {
                    continue;
                }
                let Some(authority) = authorization.authority() else {
                    continue;
                };

                let (authority_nonce, creates_authority) =
                    if let Some(nonce) = simulated_authorities.get(&authority).copied() {
                        (nonce, false)
                    } else {
                        let authority_acc = journal.load_account_code(authority)?;
                        if let Some(bytecode) = &authority_acc.info.code {
                            if !bytecode.is_empty() && !bytecode.is_eip7702() {
                                continue;
                            }
                        }
                        (
                            authority_acc.info.nonce,
                            authority_acc.is_empty() &&
                                authority_acc.is_loaded_as_not_existing_not_touched(),
                        )
                    };

                if authorization.nonce() != authority_nonce {
                    continue;
                }

                if creates_authority {
                    authority_creations += 1;
                }
                let next_nonce = authority_nonce.saturating_add(1);
                // insert overwrites an existing entry and inserts a new one otherwise,
                // matching the prior find-or-push.
                simulated_authorities.insert(authority, next_nonce);
            }
            authority_creations
        };
        if authority_creations > 0 {
            ctx.additional_limit.borrow_mut().on_eip7702_authority_creations(authority_creations);
        }

        Ok(())
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
    /// This is the hook to be called in the beginning of the `frame_run` and `inspect_frame_run`
    /// functions. This function checks if the additional limit is already exceeded, if so, we
    /// should immediately stop and synthesize an interpreter action and return it.
    #[inline]
    fn before_frame_run(
        ctx: &MegaContext<DB, ExtEnvs>,
        frame: &EthFrame<EthInterpreter>,
    ) -> Result<Option<InterpreterAction>, ContextDbError<MegaContext<DB, ExtEnvs>>> {
        // Check if the additional limit is already exceeded, if so, we should immediately stop
        // and synthesize an interpreter action.
        if ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            if let Some(interpreter_result) =
                ctx.additional_limit.borrow_mut().before_frame_run(frame)
            {
                return Ok(Some(InterpreterAction::Return(interpreter_result)));
            }
        }
        Ok(None)
    }

    /// This is the hook to be called in the `frame_run` and `inspect_frame_run`
    /// functions after the instructions are executed. Apply `MiniRex` additional limits after
    /// running instructions.
    ///
    /// This handles:
    /// - Charging `CODEDEPOSIT_STORAGE_GAS` for successful create operations
    /// - Updating additional limits via `after_create_frame_run`
    /// - Recording gas remaining for later compute gas tracking
    ///
    /// Returns `Some(gas_remaining)` if `MiniRex` is enabled and action is a Return,
    /// for use in `after_frame_run`.
    #[inline]
    fn after_frame_run_instructions(
        ctx: &MegaContext<DB, ExtEnvs>,
        frame: &EthFrame<EthInterpreter>,
        action: &mut InterpreterAction,
    ) -> Result<(), ContextDbError<MegaContext<DB, ExtEnvs>>> {
        if !ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            return Ok(());
        }
        let is_rex5 = ctx.spec.is_enabled(MegaSpecId::REX5);

        if let InterpreterAction::Return(interpreter_result) = action {
            // Charge storage gas cost for the number of bytes
            if frame.data.is_create() && interpreter_result.is_ok() {
                let code_deposit_storage_gas = constants::mini_rex::CODEDEPOSIT_STORAGE_GAS *
                    interpreter_result.output.len() as u64;
                if !interpreter_result.gas.record_cost(code_deposit_storage_gas) {
                    interpreter_result.result = InstructionResult::OutOfGas;
                }
            }

            // REX5+: pre-charge canonical code-deposit compute gas before
            // process_next_action commits the CREATE checkpoint. Skip when
            // revm's return_create would not charge it; the existing
            // limit-side hook below owns the result-marking on exceed.
            if is_rex5 && frame.data.is_create() {
                let cfg = ctx.cfg();
                if will_return_create_charge_code_deposit(
                    interpreter_result,
                    cfg.max_code_size(),
                    cfg.spec().into(),
                    cfg.is_eip3541_disabled(),
                ) {
                    let code_len = interpreter_result.output.len() as u64;
                    let canonical_code_deposit_gas =
                        code_len.saturating_mul(revm::interpreter::gas::CODEDEPOSIT);
                    let _ = ctx
                        .additional_limit
                        .borrow_mut()
                        .record_compute_gas(canonical_code_deposit_gas);
                }
            }
        }

        // Update additional limits. MiniRex is guaranteed to be enabled here.
        ctx.additional_limit.borrow_mut().after_frame_run_instructions(frame, action);

        Ok(())
    }

    /// Apply `MiniRex` additional limits after frame action processing.
    ///
    /// Under REX5+ for CREATE results, the code-deposit compute gas was
    /// already pre-charged in [`after_frame_run_instructions`]; pass
    /// `None` here so the post-action hook does not double-record.
    #[inline]
    fn after_frame_run(
        ctx: &MegaContext<DB, ExtEnvs>,
        frame_output: &mut ItemOrResult<FrameInit, FrameResult>,
        gas_remaining_before_process_action: Option<u64>,
    ) -> Result<(), ContextDbError<MegaContext<DB, ExtEnvs>>> {
        if !ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            return Ok(());
        }
        let is_rex5 = ctx.spec.is_enabled(MegaSpecId::REX5);

        if let ItemOrResult::Result(frame_result) = frame_output {
            // REX5+: code-deposit compute gas for CREATE results was already
            // pre-charged. Skip post-action recording so we don't double-count.
            let pass_through = if is_rex5 && matches!(frame_result, FrameResult::Create(_)) {
                None
            } else {
                gas_remaining_before_process_action
            };
            ctx.additional_limit.borrow_mut().after_frame_run(frame_result, pass_through);
        }

        Ok(())
    }
}

/// Mirrors `revm_handler::frame::return_create`'s pre-commit predicate.
/// Returns `true` iff `return_create` would charge `code_len * CODEDEPOSIT`
/// from the interpreter gas and commit the checkpoint.
///
/// REVIEW ON UPSTREAM BUMP: keep in lockstep with
/// `revm-handler::frame::return_create`. Any revm bump that touches the
/// predicate inputs (`is_ok`, EIP-3541 gate, EIP-170 gate, code-deposit
/// gas availability) requires re-auditing this helper.
fn will_return_create_charge_code_deposit(
    interpreter_result: &InterpreterResult,
    max_code_size: usize,
    runtime_spec_id: revm::primitives::hardfork::SpecId,
    is_eip3541_disabled: bool,
) -> bool {
    use revm::primitives::hardfork::SpecId;

    if !interpreter_result.result.is_ok() {
        return false;
    }
    if !is_eip3541_disabled &&
        runtime_spec_id.is_enabled_in(SpecId::LONDON) &&
        interpreter_result.output.first() == Some(&0xEF)
    {
        return false;
    }
    if runtime_spec_id.is_enabled_in(SpecId::SPURIOUS_DRAGON) &&
        interpreter_result.output.len() > max_code_size
    {
        return false;
    }
    let code_deposit_gas = (interpreter_result.output.len() as u64)
        .saturating_mul(revm::interpreter::gas::CODEDEPOSIT);
    interpreter_result.gas.remaining() >= code_deposit_gas
}

impl<DB: Database, EVM, ERROR, FRAME, ExtEnvs: ExternalEnvTypes> Handler
    for MegaHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context = MegaContext<DB, ExtEnvs>, Frame = FRAME>,
    ERROR: EvmTrError<EVM>
        + From<OpTransactionError>
        + From<MegaTransactionError>
        + FromStringError
        + IsTxError
        + core::fmt::Debug,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;

    type Error = ERROR;

    type HaltReason = MegaHaltReason;

    delegate! {
        to self.op {
            fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn validate_against_state_and_deduct_caller(
                &self,
                evm: &mut Self::Evm,
            ) -> Result<(), Self::Error>;
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<(), Self::Error>;
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult, eip7702_refund: i64);
        }
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        self.validate_against_state_and_deduct_caller(evm)?;
        self.load_accounts(evm)?;
        self.record_eip7702_authority_state_growth(evm)?;
        self.apply_eip7702_auth_list(evm)
    }

    fn run_system_call(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // system call does not call `pre_execution` and `post_execution`, so we need to extract
        // some logic from them.
        let ctx = evm.ctx_mut();
        ctx.on_new_tx();

        // dummy values that are not used.
        let init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        // call execution and than output.
        match self
            .execution(evm, &init_and_floor_gas)
            .and_then(|exec_result| self.execution_result(evm, exec_result))
        {
            out @ Ok(_) => out,
            Err(e) => self.catch_error(evm, e),
        }
    }

    fn run_without_catch_error(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        self.before_run(evm)?;

        let init_and_floor_gas = self.validate(evm)?;
        let eip7702_refund = self.pre_execution(evm)? as i64;
        let mut exec_result = self.execution(evm, &init_and_floor_gas)?;
        self.post_execution(evm, &mut exec_result, init_and_floor_gas, eip7702_refund)?;

        // Prepare the output
        self.execution_result(evm, exec_result)
    }

    /// This function copies the logic from `revm::handler::Handler::validate` to and
    /// add additional storage gas cost for calldata.
    ///
    /// REX5+ adds a final initial+floor gas validation after all Mega-side dynamic storage gas
    /// has been accounted for. Pre-REX5 specs keep the historical mid-sequence check exactly
    /// where it was so byte-for-byte replay is preserved.
    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        self.validate_env(evm)?;
        let mut initial_and_floor_gas = self.validate_initial_tx_gas(evm)?;

        let ctx = evm.ctx_mut();
        let is_mini_rex_enabled = ctx.spec.is_enabled(MegaSpecId::MINI_REX);
        let is_rex_enabled = ctx.spec.is_enabled(MegaSpecId::REX);
        let is_rex5_enabled = ctx.spec.is_enabled(MegaSpecId::REX5);
        if is_mini_rex_enabled {
            // record the initial gas cost as compute gas cost, limit exceeding will be captured in
            // `frame_init` function.
            ctx.additional_limit()
                .borrow_mut()
                .record_compute_gas(initial_and_floor_gas.initial_gas);

            // MegaETH MiniRex modification: calldata storage gas costs (10x the standard EVM rates)
            // - Standard tokens: 40 gas per token (vs 4)
            // - EIP-7623 floor: 100 gas per token (vs 10)
            let tokens_in_calldata = get_tokens_in_calldata(ctx.tx().input(), true);
            let calldata_storage_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_GAS * tokens_in_calldata;
            initial_and_floor_gas.initial_gas += calldata_storage_gas;
            let floor_calldata_storage_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_FLOOR_GAS * tokens_in_calldata;
            initial_and_floor_gas.floor_gas += floor_calldata_storage_gas;

            // MegaETH Rex modification: additional intrinsic storage gas cost
            // Add 39,000 gas on top of base intrinsic gas for all transactions
            if is_rex_enabled {
                initial_and_floor_gas.initial_gas += constants::rex::TX_INTRINSIC_STORAGE_GAS;
            }

            // Pre-REX5: keep the historical mid-sequence initial-gas check here so existing
            // stable-spec replays produce exactly the same OOG-after-fee-charge result on
            // transactions whose final Mega-adjusted initial_gas exceeds gas_limit only after
            // CREATE/new-account storage gas is added below.
            //
            // REX5+: this mid-sequence check is deferred to the final check below, which runs
            // after CREATE/new-account storage gas has also been added so a transaction that
            // cannot fit its final Mega-side intrinsic+storage gas is rejected as a validation
            // error before pre_execution() debits the sender or bumps the nonce.
            if !is_rex5_enabled && initial_and_floor_gas.initial_gas > ctx.tx().gas_limit() {
                return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                    gas_limit: ctx.tx().gas_limit(),
                    initial_gas: initial_and_floor_gas.initial_gas,
                }
                .into());
            }

            // MegaETH modification: additional storage gas cost for creating account
            let kind = ctx.tx().kind();
            let is_rex5_enabled = ctx.spec.is_enabled(MegaSpecId::REX5);
            let (callee_address, storage_gas) = match kind {
                TxKind::Create => {
                    let caller = ctx.tx().caller();
                    // REX5+: derive the created address from the caller's
                    // state nonce — the same value `make_create_frame` uses
                    // for the actual deployment. Pre-REX5 keeps `tx.nonce()`.
                    let nonce = if is_rex5_enabled {
                        ctx.journal_mut()
                            .inspect_account(caller, false)
                            .map_err(|e| {
                                Self::Error::from_string(format!(
                                    "Failed to inspect caller account for CREATE storage gas: {e:?}",
                                ))
                            })?
                            .info
                            .nonce
                    } else {
                        ctx.tx().nonce()
                    };
                    let created_address = caller.create(nonce);

                    let storage_gas = if is_rex_enabled {
                        // Rex spec distinguishes between contract creation and account creation.
                        ctx.create_contract_storage_gas(created_address)
                    } else {
                        // Mini-Rex spec does not distinguish between contract creation and account
                        // creation.
                        ctx.new_account_storage_gas(created_address)
                    };
                    (created_address, storage_gas)
                }
                TxKind::Call(address) => {
                    let new_account = !ctx.tx().value().is_zero() &&
                        ctx.db_mut().basic(address)?.is_none_or(|acc| acc.is_empty());
                    let storage_gas =
                        if new_account { ctx.new_account_storage_gas(address) } else { Some(0) };
                    (address, storage_gas)
                }
            };
            initial_and_floor_gas.initial_gas += storage_gas.ok_or_else(|| {
                let err_str =
                    format!("Failed to get storage gas for callee address: {callee_address}",);
                Self::Error::from_string(err_str)
            })?;

            // REX5+: charge dynamic new-account storage gas for a deposit-driven caller
            // materialisation (either `tx.mint() > 0` balance increment or pre-execution
            // nonce bump). Mirrors the `TxKind::Call(address) with value` branch above,
            // but for the caller side. Detection runs here so we observe the pre-
            // pre-execution state — `OpHandler::pre_execution` (run after `validate`) is
            // what actually materialises the caller account.
            //
            // `data_size` / `kv_update` are intentionally NOT touched: their
            // `before_tx_start` hooks already record the caller's account-info write
            // unconditionally for every transaction. Only `state_growth` (which has no
            // pre-existing caller-side accounting) and intrinsic gas need the charge.
            // Skip this branch inside sandbox contexts: the sandbox view of `caller`'s nonce
            // is overridden to 0 by `SandboxDb`, which would mis-classify a previously
            // materialised signer as empty on retry. The keyless-deploy outer flow charges
            // caller materialisation explicitly via `charge_caller_materialization_pre_sandbox`
            // before constructing the sandbox tx, based on the parent journal-visible state.
            if is_rex5_enabled && !ctx.is_inside_sandbox() {
                let caller = ctx.tx().caller();
                let system_address = ctx.system_address;
                if is_deposit_like_transaction(&ctx.inner.tx, system_address) {
                    let caller_is_empty =
                        ctx.db_mut().basic(caller)?.is_none_or(|acc| acc.is_empty());
                    if caller_is_empty {
                        // Self-call corner: if the deposit is a `TxKind::Call(caller)` with
                        // non-zero `value` AND the same empty caller as callee, the existing
                        // callee branch above has already charged `new_account_storage_gas(caller)`
                        // for the same account materialisation. Don't charge the gas a second
                        // time — but still record the state-growth event (the existing branch
                        // never records state_growth; the +1 here reflects the single account
                        // materialisation that pre_execution will perform).
                        let already_charged_as_callee = matches!(
                            ctx.tx().kind(),
                            TxKind::Call(addr) if addr == caller,
                        ) && !ctx.tx().value().is_zero();
                        if !already_charged_as_callee {
                            let storage_gas =
                                ctx.new_account_storage_gas(caller).ok_or_else(|| {
                                    let err_str = format!(
                                        "Failed to get storage gas for deposit caller: {caller}",
                                    );
                                    Self::Error::from_string(err_str)
                                })?;
                            initial_and_floor_gas.initial_gas += storage_gas;
                        }
                        ctx.additional_limit.borrow_mut().record_deposit_caller_creation();
                    }
                }
            }

            // REX5+: final initial+floor gas validation, after every Mega-side storage gas
            // contribution has been added. A transaction that fails either bound is rejected
            // here as a canonical validation error so callers see Err(...) rather than an
            // ExecutionResult::Halt with full gas spent — i.e. fees and nonce stay untouched
            // when the tx cannot fit its final intrinsic+storage gas requirement.
            if is_rex5_enabled {
                let gas_limit = ctx.tx().gas_limit();
                if initial_and_floor_gas.initial_gas > gas_limit {
                    return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                        gas_limit,
                        initial_gas: initial_and_floor_gas.initial_gas,
                    }
                    .into());
                }
                if initial_and_floor_gas.floor_gas > gas_limit {
                    return Err(InvalidTransaction::GasFloorMoreThanGasLimit {
                        gas_limit,
                        gas_floor: initial_and_floor_gas.floor_gas,
                    }
                    .into());
                }
            }
        }

        Ok(initial_and_floor_gas)
    }

    /// This function copies the logic from `revm::handler::Handler::execution` to and
    /// add new account storage gas
    #[inline]
    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        if let Some(oog_frame_result) = self.before_execution(evm, init_and_floor_gas)? {
            return Ok(oog_frame_result);
        }

        let gas_limit = evm.ctx().tx().gas_limit() - init_and_floor_gas.initial_gas;
        // Create first frame action
        let first_frame_input = self.first_frame_input(evm, gas_limit)?;

        // Run execution loop
        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;

        // Handle last frame result
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if evm.ctx().disable_beneficiary {
            Ok(())
        } else {
            self.op.reward_beneficiary(evm, exec_result)
        }
    }

    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let is_mini_rex = evm.ctx().spec.is_enabled(MegaSpecId::MINI_REX);
        if is_mini_rex {
            // Update the additional limit before returning the frame result
            evm.ctx().additional_limit.borrow_mut().before_frame_return_result::<true>(frame_result)
        }

        // Call the inner last_frame_result function first
        // This will finalize gas accounting according to REVM's rules:
        // - Spends all gas_limit
        // - Only refunds remaining gas if is_ok_or_revert()
        self.op.last_frame_result(evm, frame_result)?;

        // After REVM's gas accounting, we need to return the rescued gas from additional limits.
        if is_mini_rex {
            let ctx = evm.ctx_mut();

            let additional_limit = ctx.additional_limit.borrow();
            let gas = frame_result.gas_mut();
            gas.erase_cost(additional_limit.rescued_gas);
        }

        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // Capture volatile data info for error reporting
        let volatile_info = evm
            .ctx()
            .spec
            .is_enabled(MegaSpecId::MINI_REX)
            .then(|| {
                let volatile_data_tracker = evm.ctx().volatile_data_tracker.borrow();
                volatile_data_tracker.get_volatile_data_info()
            })
            .flatten();

        // Deposit-style sandbox txs: bypass op-revm's HaltedDepositPostRegolith conversion so
        // that a runtime halt surfaces as `Ok(Halt(actual_reason, actual_gas_used))` instead of
        // being squashed into `FailedDeposit(gas_limit)`. The keyless-deploy outer flow needs
        // the real halt reason to distinguish runtime halts (which must merge sandbox state and
        // charge `sandbox_gas_used` against the outer gas counter) from validation-rejects
        // (which still flow through `catch_error` and produce `FailedDeposit`).
        let result = if evm.ctx().is_inside_sandbox() &&
            evm.ctx().tx().tx_type() == DEPOSIT_TRANSACTION_TYPE
        {
            match core::mem::replace(evm.ctx().error(), Ok(())) {
                Err(ContextError::Db(e)) => return Err(e.into()),
                Err(ContextError::Custom(e)) => return Err(Self::Error::from_string(e)),
                Ok(_) => (),
            }
            let exec_result =
                post_execution_output(evm.ctx(), result).map_haltreason(OpHaltReason::Base);
            evm.ctx().journal_mut().commit_tx();
            evm.ctx().chain_mut().clear_tx_l1_cost();
            evm.ctx().local_mut().clear();
            evm.frame_stack().clear();
            exec_result
        } else {
            self.op.execution_result(evm, result)?
        };
        Ok(result.map_haltreason(|reason| {
            let mut additional_limit = evm.ctx().additional_limit.borrow_mut();
            if additional_limit.is_exceeding_limit_halt(&reason) {
                if let Some(access_type) = volatile_info {
                    if let Some(halt) =
                        additional_limit.detained_compute_gas_halt_reason(access_type)
                    {
                        return halt;
                    }
                }
                // normal additional limit exceeded (no volatile data access, or detention
                // was not more restrictive than the per-tx compute gas limit)
                additional_limit
                    .check_limit()
                    .maybe_halt_reason()
                    .expect("should have a halt reason")
            } else {
                // not due to additional limit exceeded
                MegaHaltReason::Base(reason)
            }
        }))
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let result = self.op.catch_error(evm, error)?;
        Ok(result.map_haltreason(MegaHaltReason::Base))
    }
}

impl<DB, EVM, ERROR, ExtEnvs: ExternalEnvTypes> InspectorHandler
    for MegaHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    DB: Database,
    MegaContext<DB, ExtEnvs>: ContextTr<Journal = Journal<DB>>,
    Journal<DB>: revm::inspector::JournalExt,
    EVM: InspectorEvmTr<
        Context = MegaContext<DB, ExtEnvs>,
        Frame = EthFrame<EthInterpreter>,
        Inspector: Inspector<
            <<Self as revm::handler::Handler>::Evm as EvmTr>::Context,
            EthInterpreter,
        >,
    >,
    ERROR: EvmTrError<EVM>
        + From<OpTransactionError>
        + From<MegaTransactionError>
        + FromStringError
        + IsTxError
        + core::fmt::Debug,
{
    type IT = EthInterpreter;

    fn inspect_run_without_catch_error(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        self.before_run(evm)?;

        let init_and_floor_gas = self.validate(evm)?;
        let eip7702_refund = self.pre_execution(evm)? as i64;
        let mut frame_result = self.inspect_execution(evm, &init_and_floor_gas)?;
        self.post_execution(evm, &mut frame_result, init_and_floor_gas, eip7702_refund)?;
        self.execution_result(evm, frame_result)
    }

    /// This function copies the logic from `Handler::execution` to add
    /// new account storage gas and early OOG check with inspector support.
    #[inline]
    fn inspect_execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        if let Some(oog_frame_result) = self.before_execution(evm, init_and_floor_gas)? {
            return Ok(oog_frame_result);
        }

        let gas_limit = evm.ctx().tx().gas_limit() - init_and_floor_gas.initial_gas;
        // Create first frame action
        let first_frame_input = self.first_frame_input(evm, gas_limit)?;

        // Run execution loop with inspector
        let mut frame_result = self.inspect_run_exec_loop(evm, first_frame_input)?;

        // Handle last frame result
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::handler::EvmTr for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
{
    type Context = MegaContext<DB, ExtEnvs>;

    type Instructions = MegaInstructions<DB, ExtEnvs>;

    type Precompiles = PrecompilesMap;

    type Frame = EthFrame<EthInterpreter>;

    #[inline]
    fn ctx(&mut self) -> &mut Self::Context {
        &mut self.inner.ctx
    }

    #[inline]
    fn ctx_ref(&self) -> &Self::Context {
        &self.inner.ctx
    }

    #[inline]
    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        (&mut self.inner.ctx, &mut self.inner.instruction)
    }

    #[inline]
    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        (&mut self.inner.ctx, &mut self.inner.precompiles)
    }

    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        &mut self.inner.frame_stack
    }

    fn frame_init(
        &mut self,
        mut frame_init: <Self::Frame as revm::handler::FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let is_mini_rex_enabled = self.ctx().spec.is_enabled(MegaSpecId::MINI_REX);
        let is_rex_enabled = self.ctx().spec.is_enabled(MegaSpecId::REX);
        let is_rex3_enabled = self.ctx().spec.is_enabled(MegaSpecId::REX3);
        let is_rex4_enabled = self.ctx().spec.is_enabled(MegaSpecId::REX4);
        let is_rex5_enabled = self.ctx().spec.is_enabled(MegaSpecId::REX5);
        let additional_limit = self.ctx().additional_limit.clone();

        // Check if this is a call to the oracle contract and mark it as accessed.
        // This handles both direct transaction calls and internal CALL operations.
        // Rex3+: Oracle access gas detention is triggered by SLOAD (not CALL), so skip this
        // CALL-based check for Rex3 and later specs.
        //
        // The check uses `target_address` which equals the oracle address for CALL and
        // STATICCALL, but equals the caller's address for CALLCODE and DELEGATECALL (since
        // those execute in the caller's state context). CALLCODE and DELEGATECALL are therefore
        // never detected here by design — they do not access oracle state.
        //
        // MiniRex: Only CALL triggers oracle access detection. STATICCALL, CALLCODE, and
        //   DELEGATECALL bypass it.
        // Rex: STATICCALL is added to oracle access detection (unifying CALL-like behavior).
        if is_mini_rex_enabled && !is_rex3_enabled {
            if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                let detect_oracle = match call_inputs.scheme {
                    CallScheme::Call => true,
                    // Rex fixes the bug in MiniRex where STATICCALL bypasses oracle access
                    // detection.
                    CallScheme::StaticCall => is_rex_enabled,
                    // CALLCODE and DELEGATECALL have target_address = caller (not oracle),
                    // so check_and_mark_oracle_access would never match anyway.
                    CallScheme::CallCode | CallScheme::DelegateCall => false,
                };
                // Mega system address is exempted from volatile data access enforcement.
                if detect_oracle && call_inputs.caller != self.ctx().system_address {
                    let volatile_data_tracker = self.ctx().volatile_data_tracker.clone();
                    let mut tracker = volatile_data_tracker.borrow_mut();
                    if tracker.check_and_mark_oracle_access(&call_inputs.target_address) {
                        if let Some(compute_gas_limit) = tracker.get_compute_gas_limit() {
                            additional_limit.borrow_mut().set_compute_gas_limit(compute_gas_limit);
                        }
                    }
                }
            }
        }

        // REX4+: If a TX-level limit is already exceeded (e.g., intrinsic DataSize/KVUpdate
        // overflow from before_tx_start), abort before interceptor dispatch. Interceptors
        // return synthetic results that skip before_frame_init(), which would otherwise
        // catch the exceeded limit.
        //
        // Gated to REX4 only: pre-REX4 specs use TX-global check_limit() which catches
        // intrinsic overflow during execution. Changing pre-REX4 behavior would break replay.
        if is_rex4_enabled {
            // Separate borrow scope: the RefMut must be dropped before push_empty_frame
            // borrows again.
            let exceeded = additional_limit
                .borrow_mut()
                .frame_result_if_exceeding_limit(&frame_init.frame_input);
            if let Some(frame_result) = exceeded {
                additional_limit.borrow_mut().push_empty_frame();
                return Ok(FrameInitResult::Result(frame_result));
            }
        }

        // REX5+: enforce `CALL_STACK_LIMIT` before interceptor dispatch. Interceptors
        // short-circuit before revm's `make_call_frame` runs its own depth check, so
        // without this guard a system contract could be invoked at unbounded depth.
        // Scope mirrors interceptor dispatch (Call/StaticCall only); other schemes still
        // flow into revm where its own depth check applies.
        if is_rex5_enabled {
            if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                if matches!(call_inputs.scheme, CallScheme::Call | CallScheme::StaticCall) &&
                    frame_init.depth > CALL_STACK_LIMIT as usize
                {
                    let frame_result = gen_call_too_deep_result(call_inputs);
                    additional_limit.borrow_mut().push_empty_frame();
                    return Ok(FrameInitResult::Result(frame_result));
                }
            }
        }

        // System contract interception dispatch.
        // Each interceptor checks target address and ABI-decodes function selectors.
        // Side-effect interceptors (oracle hint) usually return None.
        // Short-circuiting paths return Some(FrameResult).
        // These synthetic results skip `AdditionalLimit::before_frame_init`; we only push an
        // empty tracking frame to keep the additional-limit stacks aligned.
        //
        // Only `CALL` and `STATICCALL` enter interceptor dispatch. `CALLCODE` and
        // `DELEGATECALL` execute in the caller's state context, so intercepting them
        // would apply system-contract logic in the wrong state context — the scheme
        // guard enforces this policy explicitly rather than relying on upstream
        // call-frame semantics to keep `target_address` distinct.
        if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
            if matches!(call_inputs.scheme, CallScheme::Call | CallScheme::StaticCall) {
                if let Some(result) =
                    dispatch_system_contract_interceptors(self.ctx(), call_inputs, frame_init.depth)
                {
                    // Push an empty frame to keep the limit tracker stack balanced:
                    // `frame_return_result` / `last_frame_result` will pop a frame, but
                    // `after_frame_init` (which normally pushes) was skipped.
                    if is_mini_rex_enabled {
                        additional_limit.borrow_mut().push_empty_frame();
                    }
                    return Ok(FrameInitResult::Result(result));
                }
            }
        }

        if is_mini_rex_enabled {
            if let Some(frame_result) = additional_limit
                .borrow_mut()
                .before_frame_init(&mut frame_init, self.ctx().journal_mut())?
            {
                return Ok(FrameInitResult::Result(frame_result));
            }
        }

        // call the inner frame_init function to initialize the frame
        let init_result = self.inner.frame_init(frame_init)?;

        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if is_mini_rex_enabled {
            additional_limit.borrow_mut().after_frame_init(&init_result);
        }

        Ok(init_result)
    }

    /// This method copies the logic from `revm::handler::EvmTr::frame_run` to and add additional
    /// logic before `process_next_action` to handle the additional limit.
    #[inline]
    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let frame = self.inner.frame_stack.get();
        let context = &mut self.inner.ctx;
        let instructions = &mut self.inner.instruction;

        // Before frame_run Hook
        let mut action = if let Some(action) = Self::before_frame_run(context, frame)? {
            action
        } else {
            frame.interpreter.run_plain(instructions.instruction_table(), context)
        };

        // After frame_run instructions Hook
        Self::after_frame_run_instructions(context, frame, &mut action)?;

        // Record gas remaining before frame action processing
        let gas_remaining_before = match (&action, context.spec.is_enabled(MegaSpecId::MINI_REX)) {
            (InterpreterAction::Return(interpreter_result), true) => {
                Some(interpreter_result.gas.remaining())
            }
            _ => None,
        };

        // Process the frame action, it may need to create a new frame or return the current frame
        // result.
        let mut frame_output = frame
            .process_next_action::<_, ContextDbError<Self::Context>>(context, action)
            .inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;

        // After frame_run Hook
        Self::after_frame_run(context, &mut frame_output, gas_remaining_before)?;

        Ok(frame_output)
    }

    fn frame_return_result(
        &mut self,
        mut result: <Self::Frame as revm::handler::FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as revm::handler::FrameTr>::FrameResult>,
        ContextDbError<Self::Context>,
    > {
        let ctx = self.ctx_ref();
        let is_mini_rex = ctx.spec.is_enabled(MegaSpecId::MINI_REX);
        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if is_mini_rex {
            // call the `on_frame_return` function to update the `AdditionalLimit` if the limit is
            // exceeded, return the error frame result
            ctx.additional_limit.borrow_mut().before_frame_return_result::<false>(&mut result);
        }

        // Call the inner frame_return_result function to return the frame result.
        let ret = self.inner.frame_return_result(result)?;

        // Rex4+: Re-enable volatile data access when the disabling frame has returned.
        // The inner handler has already popped the frame and committed/reverted the journal,
        // so journal depth is decremented at this point. If it dropped below disable_depth,
        // the frame that invoked disableVolatileDataAccess() has returned and the disable
        // should no longer restrict sibling calls.
        if self.ctx_ref().spec.is_enabled(MegaSpecId::REX4) {
            let depth = self.ctx_ref().journal_ref().depth();
            self.ctx_ref().volatile_data_tracker.borrow_mut().enable_access_if_returning(depth);
        }

        Ok(ret)
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::inspector::InspectorEvmTr
    for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
    type Inspector = INSP;

    fn inspector(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inspector
    }

    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        (&mut self.inner.ctx, &mut self.inner.inspector)
    }

    fn ctx_inspector_frame(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame) {
        (&mut self.inner.ctx, &mut self.inner.inspector, self.inner.frame_stack.get())
    }

    fn ctx_inspector_frame_instructions(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame, &mut Self::Instructions) {
        (
            &mut self.inner.ctx,
            &mut self.inner.inspector,
            self.inner.frame_stack.get(),
            &mut self.inner.instruction,
        )
    }

    /// Override `inspect_frame_init` to handle the case when inspector returns early.
    ///
    /// When an inspector's `call` or `create` hook returns `Some(outcome)`, the default
    /// implementation returns early without calling `frame_init`. This means no frame is
    /// pushed to the additional limit trackers. However, `frame_return_result` will still
    /// be called and expect to pop a frame.
    ///
    /// To keep the frame stacks aligned, we push a dummy frame when inspector returns early.
    #[inline]
    fn inspect_frame_init(
        &mut self,
        mut frame_init: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector) = self.ctx_inspector();
        let is_mini_rex_enabled = ctx.spec.is_enabled(MegaSpecId::MINI_REX);
        let is_rex4_enabled = ctx.spec.is_enabled(MegaSpecId::REX4);
        let is_rex5_enabled = ctx.spec.is_enabled(MegaSpecId::REX5);

        // Check if inspector wants to skip this call/create
        if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            // Inspector intercepted — `frame_init()` is skipped entirely, so neither
            // `frame_result_if_exceeding_limit` nor `before_frame_init` would run.
            //
            // The priority order below mirrors `frame_init`'s exact order so that a
            // TX-level additional-limit exceed is reported instead of being shadowed by
            // a CallTooDeep guard:
            //   1. TX-level limit exceed (REX4+)
            //   2. CALL_STACK_LIMIT depth guard (REX5+)
            //   3. Deliver the inspector's synthetic output
            // Each early-return path calls `frame_end` to keep inspector callbacks paired.

            // (1) REX4+: if a TX-level limit is already exceeded (e.g., intrinsic
            // overflow), abort to ensure correct gas rescue before inspector callbacks.
            // Gated to REX4 to avoid changing stable spec behavior.
            if is_rex4_enabled {
                let exceeded = ctx
                    .additional_limit
                    .borrow_mut()
                    .frame_result_if_exceeding_limit(&frame_init.frame_input);
                if let Some(mut frame_result) = exceeded {
                    ctx.additional_limit.borrow_mut().push_empty_frame();
                    frame_end(ctx, inspector, &frame_init.frame_input, &mut frame_result);
                    return Ok(ItemOrResult::Result(frame_result));
                }
            }
            // (2) REX5+: enforce CALL_STACK_LIMIT for Call/StaticCall so an inspector
            // cannot deliver a synthetic call result at unbounded depth, mirroring the
            // protection added to `frame_init` before interceptor dispatch.
            if is_rex5_enabled {
                if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                    if matches!(call_inputs.scheme, CallScheme::Call | CallScheme::StaticCall) &&
                        frame_init.depth > CALL_STACK_LIMIT as usize
                    {
                        let mut frame_result = gen_call_too_deep_result(call_inputs);
                        ctx.additional_limit.borrow_mut().push_empty_frame();
                        frame_end(ctx, inspector, &frame_init.frame_input, &mut frame_result);
                        return Ok(ItemOrResult::Result(frame_result));
                    }
                }
            }
            // (3) MINI_REX+: push empty frame to keep the limit tracker stack balanced
            // (`before_frame_return_result` will pop).
            if is_mini_rex_enabled {
                ctx.additional_limit.borrow_mut().push_empty_frame();
            }
            frame_end(ctx, inspector, &frame_init.frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        // Normal path - delegate to frame_init (which pushes a real frame)
        let frame_input = frame_init.frame_input.clone();
        if let ItemOrResult::Result(mut output) = self.frame_init(frame_init)? {
            let (ctx, inspector) = self.ctx_inspector();
            frame_end(ctx, inspector, &frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        // Frame created successfully - initialize the interpreter
        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        inspector.initialize_interp(frame.interpreter(), ctx);
        Ok(ItemOrResult::Item(frame))
    }

    /// This method copies the logic from `MegaEvm::frame_run` with inspector support.
    /// It adds the same additional limit checks while using `inspect_instructions` instead of
    /// `run_plain`.
    #[inline]
    fn inspect_frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let (ctx, inspector, frame, instructions) = self.ctx_inspector_frame_instructions();

        let mut action = if let Some(action) = Self::before_frame_run(ctx, frame)? {
            action
        } else {
            inspect_instructions(
                ctx,
                frame.interpreter(),
                inspector,
                instructions.instruction_table(),
            )
        };

        // Apply additional limits and storage gas cost
        Self::after_frame_run_instructions(ctx, frame, &mut action)?;

        // Record gas remaining before frame action processing
        let gas_remaining_before = match (&action, ctx.spec.is_enabled(MegaSpecId::MINI_REX)) {
            (InterpreterAction::Return(interpreter_result), true) => {
                Some(interpreter_result.gas.remaining())
            }
            _ => None,
        };

        // Process the frame action, it may need to create a new frame or return the current frame
        // result.
        let mut frame_output = frame
            .process_next_action::<_, ContextDbError<Self::Context>>(ctx, action)
            .inspect(|i| {
                if i.is_result() {
                    frame.set_finished(true);
                }
            })?;

        // After frame_run Hook
        Self::after_frame_run(ctx, &mut frame_output, gas_remaining_before)?;

        // Call frame_end for inspector callback
        if let ItemOrResult::Result(frame_result) = &mut frame_output {
            let (ctx, inspector, frame) = self.ctx_inspector_frame();
            frame_end(ctx, inspector, frame.frame_input(), frame_result);
        }

        Ok(frame_output)
    }
}

/// Builds a `FrameResult` matching revm's `make_call_frame` `CallTooDeep` return:
/// `Gas::new(gas_limit)` (no spend, fully refundable to caller via `erase_cost`),
/// empty output, and the caller's `return_memory_offset`.
///
/// Used by the REX5+ depth guard that runs before system-contract interceptor dispatch.
/// Interceptors short-circuit before revm's own depth check, so without this guard a
/// system contract could be invoked at any call-stack depth.
fn gen_call_too_deep_result(call_inputs: &revm::interpreter::CallInputs) -> FrameResult {
    FrameResult::Call(CallOutcome::new(
        InterpreterResult::new(
            InstructionResult::CallTooDeep,
            Bytes::new(),
            Gas::new(call_inputs.gas_limit),
        ),
        call_inputs.return_memory_offset.clone(),
    ))
}

/// Builds a top-level `FrameResult` for the case where `validate()` returned an
/// `initial_gas` that exceeds the transaction's `gas_limit` by the time
/// `before_execution` re-checks it.
///
/// The frame result carries `InstructionResult::OutOfGas` with `Gas::new_spent(gas_limit)`
/// (entire tx budget burnt, no remaining), matching how an EVM-level OOG halt is
/// represented for top-level transactions. The `FrameResult` variant (`Call` vs `Create`)
/// is chosen by `tx_kind` so the downstream output helper can extract the right
/// fields without re-matching on transaction kind.
///
/// Called from `MegaHandler::before_execution` when `tx.gas_limit() < init_gas`,
/// which can happen after `MegaHandler::validate` has added any MegaETH-specific
/// intrinsic gas (calldata storage gas, REX intrinsic storage gas, callee-side
/// new-account storage gas, or the REX5+ deposit-caller storage gas).
fn gen_oog_frame_result(tx_kind: TxKind, gas_limit: u64) -> FrameResult {
    match tx_kind {
        TxKind::Call(_address) => FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::OutOfGas,
                Bytes::new(),
                Gas::new_spent(gas_limit),
            ),
            Default::default(),
        )),
        TxKind::Create => FrameResult::Create(CreateOutcome::new(
            InterpreterResult::new(
                InstructionResult::OutOfGas,
                Bytes::new(),
                Gas::new_spent(gas_limit),
            ),
            None,
        )),
    }
}
