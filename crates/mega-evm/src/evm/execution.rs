#[cfg(not(feature = "std"))]
use alloc as std;
use std::string::ToString;

use alloy_evm::{precompiles::PrecompilesMap, Database};
use alloy_primitives::{Bytes, TxKind};
use alloy_sol_types::SolCall;
use delegate::delegate;
use op_revm::{
    handler::{IsTxError, OpHandler},
    OpTransactionError,
};
use revm::{
    context::{
        result::{ExecutionResult, FromStringError, InvalidTransaction},
        ContextTr, FrameStack, JournalTr, Transaction,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::InstructionProvider,
        EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr, Handler,
        ItemOrResult,
    },
    inspector::{
        handler::{frame_end, frame_start},
        inspect_instructions, InspectorEvmTr, InspectorFrame, InspectorHandler,
    },
    interpreter::{
        gas::get_tokens_in_calldata, interpreter::EthInterpreter, interpreter_action::FrameInit,
        CallOutcome, CreateOutcome, FrameInput, Gas, InitialAndFloorGas, InstructionResult,
        InterpreterAction, InterpreterResult,
    },
    Inspector, Journal,
};

use crate::{
    constants, is_mega_system_transaction, sandbox::execute_keyless_deploy_call,
    sent_from_mega_system_address, ExternalEnvTypes, HostExt, IKeylessDeploy, IMegaAccessControl,
    IOracle, MegaContext, MegaEvm, MegaHaltReason, MegaInstructions, MegaSpecId,
    MegaTransactionError, OracleEnv, TxRuntimeLimit, ACCESS_CONTROL_ADDRESS,
    DISABLED_BY_PARENT_REVERT_DATA, KEYLESS_DEPLOY_ADDRESS, MEGA_SYSTEM_ADDRESS,
    MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, ORACLE_CONTRACT_ADDRESS,
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
    ERROR: FromStringError,
{
    /// The hook to be called in `revm::handler::Handler::run_without_catch_error` and
    /// `revm::handler::InspectorHandler::inspect_run_without_catch_error`
    #[inline]
    fn before_run(&self, evm: &mut EVM) -> Result<(), ERROR> {
        // Before validation, we need to properly set the mega system transaction
        let ctx = evm.ctx_mut();
        if ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            // Check if this is a mega system address transaction
            let tx = &mut ctx.inner.tx;
            if sent_from_mega_system_address(tx) {
                // Modify the transaction to make it appear as a deposit transaction
                // This will cause the OpHandler to automatically bypass signature validation,
                // nonce verification, and fee deduction during validation
                if !is_mega_system_transaction(tx) {
                    return Err(FromStringError::from_string(
                        "Mega system transaction callee is not in the whitelist".to_string(),
                    ));
                }

                // Set the deposit source hash of the transaction to mark it as a deposit
                // transaction for `OpHandler`.
                // The implementation of `revm::context_interface::Transaction` trait for
                // `MegaTransaction` determines the tx type by the existence of the source
                // hash.
                tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
                // Set gas_price to 0 so the transaction doesn't pay L2 execution gas,
                // consistent with OP deposit transaction behavior where gas is pre-paid on L1.
                tx.base.gas_price = 0;
            }
        }

        // Call the `on_new_tx` hook to initialize the transaction context.
        evm.ctx_mut().on_new_tx();

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

        if let InterpreterAction::Return(interpreter_result) = action {
            // Charge storage gas cost for the number of bytes
            if frame.data.is_create() && interpreter_result.is_ok() {
                let code_deposit_storage_gas = constants::mini_rex::CODEDEPOSIT_STORAGE_GAS *
                    interpreter_result.output.len() as u64;
                if !interpreter_result.gas.record_cost(code_deposit_storage_gas) {
                    interpreter_result.result = InstructionResult::OutOfGas;
                }
            }
        }

        // Update additional limits. MiniRex is guaranteed to be enabled here.
        ctx.additional_limit.borrow_mut().after_frame_run_instructions(frame, action);

        Ok(())
    }

    /// Apply `MiniRex` additional limits after frame action processing.
    ///
    /// This records compute gas cost induced in frame action processing (e.g., code deposit cost)
    /// and marks the frame result as exceeding limit if needed.
    #[inline]
    fn after_frame_run(
        ctx: &MegaContext<DB, ExtEnvs>,
        frame_output: &mut ItemOrResult<FrameInit, FrameResult>,
        gas_remaining_before_process_action: Option<u64>,
    ) -> Result<(), ContextDbError<MegaContext<DB, ExtEnvs>>> {
        if !ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            return Ok(());
        }

        if let ItemOrResult::Result(frame_result) = frame_output {
            ctx.additional_limit
                .borrow_mut()
                .after_frame_run(frame_result, gas_remaining_before_process_action);
        }

        Ok(())
    }
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
            fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error>;
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<(), Self::Error>;
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult, eip7702_refund: i64);
        }
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
    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        self.validate_env(evm)?;
        let mut initial_and_floor_gas = self.validate_initial_tx_gas(evm)?;

        let ctx = evm.ctx_mut();
        let is_mini_rex_enabled = ctx.spec.is_enabled(MegaSpecId::MINI_REX);
        let is_rex_enabled = ctx.spec.is_enabled(MegaSpecId::REX);
        if is_mini_rex_enabled {
            // record the initial gas cost as compute gas cost, limit exceeding will be captured in
            // `frame_init` function.
            ctx.additional_limit()
                .borrow_mut()
                .record_compute_gas(initial_and_floor_gas.initial_gas);

            // MegaETH MiniRex modification: calldata storage gas costs
            // - Standard tokens: 400 gas per token (vs 4)
            // - EIP-7623 floor: 100x increase for transaction data floor cost
            let tokens_in_calldata = get_tokens_in_calldata(ctx.tx().input(), true);
            let calldata_storage_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_GAS * tokens_in_calldata;
            initial_and_floor_gas.initial_gas += calldata_storage_gas;
            let floor_calldata_storage_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_FLOOR_GAS * tokens_in_calldata;
            initial_and_floor_gas.floor_gas += floor_calldata_storage_gas;

            // MegaETH Rex modification: additional intrinsic storage gas cost
            // Add 39,000 gas on top of base intrinsic gas for all transactions
            if ctx.spec.is_enabled(MegaSpecId::REX) {
                initial_and_floor_gas.initial_gas += constants::rex::TX_INTRINSIC_STORAGE_GAS;
            }

            // If the initial_gas exceeds the tx gas limit, return an error
            if initial_and_floor_gas.initial_gas > ctx.tx().gas_limit() {
                return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                    gas_limit: ctx.tx().gas_limit(),
                    initial_gas: initial_and_floor_gas.initial_gas,
                }
                .into());
            }

            // MegaETH modification: additional storage gas cost for creating account
            let kind = ctx.tx().kind();
            let (callee_address, storage_gas) = match kind {
                TxKind::Create => {
                    let tx = ctx.tx();
                    let caller = tx.caller();
                    let nonce = tx.nonce();
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

        let result = self.op.execution_result(evm, result)?;
        Ok(result.map_haltreason(|reason| {
            let mut additional_limit = evm.ctx().additional_limit.borrow_mut();
            if additional_limit.is_exceeding_limit_halt(&reason) {
                if let Some((access_type, volatile_compute_gas_limit)) = volatile_info {
                    let actual = additional_limit.compute_gas.tx_usage();
                    if actual > volatile_compute_gas_limit {
                        return MegaHaltReason::VolatileDataAccessOutOfGas {
                            access_type,
                            limit: volatile_compute_gas_limit,
                            actual,
                        };
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
        frame_init: <Self::Frame as revm::handler::FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let is_mini_rex_enabled = self.ctx().spec.is_enabled(MegaSpecId::MINI_REX);
        let is_rex3_enabled = self.ctx().spec.is_enabled(MegaSpecId::REX3);
        let additional_limit = self.ctx().additional_limit.clone();

        // Check if this is a call to the oracle contract and mark it as accessed.
        // This handles both direct transaction calls and internal CALL operations.
        // Rex3+: Oracle access gas detention is triggered by SLOAD (not CALL), so skip this
        // CALL-based check for Rex3 and later specs.
        if is_mini_rex_enabled && !is_rex3_enabled {
            if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                // Mega system address is exempted from volatile data access enforcement.
                if call_inputs.caller != MEGA_SYSTEM_ADDRESS {
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

        // Oracle Hint Mechanism (Rex2+):
        // Intercept sendHint(bytes32,bytes) calls to the oracle contract and forward them
        // to the oracle service backend via OracleEnv::on_hint.
        if self.ctx().spec.is_enabled(MegaSpecId::REX2) {
            if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                if call_inputs.target_address == ORACLE_CONTRACT_ADDRESS {
                    let input_bytes = call_inputs.input.bytes(self.ctx());
                    if let Ok(call) = IOracle::sendHintCall::abi_decode(&input_bytes) {
                        self.ctx().oracle_env.borrow().on_hint(
                            call_inputs.caller,
                            call.topic,
                            call.data,
                        );
                    }
                }
            }
        }

        // Keyless Deploy Interception (Rex2+):
        // Intercept keylessDeploy(bytes) calls to the keyless deploy contract.
        // This executes the deployment in a sandbox and applies state changes.
        // Only intercept TOP-LEVEL calls; internal calls from contracts are NOT intercepted.
        if self.ctx().spec.is_enabled(MegaSpecId::REX2) {
            // Only intercept if:
            // 1. Sandbox is not disabled (prevents infinite recursion)
            // 2. This is a top-level call (depth == 0)
            if !self.ctx().is_sandbox_disabled() && frame_init.depth == 0 {
                if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                    if call_inputs.target_address == KEYLESS_DEPLOY_ADDRESS {
                        let input_bytes = call_inputs.input.bytes(self.ctx());
                        if let Ok(call) =
                            IKeylessDeploy::keylessDeployCall::abi_decode(&input_bytes)
                        {
                            let result = execute_keyless_deploy_call(
                                self.ctx(),
                                call_inputs,
                                &call.keylessDeploymentTransaction,
                                call.gasLimitOverride,
                            );
                            return Ok(FrameInitResult::Result(result));
                        }
                    }
                }
            }
        }

        // Access Control Interception (Rex4+):
        // Intercept calls to the MegaAccessControl system contract.
        if self.ctx().spec.is_enabled(MegaSpecId::REX4) {
            if let FrameInput::Call(call_inputs) = &frame_init.frame_input {
                if call_inputs.target_address == ACCESS_CONTROL_ADDRESS {
                    let input_bytes = call_inputs.input.bytes(self.ctx());
                    // At this point (before inner.frame_init runs), journal.depth() equals
                    // frame_init.depth, which is the caller's journal depth.
                    let caller_journal_depth = frame_init.depth;

                    // disableVolatileDataAccess(): activates the volatile data access
                    // disable at the caller's depth. The caller and all inner calls will
                    // revert when accessing volatile data.
                    if IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&input_bytes)
                        .is_ok()
                    {
                        self.ctx()
                            .volatile_data_tracker
                            .borrow_mut()
                            .disable_access(caller_journal_depth);

                        additional_limit.borrow_mut().push_empty_frame();
                        let result = FrameResult::Call(CallOutcome::new(
                            InterpreterResult::new(
                                InstructionResult::Return,
                                Bytes::new(),
                                Gas::new(call_inputs.gas_limit),
                            ),
                            call_inputs.return_memory_offset.clone(),
                        ));
                        return Ok(FrameInitResult::Result(result));
                    }

                    // enableVolatileDataAccess(): re-enables volatile data access.
                    // Reverts with DisabledByParent() if a parent frame disabled it.
                    if IMegaAccessControl::enableVolatileDataAccessCall::abi_decode(&input_bytes)
                        .is_ok()
                    {
                        let success = self
                            .ctx()
                            .volatile_data_tracker
                            .borrow_mut()
                            .enable_access(caller_journal_depth);

                        additional_limit.borrow_mut().push_empty_frame();
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
                        return Ok(FrameInitResult::Result(result));
                    }

                    // isVolatileDataAccessDisabled(): queries whether volatile data access
                    // is disabled at the caller's depth.
                    if IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_decode(
                        &input_bytes,
                    )
                    .is_ok()
                    {
                        let disabled = self
                            .ctx()
                            .volatile_data_tracker
                            .borrow()
                            .volatile_access_disabled(caller_journal_depth);

                        let output = IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&disabled);

                        additional_limit.borrow_mut().push_empty_frame();
                        let result = FrameResult::Call(CallOutcome::new(
                            InterpreterResult::new(
                                InstructionResult::Return,
                                Bytes::from(output),
                                Gas::new(call_inputs.gas_limit),
                            ),
                            call_inputs.return_memory_offset.clone(),
                        ));
                        return Ok(FrameInitResult::Result(result));
                    }
                }
            }
        }

        if is_mini_rex_enabled {
            if let Some(frame_result) = additional_limit
                .borrow_mut()
                .before_frame_init(&frame_init, self.ctx().journal_mut())?
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

        // Check if inspector wants to skip this call/create
        if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            // Inspector intercepted — `after_frame_init` (which normally pushes a tracking
            // frame) was skipped, but `before_frame_return_result` (which pops) will still
            // run. Push an empty frame to keep the limit tracker stack balanced.
            if ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
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

fn gen_oog_frame_result(tx_kind: TxKind, gas_limit: u64) -> FrameResult {
    // If not sufficient gas, we halt with out of gas
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
