use alloy_evm::Database;
use alloy_primitives::TxKind;
use delegate::delegate;
use op_revm::{
    handler::{IsTxError, OpHandler},
    OpHaltReason, OpTransactionError,
};
use revm::{
    context::{
        result::{ExecutionResult, FromStringError, InvalidTransaction, OutOfGasError},
        ContextTr, Transaction,
    },
    handler::{EthFrame, EvmTr, EvmTrError, FrameResult, FrameTr},
    inspector::{InspectorEvmTr, InspectorHandler},
    interpreter::{
        gas::get_tokens_in_calldata, interpreter::EthInterpreter, interpreter_action::FrameInit,
        InitialAndFloorGas,
    },
    Inspector, Journal,
};

use crate::{
    constants, is_mega_system_transaction, sent_from_mega_system_address, EthHaltReason,
    ExternalEnvs, HostExt, MegaContext, MegaHaltReason, MegaSpecId, MegaTransactionError,
    DEPOSIT_TX_GAS_STIPEND_MULTIPLIER, DEPOSIT_TX_GAS_STIPEND_WHITELIST,
    MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
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

impl<DB: Database, EVM, ERROR, FRAME, ExtEnvs: ExternalEnvs> revm::handler::Handler
    for MegaHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context = MegaContext<DB, ExtEnvs>, Frame = FRAME>,
    ERROR: EvmTrError<EVM>
        + From<OpTransactionError>
        + From<MegaTransactionError>
        + FromStringError
        + IsTxError
        + std::fmt::Debug,
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

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        let ctx = evm.ctx_mut();
        ctx.on_new_tx();

        if ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            let tx = ctx.tx();
            if tx.tx_type() == DEPOSIT_TRANSACTION_TYPE {
                // If the deposit tx calls a whitelisted address, we apply gas stipend to the tx
                match tx.kind() {
                    TxKind::Create => {}
                    TxKind::Call(address) => {
                        if DEPOSIT_TX_GAS_STIPEND_WHITELIST.contains(&address) {
                            ctx.inner.tx.base.gas_limit *= DEPOSIT_TX_GAS_STIPEND_MULTIPLIER;
                        }
                    }
                }
            }

            // Check if this is a mega system address transaction
            let tx = ctx.tx();
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
                ctx.inner.tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
                // Set gas_price to 0 so the transaction doesn't pay L2 execution gas,
                // consistent with OP deposit transaction behavior where gas is pre-paid on L1.
                ctx.inner.tx.base.gas_price = 0;
            }
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

            // MegaETH MiniRex modification: 100x increase in calldata gas costs
            // - Standard tokens: 400 gas per token (vs 4)
            // - EIP-7623 floor: 100x increase for transaction data floor cost
            let tokens_in_calldata = get_tokens_in_calldata(ctx.tx().input(), true);
            let additional_calldata_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS * tokens_in_calldata;
            initial_and_floor_gas.initial_gas += additional_calldata_gas;
            let additional_floor_calldata_gas =
                constants::mini_rex::CALLDATA_STANDARD_TOKEN_ADDITIONAL_FLOOR_GAS *
                    tokens_in_calldata;
            initial_and_floor_gas.floor_gas += additional_floor_calldata_gas;

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
            // Check if the limit is exceeded before returning the frame result
            if evm
                .ctx()
                .additional_limit
                .borrow_mut()
                .before_frame_return_result(frame_result, true)
                .exceeded_limit()
            {
                // the frame result must have been marked as exceeding the limit, so return early
                return Ok(());
            }
        }

        // Call the inner last_frame_result function first
        // This will finalize gas accounting according to REVM's rules:
        // - Spends all gas_limit
        // - Only refunds remaining gas if is_ok_or_revert()
        self.op.last_frame_result(evm, frame_result)?;

        // After REVM's gas accounting, refund detained gas for volatile data access
        // This must happen AFTER the op handler to override its gas calculation
        if is_mini_rex {
            let mut volatile_data_tracker = evm.ctx().volatile_data_tracker.borrow_mut();
            let gas = frame_result.gas_mut();
            volatile_data_tracker.refund_detained_gas(gas);
        }

        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // Capture volatile data info BEFORE calling op.execution_result (which calls
        // last_frame_result) because last_frame_result will call refund_detained_gas which
        // resets detained to 0
        let volatile_info_before_refund = evm
            .ctx()
            .spec
            .is_enabled(MegaSpecId::MINI_REX)
            .then(|| {
                let volatile_data_tracker = evm.ctx().volatile_data_tracker.borrow();
                volatile_data_tracker.get_volatile_data_info()
            })
            .flatten();

        let result = self.op.execution_result(evm, result)?;
        Ok(result.map_haltreason(|reason| match reason {
            OpHaltReason::Base(EthHaltReason::OutOfGas(OutOfGasError::Basic)) => {
                // Check if this OutOfGas is due to volatile data access
                if let Some((access_type, limit, detained)) = volatile_info_before_refund {
                    // This OutOfGas happened after volatile data access - it's likely due to
                    // hitting the detention limit. Return our custom halt reason.
                    MegaHaltReason::VolatileDataAccessOutOfGas { access_type, limit, detained }
                } else {
                    // No volatile data accessed - check if data/kv limits exceeded
                    evm.ctx()
                        .additional_limit
                        .borrow_mut()
                        .check_limit()
                        .maybe_halt_reason()
                        .unwrap_or(MegaHaltReason::Base(reason))
                }
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
        + std::fmt::Debug,
{
    type IT = EthInterpreter;
}
