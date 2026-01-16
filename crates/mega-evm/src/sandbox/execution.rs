//! Keyless deploy sandbox execution.
//!
//! This module executes keyless deployment in an isolated sandbox environment
//! to implement Nick's Method for deterministic contract deployment.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use revm::{
    context::{ContextTr, JournalTr, TxEnv},
    handler::FrameResult,
    interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult},
    state::EvmState,
};

use crate::{
    calculate_keyless_deploy_address, constants, decode_keyless_tx, encode_error_result,
    encode_success_result, recover_signer, ExternalEnvTypes, KeylessDeployError, MegaContext,
    MegaEvm, MegaTransaction,
};

use super::state::SandboxDb;

/// Executes a keyless deploy call and returns the frame result.
///
/// Implements Nick's Method contract deployment:
/// 1. Validates the call (no ether transfer)
/// 2. Decodes the pre-EIP-155 transaction from calldata
/// 3. Recovers the signer and calculates the deploy address
/// 4. Executes contract creation in a sandbox environment
/// 5. Applies only allowed state changes (deployAddress + deploySigner balance)
pub(crate) fn execute_keyless_deploy_call<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    call_inputs: &revm::interpreter::CallInputs,
    tx_bytes: &Bytes,
) -> FrameResult {
    let sandbox_gas_limit = call_inputs.gas_limit;
    let return_memory_offset = call_inputs.return_memory_offset.clone();

    let make_error = |error: KeylessDeployError, gas_remaining: u64| -> FrameResult {
        let mut gas = Gas::new(sandbox_gas_limit);
        let _ = gas.record_cost(sandbox_gas_limit.saturating_sub(gas_remaining));
        FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Revert, encode_error_result(error), gas),
            return_memory_offset.clone(),
        ))
    };

    let make_success = |deployed_address: Address, gas_remaining: u64| -> FrameResult {
        let mut gas = Gas::new(sandbox_gas_limit);
        let _ = gas.record_cost(sandbox_gas_limit.saturating_sub(gas_remaining));
        FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                encode_success_result(deployed_address),
                gas,
            ),
            return_memory_offset.clone(),
        ))
    };

    // Step 1: Check no ether transfer
    if !call_inputs.value.get().is_zero() {
        return make_error(KeylessDeployError::NoEtherTransfer, sandbox_gas_limit);
    }

    // Step 2: Charge overhead gas
    let overhead_gas = constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS;
    if sandbox_gas_limit < overhead_gas {
        return make_error(
            KeylessDeployError::GasLimitLessThanIntrinsic {
                intrinsic_gas: overhead_gas,
                provided_gas: sandbox_gas_limit,
            },
            0,
        );
    }
    let remaining_gas = sandbox_gas_limit - overhead_gas;

    // Step 3: Decode the keyless transaction
    let keyless_tx = match decode_keyless_tx(tx_bytes) {
        Ok(tx) => tx,
        Err(e) => return make_error(e, remaining_gas),
    };

    // Step 4: Recover signer and calculate deploy address
    let deploy_signer = match recover_signer(&keyless_tx) {
        Ok(addr) => addr,
        Err(e) => return make_error(e, remaining_gas),
    };
    let deploy_address = calculate_keyless_deploy_address(deploy_signer);

    // Step 5: Execute sandbox and apply state changes
    match execute_keyless_sandbox(
        ctx,
        deploy_signer,
        deploy_address,
        keyless_tx.init_code.clone(),
        keyless_tx.value,
        keyless_tx.gas_price,
        remaining_gas,
    ) {
        Ok(sandbox_result) => {
            assert_eq!(sandbox_result.deploy_address, deploy_address, "Deployed address mismatch");
            make_success(deploy_address, sandbox_result.gas.remaining())
        }
        Err(e) => make_error(e, remaining_gas),
    }
}

/// Result of sandbox execution.
struct SandboxResult {
    deploy_address: Address,
    gas: Gas,
}

