//! Keyless deploy sandbox execution.
//!
//! This module executes keyless deployment in an isolated sandbox environment
//! to implement Nick's Method for deterministic contract deployment.

use alloy_consensus::Transaction;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_system_contracts::keyless_deploy::IKeylessDeploy;
use revm::{
    context::{ContextTr, TxEnv},
    handler::FrameResult,
    interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult},
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
pub(crate) fn execute_keyless_deploy_call<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
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

    // Step 6: Execute sandbox and apply state changes
    match execute_keyless_deploy_sandbox(
        ctx,
        deploy_signer,
        deploy_address,
        keyless_tx.input().clone(),
        keyless_tx.value(),
        keyless_tx.effective_gas_price(None),
        gas_limit_override_u64,
    ) {
        Ok(sandbox_result) => {
            assert_eq!(sandbox_result.deploy_address, deploy_address, "Deployed address mismatch");
            make_success!(sandbox_result.gas_used, sandbox_result.deploy_address)
        }
        Err(e) => make_error!(e),
    }
}

/// Result of sandbox execution.
struct SandboxResult {
    gas_used: u64,
    deploy_address: Address,
}

/// Executes the contract creation in a sandbox environment.
///
/// Uses a type-erased `SandboxDb` to prevent infinite type instantiation.
fn execute_keyless_deploy_sandbox<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
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

    // Extract values we need BEFORE borrowing the journal
    let mega_spec = ctx.mega_spec();
    let block = ctx.block().clone();
    let journal = ctx.journal_mut();

    // Create type-erased sandbox database with split borrows:
    // - Immutable reference to journal state (for cached accounts)
    // - Mutable reference to underlying database (for cache misses)
    let mut sandbox_db = SandboxDb::new(&journal.inner.state, &mut journal.database);

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

    // Check deploy address doesn't have code
    let deploy_account = sandbox_db
        .basic(deploy_address)
        .map_err(|e| KeylessDeployError::InternalError(e.to_string()))?
        .unwrap_or_default();
    if deploy_account.code_hash != KECCAK_EMPTY {
        return Err(KeylessDeployError::ContractAlreadyExists);
    }

    // Execute sandbox - using type-erased SandboxDb prevents infinite type instantiation
    let sandbox_result: Result<(EvmState, u64), KeylessDeployError> = {
        // Create sandbox context with the type-erased database.
        // SandboxDb is a concrete type, so MegaContext<SandboxDb, ...> doesn't recurse.
        // Disable sandbox interception to prevent recursive sandbox creation.
        let sandbox_ctx =
            MegaContext::new(sandbox_db, mega_spec).with_block(block).with_sandbox_disabled(true);
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
            Ok(ResultAndState { result: exec_result, state: sandbox_state }) => match exec_result {
                ExecutionResult::Success { gas_used, output, .. } => {
                    if let revm::context::result::Output::Create(_, Some(created_addr)) = output {
                        if created_addr != deploy_address {
                            // This should never happen - address mismatch indicates a bug
                            Err(KeylessDeployError::AddressMismatch)
                        } else {
                            Ok((sandbox_state, gas_used))
                        }
                    } else {
                        // Contract creation didn't return an address - should never happen
                        // but we return an error instead of panicking to avoid crashing the node
                        Err(KeylessDeployError::NoContractCreated)
                    }
                }
                ExecutionResult::Revert { gas_used, output } => {
                    Err(KeylessDeployError::ExecutionReverted { gas_used, output })
                }
                ExecutionResult::Halt { gas_used, reason } => {
                    Err(KeylessDeployError::ExecutionHalted { gas_used, reason })
                }
            },
            Err(e) => Err(KeylessDeployError::InternalError(e.to_string())),
        }
    };

    // Apply all state changes from sandbox to parent context
    let (sandbox_state, gas_used) = sandbox_result?;
    apply_sandbox_state(ctx, sandbox_state)?;

    Ok(SandboxResult { deploy_address, gas_used })
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
fn apply_sandbox_state<DB: alloy_evm::Database, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_state: EvmState,
) -> Result<(), KeylessDeployError> {
    merge_evm_state_optional_status(&mut ctx.journal_mut().state, &sandbox_state, false);
    Ok(())
}
