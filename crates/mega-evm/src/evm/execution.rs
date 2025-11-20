#[cfg(not(feature = "std"))]
use alloc as std;
use std::string::ToString;

use alloy_evm::{precompiles::PrecompilesMap, Database};
use alloy_primitives::TxKind;
use delegate::delegate;
use op_revm::{
    handler::{IsTxError, OpHandler},
    OpTransactionError,
};
use revm::{
    context::{
        result::{ExecutionResult, FromStringError, InvalidTransaction},
        ContextTr, FrameStack, Transaction,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::InstructionProvider,
        EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr, Handler,
        ItemOrResult,
    },
    inspector::{InspectorEvmTr, InspectorHandler},
    interpreter::{
        gas::get_tokens_in_calldata, interpreter::EthInterpreter, interpreter_action::FrameInit,
        FrameInput, Gas, InitialAndFloorGas, InstructionResult, InterpreterAction,
    },
    Inspector, Journal,
};

use crate::{
    constants, create_exceeding_interpreter_result, create_exceeding_limit_frame_result,
    is_mega_system_transaction, mark_frame_result_as_exceeding_limit,
    mark_interpreter_result_as_exceeding_limit, sent_from_mega_system_address, ExternalEnvs,
    HostExt, MegaContext, MegaEvm, MegaHaltReason, MegaInstructions, MegaSpecId,
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
    ExtEnvs: ExternalEnvs,
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
}

impl<DB: Database, EVM, ERROR, FRAME, ExtEnvs: ExternalEnvs> Handler
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
        if is_mini_rex_enabled {
            let mut additional_limit = ctx.additional_limit().borrow_mut();
            // record the initial gas cost as compute gas cost
            if additional_limit
                .record_compute_gas(initial_and_floor_gas.initial_gas)
                .exceeded_limit()
            {
                // TODO: can we custom error?
                return Err(InvalidTransaction::CallGasCostMoreThanGasLimit {
                    gas_limit: additional_limit.compute_gas_limit,
                    initial_gas: initial_and_floor_gas.initial_gas,
                }
                .into());
            }
            drop(additional_limit);

            // MegaETH modification: additional storage gas cost for creating account
            let kind = ctx.tx().kind();
            let new_account = match kind {
                TxKind::Create => true,
                TxKind::Call(address) => {
                    !ctx.tx().value().is_zero() &&
                        match ctx.db_mut().basic(address)? {
                            Some(account) => account.is_empty(),
                            None => true,
                        }
                }
            };
            if new_account {
                let callee_address = match kind {
                    TxKind::Create => {
                        let tx = ctx.tx();
                        let caller = tx.caller();
                        let nonce = tx.nonce();
                        caller.create(nonce)
                    }
                    TxKind::Call(address) => address,
                };
                initial_and_floor_gas.initial_gas +=
                    ctx.new_account_storage_gas(callee_address).map_err(|_| {
                        let err_str = format!(
                            "Failed to get new account gas for callee address: {callee_address}",
                        );
                        Self::Error::from_string(err_str)
                    })?;
            }

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
            // Add 160,000 gas on top of base intrinsic gas for all transactions
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
        }

        Ok(initial_and_floor_gas)
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
            let mut additional_limit = evm.ctx().additional_limit.borrow_mut();
            // Update the additional limit before returning the frame result
            if additional_limit.before_frame_return_result::<true>(frame_result).exceeded_limit() {
                mark_frame_result_as_exceeding_limit(frame_result);
            }
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
                if let Some((access_type, compute_gas_limit)) = volatile_info {
                    // it's due to volatile data access
                    MegaHaltReason::VolatileDataAccessOutOfGas {
                        access_type,
                        limit: compute_gas_limit,
                        actual: additional_limit.compute_gas_tracker.current_gas_used(),
                    }
                } else {
                    // normal additional limit exceeded without volatile data access
                    additional_limit
                        .check_limit()
                        .maybe_halt_reason()
                        .expect("should have a halt reason")
                }
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