/// Executes the contract creation in a sandbox environment.
///
/// Uses a type-erased `SandboxDb` to prevent infinite type instantiation.
fn execute_keyless_sandbox<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    deploy_signer: Address,
    deploy_address: Address,
    init_code: Bytes,
    value: U256,
    gas_price: u128,
    gas_limit: u64,
) -> Result<SandboxResult, KeylessDeployError> {
    use alloy_evm::Evm;
    use revm::context::result::{ExecutionResult, ResultAndState};

    let gas_cost = U256::from(gas_limit) * U256::from(gas_price);

    // Check signer balance
    let signer_account = ctx
        .journal_mut()
        .load_account(deploy_signer)
        .map_err(|_| KeylessDeployError::DeploymentFailed)?;

    let total_cost = gas_cost.checked_add(value).ok_or(KeylessDeployError::DeploymentFailed)?;
    if signer_account.data.info.balance < total_cost {
        return Err(KeylessDeployError::DeploymentFailed);
    }

    // Check deploy address doesn't have code
    let deploy_account = ctx
        .journal_mut()
        .load_account(deploy_address)
        .map_err(|_| KeylessDeployError::DeploymentFailed)?;

    if deploy_account.data.info.code.as_ref().is_some_and(|c| !c.is_empty()) {
        return Err(KeylessDeployError::DeploymentFailed);
    }

    // Extract values we need BEFORE borrowing the journal
    let spec = ctx.mega_spec();
    let block = ctx.block().clone();

    // Set sandbox flag to prevent recursive keyless deploy interception
    ctx.set_in_keyless_deploy_sandbox(true);

    // Execute sandbox - using type-erased SandboxDb prevents infinite type instantiation
    let sandbox_result: Result<(EvmState, Gas), KeylessDeployError> = {
        // Create type-erased sandbox database from journal
        // This captures the current state and type-erases the database
        let sandbox_db = SandboxDb::from_journal(ctx.journal());

        // Create sandbox context with the type-erased database
        // SandboxDb is a concrete type, so MegaContext<SandboxDb, ...> doesn't recurse
        let sandbox_ctx = MegaContext::new(sandbox_db, spec).with_block(block);
        let mut sandbox_evm = MegaEvm::new(sandbox_ctx);

        // Build and execute CREATE transaction
        let tx = TxEnv {
            caller: deploy_signer,
            kind: TxKind::Create,
            data: init_code,
            value,
            gas_limit,
            gas_price,
            nonce: 0,
            ..Default::default()
        };
        let result = sandbox_evm.transact_raw(MegaTransaction::new(tx));

        // Process result and extract what we need
        match result {
            Ok(ResultAndState { result: exec_result, state: sandbox_state }) => {
                match exec_result {
                    ExecutionResult::Success { gas_used, gas_refunded, output, .. } => {
                        if let revm::context::result::Output::Create(_, Some(created_addr)) = output
                        {
                            if created_addr != deploy_address {
                                Err(KeylessDeployError::DeploymentFailed)
                            } else {
                                let mut gas = Gas::new(gas_limit);
                                let _ = gas.record_cost(gas_used);
                                gas.record_refund(gas_refunded as i64);
                                Ok((sandbox_state, gas))
                            }
                        } else {
                            Err(KeylessDeployError::DeploymentFailed)
                        }
                    }
                    ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => {
                        Err(KeylessDeployError::DeploymentFailed)
                    }
                }
            }
            Err(_) => Err(KeylessDeployError::DeploymentFailed),
        }
    };

    // Clear sandbox flag
    ctx.set_in_keyless_deploy_sandbox(false);

    // Now we can apply state changes to the parent context
    let (sandbox_state, gas) = sandbox_result?;
    apply_sandbox_state_changes(ctx, &sandbox_state, deploy_signer, deploy_address)?;

    Ok(SandboxResult { deploy_address, gas })
}

/// Applies filtered state changes from sandbox to parent journal.
///
/// Only updates:
/// - `deploy_address`: Full state (code, storage, balance, status)
/// - `deploy_signer`: Balance only (gas deduction + value transfer)
fn apply_sandbox_state_changes<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_state: &EvmState,
    deploy_signer: Address,
    deploy_address: Address,
) -> Result<(), KeylessDeployError> {
    for (address, account) in sandbox_state {
        if *address == deploy_address {
            let deploy_account = ctx
                .journal_mut()
                .load_account(*address)
                .map_err(|_| KeylessDeployError::DeploymentFailed)?;
            deploy_account.data.info = account.info.clone();
            deploy_account.data.storage = account.storage.clone();
            deploy_account.data.status = account.status;
        } else if *address == deploy_signer {
            let signer_account = ctx
                .journal_mut()
                .load_account(*address)
                .map_err(|_| KeylessDeployError::DeploymentFailed)?;
            signer_account.data.info.balance = account.info.balance;
            signer_account.data.mark_touch();
        }
    }
    Ok(())
}
