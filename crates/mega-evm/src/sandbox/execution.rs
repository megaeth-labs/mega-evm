//! Keyless deploy sandbox execution.
//!
//! This module executes keyless deployment in an isolated sandbox environment
//! to implement Nick's Method for deterministic contract deployment.
//!
//! # Spam Protection
//!
//! This module guarantees that once sandbox execution starts and completes, the signer
//! will always be charged for gas consumed. This is achieved through:
//!
//! - **Top-level restriction**: Only intercepted at `depth == 0` (see `evm/execution.rs`)
//! - **Execution errors return success**: `ExecutionReverted`, `ExecutionHalted`, and
//!   `EmptyCodeDeployed` return `InstructionResult::Return` with error data encoded in the return
//!   value, not `InstructionResult::Revert`
//! - **Atomic state application**: `apply_sandbox_state` always succeeds after sandbox execution
//!   completes
//!
//! This design ensures there is no way for an attacker to trigger sandbox execution
//! and then have the gas charges reverted.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{string::ToString, vec::Vec};

use alloy_consensus::Transaction as AlloyTransaction;
use alloy_evm::Evm;
use alloy_primitives::{Address, Bytes, Log, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},
        ContextTr, TxEnv,
    },
    context_interface::Transaction,
    handler::FrameResult,
    interpreter::{CallOutcome, Gas, Host, InstructionResult, InterpreterResult},
    primitives::KECCAK_EMPTY,
    state::EvmState,
    Database,
};

use crate::{
    constants, merge_evm_state_optional_status, ExternalEnvTypes, MegaContext, MegaEvm,
    MegaTransaction,
};

use super::tx::{calculate_keyless_deploy_address, decode_keyless_tx, recover_signer};

use super::{
    error::{encode_error_result, KeylessDeployError},
    state::SandboxDb,
};

