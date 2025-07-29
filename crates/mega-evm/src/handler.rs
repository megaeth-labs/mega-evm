use alloy_evm::Database;
use delegate::delegate;
use op_revm::handler::{IsTxError, OpHandler};
use revm::{
    context::{
        result::{FromStringError, InvalidTransaction, ResultAndState},
        Cfg, ContextTr, Transaction,
    },
    handler::{EvmTr, EvmTrError, Frame, FrameInitOrResult, FrameOrResult, FrameResult},
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, FrameInput, InitialAndFloorGas},
    Inspector,
};

use crate::{constants, Context, HaltReason, SpecId, TransactionError};

/// Revm handler for `MegaETH`. It internally wraps the [`op_revm::handler::OpHandler`] and inherits
/// most functionalities from Optimism.
#[allow(missing_debug_implementations)]
pub struct Handler<EVM, ERROR, FRAME> {
    /// The `MegaethEvm` spec id. This field is need because the `EVM` type passed to `OpHandler`
    /// is `OpContextTr`, which contains `OpSpecId` instead of `MegaethSpecId`. Although the actual
    /// `MegaethSpecId` exists in the `EVM` type, it is not visible here.
    spec: SpecId,
    op: OpHandler<EVM, ERROR, FRAME>,
    /// Whether to disable the post-transaction reward to beneficiary.
    disable_beneficiary: bool,
}

impl<EVM, ERROR, FRAME> Handler<EVM, ERROR, FRAME> {
    /// Create a new `MegaethHandler`.
    pub fn new(spec: SpecId, disable_beneficiary: bool) -> Self {
        Self { op: OpHandler::new(), spec, disable_beneficiary }
    }
}

impl<EVM, ERROR, FRAME> Default for Handler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new(SpecId::default(), false)
    }
}

impl<DB: Database, EVM, ERROR, FRAME> revm::handler::Handler for Handler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context = Context<DB>>,
    ERROR: EvmTrError<EVM> + From<TransactionError> + FromStringError + IsTxError,
    FRAME: Frame<Evm = EVM, Error = ERROR, FrameResult = FrameResult, FrameInit = FrameInput>,
{
    type Evm = EVM;

    type Error = ERROR;

    type Frame = FRAME;

    type HaltReason = HaltReason;

    delegate! {
        to self.op {
            fn validate_tx_against_state(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn deduct_caller(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn last_frame_result(&self, evm: &mut Self::Evm, frame_result: &mut <Self::Frame as Frame>::FrameResult) -> Result<(), Self::Error>;
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <Self::Frame as Frame>::FrameResult) -> Result<(), Self::Error>;
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <Self::Frame as Frame>::FrameResult, eip7702_refund: i64);
            fn output(&self, evm: &mut Self::Evm, result: <Self::Frame as Frame>::FrameResult) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn catch_error(&self, evm: &mut Self::Evm, error: Self::Error) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
        }
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        evm.ctx().log_data_size = 0;
        self.op.pre_execution(evm)
    }

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        self.op.validate_env(evm)?;
        let ctx = evm.ctx_ref();

        if self.spec.is_enabled_in(SpecId::MINI_REX) && ctx.tx().kind().is_create() {
            // additionally, ensure initcode size does not exceed `contract size limit` + 24KB
            let max_initcode_size =
                ctx.cfg().max_code_size() + constants::mini_rex::ADDITIONAL_INITCODE_SIZE;
            if ctx.tx().input().len() > max_initcode_size {
                return Err(InvalidTransaction::CreateInitCodeSizeLimit.into());
            }
        }

        Ok(())
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <Self::Frame as Frame>::FrameResult,
    ) -> Result<(), Self::Error> {
        if self.disable_beneficiary {
            Ok(())
        } else {
            self.op.reward_beneficiary(evm, exec_result)
        }
    }
}

impl<DB: Database, EVM, ERROR, FRAME> InspectorHandler for Handler<EVM, ERROR, FRAME>
where
    EVM: InspectorEvmTr<
        Context = Context<DB>,
        Inspector: Inspector<
            <<Self as revm::handler::Handler>::Evm as EvmTr>::Context,
            EthInterpreter,
        >,
    >,
    ERROR: EvmTrError<EVM> + From<TransactionError> + FromStringError + IsTxError,
    FRAME: InspectorFrame<
        Evm = EVM,
        Error = ERROR,
        FrameResult = FrameResult,
        FrameInit = FrameInput,
        IT = EthInterpreter,
    >,
{
    type IT = EthInterpreter;

    delegate! {
        to self.op {
            fn inspect_run(&mut self, evm: &mut Self::Evm) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn inspect_run_without_catch_error(&mut self, evm: &mut Self::Evm) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn inspect_execution(&mut self, evm: &mut Self::Evm, init_and_floor_gas: &InitialAndFloorGas) -> Result<FrameResult, Self::Error>;
            fn inspect_first_frame_init(&mut self, evm: &mut Self::Evm, frame_input: <Self::Frame as Frame>::FrameInit) -> Result<FrameOrResult<Self::Frame>, Self::Error>;
            fn inspect_frame_call(&mut self, frame: &mut Self::Frame, evm: &mut Self::Evm) -> Result<FrameInitOrResult<Self::Frame>, Self::Error>;
            fn inspect_run_exec_loop(&mut self, evm: &mut Self::Evm, frame: Self::Frame) -> Result<FrameResult, Self::Error>;
        }
    }
}
