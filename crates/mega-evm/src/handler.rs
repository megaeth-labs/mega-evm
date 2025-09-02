use alloy_evm::Database;
use delegate::delegate;
use op_revm::handler::{IsTxError, OpHandler};
use revm::{
    context::{
        result::{
            ExecutionResult, FromStringError, HaltReason as BaseHaltReason, InvalidTransaction,
            OutOfGasError, ResultAndState,
        },
        Cfg, ContextTr, Transaction,
    },
    handler::{EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr},
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler, JournalExt},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, FrameInput, InitialAndFloorGas,
    },
    Inspector, Journal,
};

use crate::{constants, Context, HaltReason, SpecId, TransactionError};

/// Revm handler for `MegaETH`. It internally wraps the [`op_revm::handler::OpHandler`] and inherits
/// most functionalities from Optimism.
#[allow(missing_debug_implementations)]
pub struct Handler<EVM, ERROR, FRAME> {
    op: OpHandler<EVM, ERROR, FRAME>,
    /// Whether to disable the post-transaction reward to beneficiary.
    disable_beneficiary: bool,
}

impl<EVM, ERROR, FRAME> Handler<EVM, ERROR, FRAME> {
    /// Create a new `MegaethHandler`.
    pub fn new(disable_beneficiary: bool) -> Self {
        Self { op: OpHandler::new(), disable_beneficiary }
    }
}

impl<EVM, ERROR, FRAME> Default for Handler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new(false)
    }
}

impl<DB: Database, EVM, ERROR, FRAME> revm::handler::Handler for Handler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context = Context<DB>, Frame = FRAME>,
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
            fn catch_error(&self, evm: &mut Self::Evm, error: Self::Error) -> Result<ExecutionResult<Self::HaltReason>, Self::Error>;
        }
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let mut exec_result = self.op.execution_result(evm, result)?;

        // Apply gas limit enforcement for transactions that accessed beneficiary
        if evm.ctx().has_accessed_beneficiary_balance() {
            if let ExecutionResult::Halt {
                reason: HaltReason::Base(BaseHaltReason::OutOfGas(OutOfGasError::Basic)),
                ..
            } = &exec_result
            {
                // Determine if OutOfGas was due to enforcement or natural gas limit
                let tx_gas_limit = evm.ctx().tx().gas_limit();

                if tx_gas_limit <= constants::mini_rex::BENEFICIARY_GAS_LIMIT {
                    // Natural OutOfGas - transaction had low gas limit, keep original
                } else {
                    // Enforcement OutOfGas - use InvalidOperand to distinguish from natural
                    // OutOfGas
                    exec_result = ExecutionResult::Halt {
                        reason: HaltReason::Base(BaseHaltReason::OutOfGas(
                            OutOfGasError::InvalidOperand,
                        )),
                        gas_used: constants::mini_rex::BENEFICIARY_GAS_LIMIT,
                    };
                }
            }
        }

        Ok(exec_result)
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        evm.ctx().log_data_size = 0;
        // Reset block env access for new transaction execution
        evm.ctx().reset_block_env_access();
        // Check beneficiary access for the current transaction
        evm.ctx().check_tx_beneficiary_access();
        self.op.pre_execution(evm)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if self.disable_beneficiary {
            Ok(())
        } else {
            self.op.reward_beneficiary(evm, exec_result)
        }
    }
}

impl<DB, EVM, ERROR> InspectorHandler for Handler<EVM, ERROR, EthFrame<EthInterpreter>>
where
    DB: Database,
    Context<DB>: ContextTr<Journal = Journal<DB>>,
    Journal<DB>: revm::inspector::JournalExt,
    EVM: InspectorEvmTr<
        Context = Context<DB>,
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