impl<DB, EVM, ERROR, ExtEnvs: ExternalEnvs> InspectorHandler
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
}

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::handler::EvmTr for MegaEvm<DB, INSP, ExtEnvs>
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
        let additional_limit = self.ctx().additional_limit.clone();
        if is_mini_rex_enabled &&
            additional_limit.borrow_mut().before_frame_init(&frame_init).exceeded_limit()
        {
            // if the limit is exceeded, create an error frame result and return it directly
            let (gas_limit, return_memory_offset) = match &frame_init.frame_input {
                FrameInput::Create(inputs) => (inputs.gas_limit, None),
                FrameInput::Call(inputs) => {
                    (inputs.gas_limit, Some(inputs.return_memory_offset.clone()))
                }
                FrameInput::Empty => unreachable!(),
            };
            return Ok(FrameInitResult::Result(create_exceeding_limit_frame_result(
                Gas::new(gas_limit),
                return_memory_offset,
            )));
        }

        // call the inner frame_init function to initialize the frame
        let init_result = self.inner.frame_init(frame_init)?;

        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if is_mini_rex_enabled {
            if let ItemOrResult::Item(frame) = &init_result {
                additional_limit.borrow_mut().after_frame_init_on_frame(frame);
            }
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

        let is_mini_rex_enabled = context.spec.is_enabled(MegaSpecId::MINI_REX);

        // Check if the additional limit is already exceeded, if so, we should immediately stop
        // and synthesize an interpreter action.
        let mut action = if is_mini_rex_enabled {
            let mut additional_limit = context.additional_limit.borrow_mut();
            if additional_limit.check_limit().exceeded_limit() {
                InterpreterAction::Return(create_exceeding_interpreter_result(
                    frame.interpreter.gas,
                ))
            } else {
                drop(additional_limit);
                frame.interpreter.run_plain(instructions.instruction_table(), context)
            }
        } else {
            frame.interpreter.run_plain(instructions.instruction_table(), context)
        };

        // Apply additional limits and storage gas cost only when the `MINI_REX` spec is enabled.
        if is_mini_rex_enabled {
            if let InterpreterAction::Return(interpreter_result) = &mut action {
                // charge storage gas cost for the number of bytes
                if frame.data.is_create() && interpreter_result.is_ok() {
                    // if the creation is successful, charge the storage gas cost for the
                    // number of bytes.
                    let code_deposit_storage_gas = constants::mini_rex::CODEDEPOSIT_STORAGE_GAS *
                        interpreter_result.output.len() as u64;
                    if !interpreter_result.gas.record_cost(code_deposit_storage_gas) {
                        // if out of gas, set the instruction result to OOG
                        interpreter_result.result = InstructionResult::OutOfGas;
                    }
                }

                // update additional limits
                let mut additional_limit = context.additional_limit.borrow_mut();
                if frame.data.is_create() &&
                    additional_limit.after_create_frame_run(interpreter_result).exceeded_limit()
                {
                    // if exceeded the limit, set the instruction result
                    mark_interpreter_result_as_exceeding_limit(interpreter_result);
                }
            }
        }

        // Record gas remaining before frame action processing
        let gas_remaining_before = match (&action, is_mini_rex_enabled) {
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

        // Record compute gas cost induced in frame action processing (e.g., code deposit cost)
        if let (ItemOrResult::Result(frame_result), Some(gas_remaining_before), true) =
            (&mut frame_output, gas_remaining_before, is_mini_rex_enabled)
        {
            let compute_gas_cost =
                gas_remaining_before.saturating_sub(frame_result.gas().remaining());
            let mut additional_limit = self.ctx().additional_limit.borrow_mut();
            if additional_limit.record_compute_gas(compute_gas_cost).exceeded_limit() {
                mark_frame_result_as_exceeding_limit(frame_result);
            }
        }

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
            let mut additional_limit = ctx.additional_limit.borrow_mut();

            // call the `on_frame_return` function to update the `AdditionalLimit` if the limit is
            // exceeded, return the error frame result
            if additional_limit.before_frame_return_result::<false>(&result).exceeded_limit() {
                match &mut result {
                    FrameResult::Call(call_outcome) => {
                        mark_interpreter_result_as_exceeding_limit(&mut call_outcome.result)
                    }
                    FrameResult::Create(create_outcome) => {
                        mark_interpreter_result_as_exceeding_limit(&mut create_outcome.result)
                    }
                }
            }
        }

        // Call the inner frame_return_result function to return the frame result.
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::inspector::InspectorEvmTr for MegaEvm<DB, INSP, ExtEnvs>
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
}
