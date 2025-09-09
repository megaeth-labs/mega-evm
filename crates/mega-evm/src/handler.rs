use alloy_evm::Database;
use delegate::delegate;
use op_revm::handler::{IsTxError, OpHandler};
use revm::{
    context::{
        result::{ExecutionResult, FromStringError, InvalidTransaction, ResultAndState},
        Cfg, ContextTr, Transaction,
    },
    handler::{EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr},
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler, JournalExt},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, FrameInput, InitialAndFloorGas,
    },
    Inspector, Journal,
};

use crate::{constants, Context, ExternalEnvOracle, HaltReason, SpecId, TransactionError};

/// Revm handler for `MegaETH`. It internally wraps the [`op_revm::handler::OpHandler`] and inherits
/// most functionalities from Optimism.
#[allow(missing_debug_implementations)]
pub struct Handler<EVM, ERROR, FRAME> {
    op: OpHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> Handler<EVM, ERROR, FRAME> {
    /// Create a new `MegaethHandler`.
    pub fn new() -> Self {
        Self { op: OpHandler::new() }
    }
}

impl<EVM, ERROR, FRAME> Default for Handler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, EVM, ERROR, FRAME, Oracle: ExternalEnvOracle> revm::handler::Handler
    for Handler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context = Context<DB, Oracle>, Frame = FRAME>,
    ERROR: EvmTrError<EVM> + From<TransactionError> + FromStringError + IsTxError,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;

    type Error = ERROR;

    type HaltReason = HaltReason;

    delegate! {
        to self.op {
            fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn validate_against_state_and_deduct_caller(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn last_frame_result(&mut self, evm: &mut Self::Evm, frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<(), Self::Error>;
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<(), Self::Error>;
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult, eip7702_refund: i64);
            fn execution_result(&mut self, evm: &mut Self::Evm, result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<ExecutionResult<Self::HaltReason>, Self::Error>;
            fn catch_error(&self, evm: &mut Self::Evm, error: Self::Error) -> Result<ExecutionResult<Self::HaltReason>, Self::Error>;
        }
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        evm.ctx().on_new_tx();

        self.op.pre_execution(evm)
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
}

impl<DB, EVM, ERROR, Oracle: ExternalEnvOracle> InspectorHandler
    for Handler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    DB: Database,
    Context<DB, Oracle>: ContextTr<Journal = Journal<DB>>,
    Journal<DB>: revm::inspector::JournalExt,
    EVM: InspectorEvmTr<
        Context = Context<DB, Oracle>,
        Frame = EthFrame<EthInterpreter>,
        Inspector: Inspector<
            <<Self as revm::handler::Handler>::Evm as EvmTr>::Context,
            EthInterpreter,
        >,
    >,
    ERROR: EvmTrError<EVM> + From<TransactionError> + FromStringError + IsTxError,
{
    type IT = EthInterpreter;
}
