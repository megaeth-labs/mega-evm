use alloy_evm::Database;
use alloy_primitives::TxKind;
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
        Cfg, ContextSetters, ContextTr, Transaction,
    },
    handler::{validation, EthFrame, EvmTr, EvmTrError, FrameInitOrResult, FrameResult, FrameTr},
    inspector::{InspectorEvmTr, InspectorFrame, InspectorHandler, JournalExt},
    interpreter::{
        gas::get_tokens_in_calldata, interpreter::EthInterpreter, interpreter_action::FrameInit,
        FrameInput, InitialAndFloorGas,
    },
    Inspector, Journal,
};

use crate::{
    constants, is_mega_system_address_transaction, EthHaltReason, ExternalEnvOracle, HostExt,
    MegaContext, MegaHaltReason, MegaSpecId, MegaTransactionError,
};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;

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
            fn reimburse_caller(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult) -> Result<(), Self::Error>;
            fn refund(&self, evm: &mut Self::Evm, exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult, eip7702_refund: i64);
        }
    }

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        evm.ctx().on_new_tx();

        // Check if this is a mega system address transaction
        if is_mega_system_address_transaction(evm.ctx().tx()) {
            // Modify the transaction to make it appear as a deposit transaction
            // This will cause the OpHandler to automatically bypass signature validation,
            // nonce verification, and fee deduction during validation

            // Set the deposit source hash of the transaction to mark it as a deposit transaction
            // for `OpHandler`.
            // The implementation of `revm::context_interface::Transaction` trait for
            // `MegaTransaction` determines the tx type by the existence of the source
            // hash.
            evm.ctx_mut().inner.tx.deposit.source_hash =
                constants::MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
        }

        self.op.pre_execution(evm)
    }

    /// This function copies the logic from `revm::handler::Handler::validate` to and
    /// add additional gas cost for calldata.
    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        self.validate_env(evm)?;
        let mut initial_and_floor_gas = self.validate_initial_tx_gas(evm)?;

        let ctx = evm.ctx_mut();
        if ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            // MegaETH modification: additional gas cost for creating account
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
                    ctx.new_account_gas(callee_address).map_err(|_| {
                        let err_str = format!(
                            "Failed to get new account gas for callee address: {callee_address}",
                        );
                        Self::Error::from_string(err_str)
                    })?;
            }

            // MegaETH modification: additional gas cost for calldata (Istanbul hardfork must be
            // enabled)
            let tokens_in_calldata = get_tokens_in_calldata(ctx.tx().input(), true);
            let additional_calldata_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS * tokens_in_calldata;
            initial_and_floor_gas.initial_gas += additional_calldata_gas;

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