/// Executes a keyless deploy call and returns the frame result.
///
/// Implements Nick's Method contract deployment:
/// 1. Validates the call (no ether transfer)
/// 2. Decodes the pre-EIP-155 transaction from calldata
/// 3. Validates gas limit override against transaction gas limit
/// 4. Recovers the signer and calculates the deploy address
/// 5. Executes contract creation in a sandbox environment
/// 6. Applies only allowed state changes (deployAddress + deploySigner balance)
///
/// # Spam Protection Guarantee
///
/// This function is designed so that once sandbox execution starts and completes
/// (producing either `SandboxOutcome::Success` or `SandboxOutcome::Failure`), the
/// outer transaction **cannot revert**. Execution errors return success with error
/// data encoded in the return value, ensuring the signer is always charged for gas.
///
/// This guarantee depends on this function only being called at `depth == 0`
/// (enforced in `evm/execution.rs`), preventing contracts from wrapping and reverting.
pub fn execute_keyless_deploy_call<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    call_inputs: &revm::interpreter::CallInputs,
    tx_bytes: &Bytes,
    gas_limit_override: U256,
) -> FrameResult {
    // Gas tracking for this call
    let mut gas = Gas::new(call_inputs.gas_limit);
    let return_memory_offset = call_inputs.return_memory_offset.clone();

    // Macros to construct frame results, avoiding closure borrow issues
    macro_rules! make_error {
        ($error:expr) => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(InstructionResult::Revert, encode_error_result($error), gas),
                return_memory_offset,
            ))
        };
    }

    macro_rules! make_halt {
        () => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::OutOfGas,
                    Bytes::new(),
                    Gas::new_spent(gas.limit()),
                ),
                return_memory_offset,
            ))
        };
    }

    macro_rules! make_success {
        ($gas_used:expr, $deployed_address:expr) => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return,
                    IKeylessDeploy::keylessDeployCall::abi_encode_returns(
                        &IKeylessDeploy::keylessDeployReturn {
                            gasUsed: $gas_used,
                            deployedAddress: $deployed_address,
                            errorData: Bytes::new(),
                        },
                    )
                    .into(),
                    gas,
                ),
                return_memory_offset,
            ))
        };
    }

    // Macro for execution failures (ExecutionReverted, ExecutionHalted, EmptyCodeDeployed).
    // These return success (not revert) so state changes persist and the signer is charged.
    macro_rules! make_execution_failure {
        ($gas_used:expr, $error:expr) => {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Return, // Success, not Revert
                    IKeylessDeploy::keylessDeployCall::abi_encode_returns(
                        &IKeylessDeploy::keylessDeployReturn {
                            gasUsed: $gas_used,
                            deployedAddress: Address::ZERO,
                            errorData: encode_error_result($error).to_vec().into(),
                        },
                    )
                    .into(),
                    gas,
                ),
                return_memory_offset,
            ))
        };
    }

    // Step 1: Charge overhead gas
    let cost = constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS;
    let has_sufficient_gas = gas.record_cost(cost);
    if !has_sufficient_gas {
        return make_halt!();
    }

    // Step 2: Check no ether transfer
    if !call_inputs.value.get().is_zero() {
        return make_error!(KeylessDeployError::NoEtherTransfer);
    }

    // Step 3: Decode the keyless transaction
    let keyless_tx = match decode_keyless_tx(tx_bytes) {
        Ok(tx) => tx,
        Err(e) => return make_error!(e),
    };

    // Step 4: Validate gas limit override
    let tx_gas_limit = keyless_tx.gas_limit();
    let gas_limit_override_u64: u64 = gas_limit_override.try_into().unwrap_or(u64::MAX);
    if gas_limit_override_u64 < tx_gas_limit {
        return make_error!(KeylessDeployError::GasLimitTooLow {
            tx_gas_limit,
            provided_gas_limit: gas_limit_override_u64,
        });
    }

    // Step 5: Recover signer and calculate deploy address
    let deploy_signer = match recover_signer(&keyless_tx) {
        Ok(addr) => addr,
        Err(e) => return make_error!(e),
    };
    let deploy_address = calculate_keyless_deploy_address(deploy_signer);

    // Restrict keyless deploys to signers with nonce <= 1 in parent state.
    let signer_nonce = match get_account_nonce(ctx, deploy_signer) {
        Ok(nonce) => nonce,
        Err(e) => return make_error!(e),
    };
    if signer_nonce > 1 {
        return make_error!(KeylessDeployError::SignerNonceTooHigh { signer_nonce });
    }

    // Step 6: Build the sandbox transaction.
    // The gas limit is set to the gas limit override.
    // The nonce is set to 0.
    // The enveloped_tx is set to the original raw keyless deploy transaction bytes
    let sandbox_tx = {
        let tx = TxEnv {
            caller: deploy_signer,
            kind: TxKind::Create,
            data: keyless_tx.input().clone(),
            value: keyless_tx.value(),
            gas_limit: gas_limit_override_u64,
            gas_price: keyless_tx.effective_gas_price(None),
            nonce: 0,
            ..Default::default()
        };
        let mut mega_tx = MegaTransaction::new(tx);
        mega_tx.enveloped_tx = Some(tx_bytes.clone());
        mega_tx
    };

    // Step 7: Check deploy address doesn't already have code
    {
        let deploy_account = ctx
            .journal_mut()
            .database
            .basic(deploy_address)
            .map_err(|e| KeylessDeployError::InternalError(e.to_string()));
        match deploy_account {
            Ok(Some(info)) if info.code_hash != KECCAK_EMPTY => {
                return make_error!(KeylessDeployError::ContractAlreadyExists);
            }
            Err(e) => return make_error!(e),
            _ => {}
        }
    }

    // Step 8: Execute sandbox and apply state changes
    match execute_keyless_deploy_sandbox(ctx, sandbox_tx) {
        Ok(SandboxOutcome::Success { state, result }) => {
            if let Err(e) = apply_sandbox_state(ctx, state, deploy_signer) {
                return make_error!(e);
            }

            // Verify the deployed address matches the expected address
            if result.deploy_address != deploy_address {
                return make_error!(KeylessDeployError::AddressMismatch);
            }

            // Emit logs from sandbox in parent context
            for log in result.logs {
                ctx.log(log);
            }

            make_success!(result.gas_used, result.deploy_address)
        }
        Ok(SandboxOutcome::Failure { state, error }) => {
            if let Err(e) = apply_sandbox_state(ctx, state, deploy_signer) {
                return make_error!(e);
            }
            // Extract gas_used from the execution error
            let gas_used = match &error {
                KeylessDeployError::ExecutionReverted { gas_used, .. } => *gas_used,
                KeylessDeployError::ExecutionHalted { gas_used, .. } => *gas_used,
                KeylessDeployError::EmptyCodeDeployed { gas_used } => *gas_used,
                _ => 0, // Shouldn't happen for execution errors
            };
            // Return success (not revert) so state changes persist and signer is charged
            make_execution_failure!(gas_used, error)
        }
        Err(e) => make_error!(e),
    }
}

/// Result of sandbox execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxResult {
    gas_used: u64,
    deploy_address: Address,
    logs: Vec<Log>,
}

