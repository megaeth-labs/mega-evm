use alloy_evm::Database;
use delegate::delegate;
use op_revm::{
    handler::{IsTxError, OpHandler},
    OpHaltReason, OpTransactionError,
};
use revm::{
    context::{
        result::{
            ExecutionResult, FromStringError, InvalidTransaction, OutOfGasError, ResultAndState,
        },
        Cfg, ContextTr, Transaction,
    },
    handler::{validation, EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr},
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler, JournalExt},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, FrameInput, InitialAndFloorGas,
    },
    Inspector, Journal,
};

use crate::{
    constants, EthHaltReason, ExternalEnvOracle, MegaContext, MegaHaltReason, MegaSpecId,
    MegaTransactionError,
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

impl<DB: Database, EVM, ERROR, FRAME, Oracle: ExternalEnvOracle> revm::handler::Handler
    for MegaHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context = MegaContext<DB, Oracle>, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<OpTransactionError> + FromStringError + IsTxError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;

    type Error = ERROR;

    type HaltReason = MegaHaltReason;

    delegate! {
        to self.op {
            fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn validate_against_state_and_deduct_caller(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<(), Self::Error>;
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult, eip7702_refund: i64);
        }
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        evm.ctx().on_new_tx();
        self.op.pre_execution(evm)
    }

    /// This function copies the logic from `revm::handler::Handler::validate_initial_tx_gas` to and
    /// add additional gas cost for calldata.
    fn validate_initial_tx_gas(&self, evm: &Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        let ctx = evm.ctx_ref();

        let mut initial_and_floor_gas =
            validation::validate_initial_tx_gas(ctx.tx(), ctx.cfg().spec().into())?;

        // MegaETH modification: additional gas cost for calldata
        let additional_calldata_gas =
            constants::mini_rex::CALLDATA_ADDITIONAL_GAS * ctx.tx().input().len() as u64;
        initial_and_floor_gas.initial_gas += additional_calldata_gas;

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
        if evm.ctx().spec.is_enabled(MegaSpecId::MINI_REX) &&
            evm.ctx()
                .additional_limit
                .borrow_mut()
                .before_frame_return_result(frame_result, true)
                .exceeded_limit()
        {
            return Ok(());
        }

        self.op.last_frame_result(evm, frame_result)
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let result = self.op.execution_result(evm, result)?;
        Ok(result.map_haltreason(|reason| match reason {
            OpHaltReason::Base(EthHaltReason::OutOfGas(OutOfGasError::Basic)) => {
                // if it halts due to OOG, we further check if the data or kv update limit is
                // exceeded
                evm.ctx()
                    .additional_limit
                    .borrow_mut()
                    .check_limit()
                    .maybe_halt_reason()
                    .unwrap_or(MegaHaltReason::Base(reason))
            }
            _ => MegaHaltReason::Base(reason),
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

impl<DB, EVM, ERROR, Oracle: ExternalEnvOracle> InspectorHandler
    for MegaHandler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    DB: Database,
    MegaContext<DB, Oracle>: ContextTr<Journal = Journal<DB>>,
    Journal<DB>: revm::inspector::JournalExt,
    EVM: InspectorEvmTr<
        Context = MegaContext<DB, Oracle>,
        Frame = EthFrame<EthInterpreter>,
        Inspector: Inspector<
            <<Self as revm::handler::Handler>::Evm as EvmTr>::Context,
            EthInterpreter,
        >,
    >,
    ERROR: EvmTrError<EVM> + From<OpTransactionError> + FromStringError + IsTxError,
{
    type IT = EthInterpreter;
}
