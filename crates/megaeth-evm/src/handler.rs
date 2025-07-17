use delegate::delegate;
use op_revm::{
    api::exec::OpContextTr,
    handler::{IsTxError, OpHandler},
};
use revm::{
    context::result::{FromStringError, ResultAndState},
    handler::{EvmTr, EvmTrError, Frame, FrameInitOrResult, FrameOrResult, FrameResult, Handler},
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, FrameInput, InitialAndFloorGas},
    Inspector,
};

use crate::{MegaethHaltReason, MegaethTransactionError};

/// Revm handler for `MegaETH`. It internally wraps the [`op_revm::handler::OpHandler`] and inherits most functionalities from Optimism.
#[allow(missing_debug_implementations)]
pub struct MegaethHandler<EVM, ERROR, FRAME> {
    op: OpHandler<EVM, ERROR, FRAME>,
}

impl<EVM, ERROR, FRAME> MegaethHandler<EVM, ERROR, FRAME> {
    /// Create a new `MegaethHandler`.
    pub fn new() -> Self {
        Self {
            op: OpHandler::new(),
        }
    }
}

impl<EVM, ERROR, FRAME> Default for MegaethHandler<EVM, ERROR, FRAME> {
    fn default() -> Self {
        Self::new()
    }
}

impl<EVM, ERROR, FRAME> Handler for MegaethHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: OpContextTr>,
    ERROR: EvmTrError<EVM> + From<MegaethTransactionError> + FromStringError + IsTxError,
    FRAME: Frame<Evm = EVM, Error = ERROR, FrameResult = FrameResult, FrameInit = FrameInput>,
{
    type Evm = EVM;

    type Error = ERROR;

    type Frame = FRAME;

    type HaltReason = MegaethHaltReason;

    delegate! {
        to self.op {
            fn run(&mut self, evm: &mut Self::Evm) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn run_without_catch_error(&mut self, evm: &mut Self::Evm) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error>;
            fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error>;
            fn execution(&mut self, evm: &mut Self::Evm, init_and_floor_gas: &InitialAndFloorGas) -> Result<FrameResult, Self::Error>;
            fn post_execution(&self, evm: &mut Self::Evm, exec_result: FrameResult, init_and_floor_gas: InitialAndFloorGas, eip7702_gas_refund: i64) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn validate_initial_tx_gas(&self, evm: &Self::Evm) -> Result<InitialAndFloorGas, Self::Error>;
            fn validate_tx_against_state(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn load_accounts(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn apply_eip7702_auth_list(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error>;
            fn deduct_caller(&self, evm: &mut Self::Evm) -> Result<(), Self::Error>;
            fn first_frame_input(&mut self, evm: &mut Self::Evm, gas_limit: u64) -> Result<FrameInput, Self::Error>;
            fn last_frame_result(&self, evm: &mut Self::Evm, frame_result: &mut <Self::Frame as Frame>::FrameResult) -> Result<(), Self::Error>;
            fn first_frame_init(&mut self, evm: &mut Self::Evm, frame_input: <Self::Frame as Frame>::FrameInit) -> Result<FrameOrResult<Self::Frame>, Self::Error>;
            fn frame_init(&mut self, frame: &Self::Frame, evm: &mut Self::Evm, frame_input: <Self::Frame as Frame>::FrameInit) -> Result<FrameOrResult<Self::Frame>, Self::Error>;
            fn frame_call(&mut self, frame: &mut Self::Frame, evm: &mut Self::Evm) -> Result<FrameInitOrResult<Self::Frame>, Self::Error>;
            fn frame_return_result(&mut self, frame: &mut Self::Frame, evm: &mut Self::Evm, result: <Self::Frame as Frame>::FrameResult) -> Result<(), Self::Error>;
            fn run_exec_loop(&mut self, evm: &mut Self::Evm, frame: Self::Frame) -> Result<FrameResult, Self::Error>;
            fn eip7623_check_gas_floor(&self, evm: &mut Self::Evm, exec_result: &mut <Self::Frame as Frame>::FrameResult, init_and_floor_gas: InitialAndFloorGas);
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <Self::Frame as Frame>::FrameResult, eip7702_refund: i64);
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <Self::Frame as Frame>::FrameResult) -> Result<(), Self::Error>;
            fn reward_beneficiary(&self, evm: &mut Self::Evm, exec_result: &mut <Self::Frame as Frame>::FrameResult) -> Result<(), Self::Error>;
            fn output(&self, evm: &mut Self::Evm, result: <Self::Frame as Frame>::FrameResult) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
            fn catch_error(&self, evm: &mut Self::Evm, error: Self::Error) -> Result<ResultAndState<Self::HaltReason>, Self::Error>;
        }
    }
}

impl<EVM, ERROR, FRAME> InspectorHandler for MegaethHandler<EVM, ERROR, FRAME>
where
    EVM: InspectorEvmTr<
        Context: OpContextTr,
        Inspector: Inspector<<<Self as Handler>::Evm as EvmTr>::Context, EthInterpreter>,
    >,
    ERROR: EvmTrError<EVM> + From<MegaethTransactionError> + FromStringError + IsTxError,
    // TODO `FrameResult` should be a generic trait.
    // TODO `FrameInit` should be a generic.
    FRAME: InspectorFrame<
        Evm = EVM,
        Error = ERROR,
        FrameResult = FrameResult,
        FrameInit = FrameInput,
        IT = EthInterpreter,
    >,
{
    type IT = EthInterpreter;
}