/// Executes the contract creation in a sandbox environment.
///
/// Uses a type-erased `SandboxDb` to prevent infinite type instantiation.
///
/// # Arguments
///
/// * `ctx` - The parent context to execute in
/// * `sandbox_tx` - The transaction to execute, with `enveloped_tx` set to the original raw keyless
///   deploy transaction bytes
pub fn execute_keyless_deploy_sandbox<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_tx: MegaTransaction,
) -> Result<SandboxOutcome, KeylessDeployError> {
    let deploy_signer = sandbox_tx.caller();
    let gas_limit = sandbox_tx.gas_limit();
    let gas_price = sandbox_tx.gas_price();
    let value = sandbox_tx.value();

    // Extract values we need BEFORE borrowing the journal
    let mega_spec = ctx.mega_spec();
    let block = ctx.block().clone();
    let chain = ctx.chain().clone();
    let journal = ctx.journal_mut();

    // Create type-erased sandbox database with split borrows:
    // - Immutable reference to journal state (for cached accounts)
    // - Mutable reference to underlying database (for cache misses)
    // Override the signer's nonce to 0 for keyless deploy (Nick's Method requires nonce=0)
    let mut sandbox_db = SandboxDb::new(&journal.inner.state, &mut journal.database)
        .with_nonce_override(deploy_signer);

    // Check signer balance
    let signer_account = sandbox_db
        .basic(deploy_signer)
        .map_err(|e| KeylessDeployError::InternalError(e.to_string()))?
        .unwrap_or_default();

    // Ensure signer has enough balance to cover gas cost and value
    let gas_cost = U256::from(gas_limit) * U256::from(gas_price);
    let total_cost = gas_cost.checked_add(value).ok_or(KeylessDeployError::InsufficientBalance)?;
    if signer_account.balance < total_cost {
        return Err(KeylessDeployError::InsufficientBalance);
    }

    // Execute sandbox - using type-erased SandboxDb prevents infinite type instantiation
    let sandbox_result: Result<SandboxOutcome, KeylessDeployError> = {
        // Create sandbox context with the type-erased database.
        // SandboxDb is a concrete type, so MegaContext<SandboxDb, ...> doesn't recurse.
        // Disable sandbox interception to prevent recursive sandbox creation.
        let sandbox_ctx = MegaContext::new(sandbox_db, mega_spec)
            .with_block(block)
            .with_chain(chain)
            .with_sandbox_disabled(true);
        let mut sandbox_evm = MegaEvm::new(sandbox_ctx);

        // Execute the transaction
        let result = sandbox_evm.transact_raw(sandbox_tx);

        // Process result and extract what we need
        match result {
            Ok(ResultAndState { result: exec_result, state: sandbox_state }) => match exec_result {
                ExecutionResult::Success { gas_used, output, logs, .. } => {
                    if let revm::context::result::Output::Create(bytecode, Some(created_addr)) =
                        output
                    {
                        // Empty bytecode is treated as failure to prevent replay attacks.
                        // Without this check, a keyless deploy tx that returns empty code could
                        // be submitted multiple times, draining the signer's funds.
                        if bytecode.is_empty() {
                            return Ok(SandboxOutcome::Failure {
                                state: sandbox_state,
                                error: KeylessDeployError::EmptyCodeDeployed { gas_used },
                            });
                        }
                        Ok(SandboxOutcome::Success {
                            state: sandbox_state,
                            result: SandboxResult { deploy_address: created_addr, gas_used, logs },
                        })
                    } else {
                        // Contract creation didn't return an address - should never happen
                        // but we return an error instead of panicking to avoid crashing the node
                        Err(KeylessDeployError::NoContractCreated)
                    }
                }
                ExecutionResult::Revert { gas_used, output } => Ok(SandboxOutcome::Failure {
                    state: sandbox_state,
                    error: KeylessDeployError::ExecutionReverted { gas_used, output },
                }),
                ExecutionResult::Halt { gas_used, reason } => Ok(SandboxOutcome::Failure {
                    state: sandbox_state,
                    error: KeylessDeployError::ExecutionHalted { gas_used, reason },
                }),
            },
            Err(e) => Err(KeylessDeployError::InternalError(e.to_string())),
        }
    };

    sandbox_result
}

/// Outcome of sandbox execution, including state for merging on failure.
#[derive(Debug)]
pub enum SandboxOutcome {
    /// Successful execution with the resulting state and return data.
    Success {
        /// Sandbox state to merge into the parent context.
        state: EvmState,
        /// Execution result details.
        result: SandboxResult,
    },
    /// Failed execution with the resulting state and error.
    Failure {
        /// Sandbox state to merge into the parent context.
        state: EvmState,
        /// Error returned by sandbox execution.
        error: KeylessDeployError,
    },
}

/// Applies all state changes from sandbox execution to the parent journal.
///
/// Note that we need to merge all accounts into the parent journal, even if they are not
/// touched or created. This is because we need to know which accounts are read but
/// not written to obtain `ReadSet` to facilitate stateless witness generation.
///
/// We do not merge any account status into the parent journal and the coldness of accounts and
/// storage slots are preserved. This is because the changes in the sandbox are treated as a silent
/// change in the database and should not affect the behavior of the current transaction (e.g., gas
/// cost due to coldness) execept that the state itself are different.
///
/// The sandbox execution uses nonce 0 for the signer (Nick's Method), and after the CREATE
/// transaction the nonce becomes 1. This nonce change is intentionally preserved in the parent
/// state to reflect that the signer has been used for a keyless deploy.
fn apply_sandbox_state<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_state: EvmState,
    _deploy_signer: Address,
) -> Result<(), KeylessDeployError> {
    let journal = ctx.journal_mut();

    // Merge the sandbox state into the parent journal
    merge_evm_state_optional_status(&mut journal.state, &sandbox_state, false);

    Ok(())
}

fn get_account_nonce<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    address: Address,
) -> Result<u64, KeylessDeployError> {
    let journal = ctx.journal_mut();
    if let Some(acc) = journal.state.get(&address) {
        return Ok(acc.info.nonce);
    }
    Ok(journal
        .database
        .basic(address)
        .map_err(|e| KeylessDeployError::InternalError(e.to_string()))?
        .map(|info| info.nonce)
        .unwrap_or(0))
}
