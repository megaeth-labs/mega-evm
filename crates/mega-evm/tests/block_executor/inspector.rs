//! Tests for inspector integration with `MegaBlockExecutor`.
//!
//! These tests verify that inspectors work correctly when executing transactions
//! using `MegaBlockExecutor`.

use std::{cell::Cell, convert::Infallible};

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, MegaTxEnvelope, TestExternalEnvs,
};
use revm::{
    bytecode::opcode::{ADD, CALL, GAS, PUSH0, SLOAD, SSTORE},
    context::{BlockEnv, ContextTr, JournalTr},
    database::State,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Gas, InstructionResult,
        InterpreterResult, InterpreterTypes,
    },
    Inspector,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");

/// Helper function to create a recovered call transaction.
fn create_transaction(
    nonce: u64,
    gas_limit: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453), // Base mainnet
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Helper function to create a contract creation transaction.
fn create_deploy_transaction(
    nonce: u64,
    gas_limit: u64,
    init_code: Bytes,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Creates a contract that performs SSTORE operations.
fn create_test_contract() -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0) // key (slot 0)
        .append(SLOAD) // load value from slot 0
        .push_number(1u8) // push 1 to increment
        .append(ADD) // add 1 to loaded value
        .append(PUSH0) // key for SSTORE
        .append(SSTORE) // store incremented value
        .stop()
        .build()
}

#[test]
fn test_inspector_works_with_block_executor() {
    // Create database and deploy contract
    let mut db = MemoryDatabase::default();
    let bytecode = create_test_contract();
    db.set_account_code(CONTRACT, bytecode);
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory and block executor factory with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create block context
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());

    // Create inspector
    let inspector = GasInspector::new();

    // Create block executor with inspector
    let mut executor = block_executor_factory
        .create_executor_with_inspector(&mut state, block_ctx, evm_env, inspector);

    // Execute transaction
    let tx = create_transaction(0, 1_000_000);
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "Transaction should succeed: {:?}", result.err());

    // Get inspector records via evm
    let records = executor.evm().inspector.records();

    // Verify that inspector recorded opcodes
    assert!(!records.is_empty(), "Inspector should have recorded opcodes");

    // Verify that we recorded the expected opcodes
    let opcodes: Vec<_> = records.iter().map(|r| r.opcode.as_str()).collect();
    assert!(opcodes.contains(&"SLOAD"), "Should record SLOAD opcode");
    assert!(opcodes.contains(&"ADD"), "Should record ADD opcode");
    assert!(opcodes.contains(&"SSTORE"), "Should record SSTORE opcode");

    // Verify that gas costs are being tracked
    for record in &records {
        assert!(
            record.gas_before >= record.gas_after,
            "Gas should decrease or stay the same after opcode execution"
        );
    }

    // Finish the block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt");
}

/// An inspector that returns early for nested calls, skipping frame execution.
///
/// This is used to test the fix for the additional limit frame stack maintenance bug.
/// When an inspector returns `Some(CallOutcome)` from its `call` hook, the `frame_init`
/// is skipped, but `frame_return_result` is still called. Without the fix, this would
/// cause a panic due to frame stack misalignment.
#[derive(Default)]
struct SkipNestedCallInspector {
    /// Number of nested calls that were intercepted and skipped.
    calls_intercepted: Cell<u32>,
    /// Number of `call_end` hooks that were invoked.
    call_ends: Cell<u32>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipNestedCallInspector {
    #[allow(clippy::if_then_some_else_none)] // if-else is clearer here
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let depth = context.journal().depth();
        if depth > 0 {
            // Return early for nested calls - this triggers the bug scenario where
            // inspector skips `frame_init` but `frame_return_result` is still called
            self.calls_intercepted.set(self.calls_intercepted.get() + 1);
            Some(CallOutcome {
                result: InterpreterResult {
                    result: InstructionResult::Stop,
                    output: Bytes::new(),
                    gas: Gas::new(inputs.gas_limit),
                },
                memory_offset: 0..0,
            })
        } else {
            None
        }
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends.set(self.call_ends.get() + 1);
    }
}

const CONTRACT_B: Address = address!("3000000000000000000000000000000000000003");

/// Creates a contract that makes a CALL to another contract.
fn create_caller_contract() -> Bytes {
    // Contract that calls CONTRACT_B with 0 value, empty calldata
    // PUSH1 0    - retSize
    // PUSH1 0    - retOffset
    // PUSH1 0    - argsSize
    // PUSH1 0    - argsOffset
    // PUSH1 0    - value
    // PUSH20 addr - address
    // GAS        - gas
    // CALL
    // STOP
    BytecodeBuilder::default()
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argsSize
        .push_number(0u8) // argsOffset
        .push_number(0u8) // value
        .push_address(CONTRACT_B) // address
        .append(GAS) // gas (use all available)
        .append(CALL)
        .stop()
        .build()
}

/// Creates a simple contract that just stops.
fn create_target_contract() -> Bytes {
    BytecodeBuilder::default().stop().build()
}

/// Test that inspector early return works correctly with additional limit tracking.
///
/// This test validates the fix for the bug where inspector's call hook returning early
/// would cause frame stack misalignment in the additional limit trackers.
///
/// Before the fix, this test would panic with "frame stack is empty" because:
/// 1. Inspector's call hook returns `Some(CallOutcome)` for the nested call
/// 2. `frame_init` is skipped (no frame pushed to additional limit trackers)
/// 3. `frame_return_result` is still called (tries to pop a frame that doesn't exist)
///
/// After the fix, we push a dummy frame when inspector returns early, so the pop works.
#[test]
fn test_inspector_early_return_with_additional_limits() {
    // Create database and deploy contracts
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, create_caller_contract());
    db.set_account_code(CONTRACT_B, create_target_contract());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory and block executor factory with MiniRex hardfork activated
    // MiniRex enables the additional limit tracking that was affected by the bug
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create block context
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());

    // Create inspector that skips nested calls
    let inspector = SkipNestedCallInspector::default();

    // Create block executor with inspector
    let mut executor = block_executor_factory
        .create_executor_with_inspector(&mut state, block_ctx, evm_env, inspector);

    // Execute transaction - this triggers a nested CALL that the inspector intercepts
    let tx = create_transaction(0, 1_000_000);

    // Before the fix, this would panic with "frame stack is empty"
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "Transaction should succeed: {:?}", result.err());

    // Verify the inspector intercepted the nested call
    assert_eq!(
        executor.evm().inspector.calls_intercepted.get(),
        1,
        "Inspector should have intercepted 1 nested call"
    );

    // Verify call_end was still invoked (inspector hooks work correctly)
    assert_eq!(
        executor.evm().inspector.call_ends.get(),
        2,
        "call_end should be invoked for both the main call and the intercepted nested call"
    );

    // Finish the block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt");
}

/// An inspector that returns early for create operations, skipping frame execution.
///
/// Similar to `SkipNestedCallInspector` but for CREATE/CREATE2 operations.
#[derive(Default)]
struct SkipCreateInspector {
    /// Number of create operations that were intercepted and skipped.
    creates_intercepted: Cell<u32>,
    /// Number of `create_end` hooks that were invoked.
    create_ends: Cell<u32>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipCreateInspector {
    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        // Always intercept create operations - this triggers the bug scenario
        self.creates_intercepted.set(self.creates_intercepted.get() + 1);
        Some(CreateOutcome {
            result: InterpreterResult {
                result: InstructionResult::Stop,
                output: Bytes::new(),
                gas: Gas::new(inputs.gas_limit),
            },
            address: None,
        })
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.create_ends.set(self.create_ends.get() + 1);
    }
}

/// Test that inspector early return works correctly for CREATE operations.
///
/// This test validates the fix for CREATE operations using a contract creation transaction.
#[test]
fn test_inspector_early_return_create_with_additional_limits() {
    // Create database with funded caller
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory and block executor factory with MiniRex hardfork activated
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));
    let receipt_builder = OpAlloyReceiptBuilder::default();
    let block_executor_factory =
        MegaBlockExecutorFactory::new(chain_spec, evm_factory, receipt_builder);

    // Create EVM environment
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create block context
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, None, Bytes::new(), BlockLimits::no_limits());

    // Create inspector that skips create operations
    let inspector = SkipCreateInspector::default();

    // Create block executor with inspector
    let mut executor = block_executor_factory
        .create_executor_with_inspector(&mut state, block_ctx, evm_env, inspector);

    // Execute contract creation transaction - this triggers the CREATE that the inspector intercepts
    // Init code is just STOP (0x00)
    // Use higher gas limit to cover MiniRex initial gas costs for CREATE transactions
    let init_code = Bytes::from(vec![0x00]);
    let tx = create_deploy_transaction(0, 10_000_000, init_code);

    // Before the fix, this would panic with "frame stack is empty"
    let result = executor.execute_transaction(&tx);
    assert!(result.is_ok(), "Transaction should succeed: {:?}", result.err());

    // Verify the inspector intercepted the create operation
    assert_eq!(
        executor.evm().inspector.creates_intercepted.get(),
        1,
        "Inspector should have intercepted 1 create operation"
    );

    // Verify create_end was still invoked (inspector hooks work correctly)
    assert_eq!(
        executor.evm().inspector.create_ends.get(),
        1,
        "create_end should be invoked for the intercepted create"
    );

    // Finish the block
    let block_result = executor.finish();
    assert!(block_result.is_ok(), "Block should finish successfully");

    let (_, receipts) = block_result.unwrap();
    assert_eq!(receipts.receipts.len(), 1, "Should have 1 receipt");
}
