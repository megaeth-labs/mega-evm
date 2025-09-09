//! # `MegaETH` EVM Additional Limits
//!
//! This module provides additional hard limit enforcement for the `MegaETH` EVM beyond the standard
//! EVM gas limit. It implements two types of hard limits:
//!
//! ## Key Features
//!
//! - **Data Size Limits**: Tracks and enforces limits on the total data size generated during
//!   transaction execution, including transaction data, logs, storage operations, and account
//!   updates. These data will eventually become the data of the block payload, so it is necessary
//!   to limit the data size.
//! - **KV Update Limits**: Tracks and enforces limits on the number of key-value write operations
//!   performed during transaction execution. Too many key-value write operations will cause the the
//!   block slow to be written in database, so it is necessary to limit the number.
//!
//! ## Components
//!
//! - [`AdditionalLimit`]: Main struct that coordinates both data size and KV update limits, which
//!   contains two sub-components:
//!     - [`DataSizeTracker`]: Tracks the total data size generated from transaction execution
//!     - [`KVUpdateCounter`]: Counts the number of key-value storage operations
//!
//! ## Usage
//!
//! The limits are automatically enforced during EVM execution through hooks that are called
//! at various points in the execution lifecycle. When limits are exceeded, the transaction
//! should halt with [`AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT`] instruction result, and
//! the EVM should halt in a similar way as OOG, consuming all remaining gas.

use core::ops::Range;

use alloy_primitives::{Address, Bytes, U256};
use revm::{
    context::Transaction,
    handler::{evm::FrameInitResult, EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, CallOutcome, CreateOutcome, FrameInput, Gas,
        InstructionResult, InterpreterResult,
    },
};

use crate::{constants, GasCostOracle};

/// Additional limits for the `MegaETH` EVM beyond standard EVM limits.
///
/// This struct coordinates both data size and key-value update limits, providing
/// a unified interface for limit enforcement during transaction execution.
///
/// # Fields
///
/// - `data_limit`: Maximum total data size allowed during transaction execution
/// - `kv_update_limit`: Maximum number of key-value storage operations allowed
/// - `data_size_tracker`: Tracks the current data size usage
/// - `kv_update_counter`: Counts the current number of KV operations
///
/// # Default Values
///
/// By default, uses the `MINI_REX` specification limits:
/// - Data limit: [`constants::mini_rex::TX_DATA_LIMIT`]
/// - KV update limit: [`constants::mini_rex::TX_KV_UPDATE_LIMIT`]
#[derive(Debug)]
pub struct AdditionalLimit {
    /// The data limit for the EVM. When the data limit is reached, the transaction will error and
    /// halt (consuming all remaining gas).
    ///
    /// This limit controls the maximum total data size that can be generated during
    /// transaction execution, including transaction data, logs, storage operations, etc.
    pub data_limit: u64,

    /// The key-value update limit for the EVM. When the key-value update limit is reached, the
    /// transaction will error and halt (consuming all remaining gas).
    ///
    /// This limit controls the maximum number of key-value storage operations that can be
    /// performed during transaction execution, including storage reads, writes, and account
    /// updates.
    pub kv_update_limit: u64,

    /// A tracker for the total data size (in bytes) generated from a transaction execution.
    ///
    /// This tracker monitors all data generated during execution, including transaction data,
    /// logs, storage operations, and account updates.
    pub data_size_tracker: DataSizeTracker,

    /// A counter for the number of key-value updates performed during transaction execution.
    ///
    /// This counter tracks storage operations and account updates, providing frame-aware
    /// counting that properly handles nested calls and reverts.
    pub kv_update_counter: KVUpdateCounter,
}

impl Default for AdditionalLimit {
    fn default() -> Self {
        Self {
            data_limit: constants::mini_rex::TX_DATA_LIMIT,
            kv_update_limit: constants::mini_rex::TX_KV_UPDATE_LIMIT,
            data_size_tracker: DataSizeTracker::new(),
            kv_update_counter: KVUpdateCounter::new(),
        }
    }
}

impl AdditionalLimit {
    /// The [`InstructionResult`] to indicate that the limit is exceeded.
    ///
    /// This constant is used to signal that either the data limit or KV update limit
    /// has been exceeded during transaction execution.
    pub const EXCEEDING_LIMIT_INSTRUCTION_RESULT: InstructionResult = InstructionResult::OutOfGas;

    /// Resets the internal state for a new transaction or block.
    ///
    /// This method clears both the data size tracker and KV update counter,
    /// preparing the limit system for a new execution context.
    pub fn reset(&mut self) {
        self.data_size_tracker.reset();
        self.kv_update_counter.reset();
    }

    /// Checks if any of the configured limits have been exceeded.
    ///
    /// This method examines both the data size and KV update limits to determine
    /// if the current usage exceeds the configured thresholds.
    ///
    /// # Returns
    ///
    /// Returns an [`AdditionalLimitResult`] indicating whether limits have been exceeded
    /// and which specific limit was exceeded if any.
    #[inline]
    pub fn check_limit(&self) -> AdditionalLimitResult {
        if self.data_size_tracker.exceeds_limit() {
            AdditionalLimitResult::ExceedsDataLimit {
                limit: self.data_limit,
                used: self.data_size_tracker.current_size(),
            }
        } else if self.kv_update_counter.exceeds_limit() {
            AdditionalLimitResult::ExceedsKVUpdateLimit {
                limit: self.kv_update_limit,
                used: self.kv_update_counter.current_count(),
            }
        } else {
            AdditionalLimitResult::WithinLimit
        }
    }

    /// Determines if a frame result indicates that limits have been exceeded.
    ///
    /// This method checks both the instruction result and the current limit status
    /// to determine if the frame failed due to limit enforcement.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame result to check
    ///
    /// # Returns
    ///
    /// Returns `true` if the result indicates limit exceeded, `false` otherwise.
    pub fn is_exceeding_limit_result(&self, result: &FrameResult) -> bool {
        result.instruction_result() == Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT &&
            self.check_limit().exceeded_limit()
    }
}

/// Hooks for transaction execution lifecycle.
///
/// These methods are called at various points during EVM execution to track
/// operations and enforce limits. They provide integration points with the
/// EVM execution engine.
impl AdditionalLimit {
    /// Hook called when starting a new transaction.
    ///
    /// This method initializes the data size tracker with transaction data
    /// and checks if the initial transaction data exceeds limits.
    ///
    /// # Arguments
    ///
    /// * `tx` - The transaction being executed
    ///
    /// # Returns
    ///
    /// Returns the result of the initial limit check.
    pub(crate) fn on_new_tx(&mut self, tx: &crate::MegaTransaction) -> AdditionalLimitResult {
        // reset the tx data size tracker and kv update counter
        self.reset();

        // record the transaction data size
        self.data_size_tracker.on_tx_start(tx);

        self.check_limit()
    }

    /// Hook called when a new execution frame is initialized.
    ///
    /// This method sets up tracking for a new execution frame, allowing
    /// proper handling of nested calls and reverts.
    ///
    /// # Arguments
    ///
    /// * `init_result` - The frame initialization result
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after frame initialization.
    pub(crate) fn on_frame_init(
        &mut self,
        init_result: &FrameInitResult<'_, EthFrame<EthInterpreter>>,
    ) -> AdditionalLimitResult {
        self.data_size_tracker.on_frame_init(init_result);
        self.kv_update_counter.on_frame_init(init_result);

        self.check_limit()
    }

    /// Hook called when an execution frame returns.
    ///
    /// This method handles the completion of an execution frame, including
    /// proper cleanup for reverted frames and limit checking.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame execution result
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after frame completion.
    pub(crate) fn on_frame_return(&mut self, result: &FrameResult) -> AdditionalLimitResult {
        self.data_size_tracker.on_frame_return(result);
        self.kv_update_counter.on_frame_return(result);

        self.check_limit()
    }

    /// Hook called when a LOG operation is executed.
    ///
    /// This method tracks the data size generated by log operations,
    /// including both topics and data.
    ///
    /// # Arguments
    ///
    /// * `num_topics` - Number of log topics
    /// * `data_size` - Size of log data in bytes
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording log data.
    pub(crate) fn on_log(&mut self, num_topics: u64, data_size: u64) -> AdditionalLimitResult {
        self.data_size_tracker.on_log(num_topics, data_size);

        self.check_limit()
    }

    /// Hook called when an SSTORE operation is executed.
    ///
    /// This method tracks both data size and KV update count for storage
    /// write operations.
    ///
    /// # Arguments
    ///
    /// * `address` - The contract address
    /// * `key` - The storage slot key
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the storage operation.
    pub(crate) fn on_sstore(&mut self, address: Address, key: U256) -> AdditionalLimitResult {
        self.data_size_tracker.on_sstore(address, key);
        self.kv_update_counter.on_sstore(address, key);

        self.check_limit()
    }

    /// Hook called when an SLOAD operation is executed.
    ///
    /// This method tracks the data size generated by storage read operations.
    ///
    /// # Arguments
    ///
    /// * `address` - The contract address
    /// * `key` - The storage slot key
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the storage read.
    pub(crate) fn on_sload(&mut self, address: Address, key: U256) -> AdditionalLimitResult {
        self.data_size_tracker.on_sload(address, key);

        self.check_limit()
    }

    /// Hook called when a CREATE or CREATE2 operation is executed.
    ///
    /// This method tracks both data size and KV update count for contract
    /// creation operations.
    ///
    /// # Arguments
    ///
    /// * `created_address` - The address of the created contract
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the contract creation.
    pub(crate) fn on_create(&mut self, created_address: Address) -> AdditionalLimitResult {
        self.data_size_tracker.on_create(created_address);
        self.kv_update_counter.on_create(created_address);

        self.check_limit()
    }

    /// Hook called when a CALL operation is executed.
    ///
    /// This method tracks both data size and KV update count for call operations,
    /// but only when there is a value transfer involved.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The address being called
    /// * `has_transfer` - Whether the call involves a value transfer
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the call operation.
    pub(crate) fn on_call(
        &mut self,
        target_address: Address,
        has_transfer: bool,
    ) -> AdditionalLimitResult {
        self.data_size_tracker.on_call(target_address, has_transfer);
        self.kv_update_counter.on_call(target_address, has_transfer);

        self.check_limit()
    }
}

/// Result type indicating whether additional limits have been exceeded.
///
/// This enum provides detailed information about limit enforcement results,
/// including which specific limit was exceeded and the current usage values.
#[allow(clippy::enum_variant_names)]
#[allow(unused)]
#[allow(missing_docs)]
#[derive(Debug, Default)]
pub enum AdditionalLimitResult {
    /// Indicates that all limits are within their configured thresholds.
    #[default]
    WithinLimit,

    /// Indicates that the data size limit has been exceeded.
    ///
    /// # Fields
    ///
    /// * `limit` - The configured data size limit
    /// * `used` - The current data size usage
    ExceedsDataLimit { limit: u64, used: u64 },

    /// Indicates that the KV update limit has been exceeded.
    ///
    /// # Fields
    ///
    /// * `limit` - The configured KV update limit
    /// * `used` - The current KV update count
    ExceedsKVUpdateLimit { limit: u64, used: u64 },
}

/// Implementation of utility methods for `AdditionalLimitResult`.
#[allow(unused)]
impl AdditionalLimitResult {
    /// Checks if any limit has been exceeded.
    ///
    /// # Returns
    ///
    /// Returns `true` if either the data limit or KV update limit has been exceeded.
    pub(crate) fn exceeded_limit(&self) -> bool {
        matches!(self, Self::ExceedsDataLimit { .. } | Self::ExceedsKVUpdateLimit { .. })
    }

    /// Checks if all limits are within their configured thresholds.
    ///
    /// # Returns
    ///
    /// Returns `true` if no limits have been exceeded.
    pub(crate) fn within_limit(&self) -> bool {
        matches!(self, Self::WithinLimit)
    }
}

/// A counter for tracking key-value storage operations during transaction execution.
///
/// This struct provides frame-aware counting of storage operations, properly handling
/// nested calls and reverts. It tracks both storage writes (SSTORE) and account updates
/// (CREATE, CALL with transfers).
///
/// # Fields
///
/// * `total_count` - The total number of KV operations performed
/// * `kv_update_stack` - Stack for tracking operations per execution frame
#[derive(Debug)]
pub struct KVUpdateCounter {
    /// The total number of key-value updates performed during execution.
    total_count: u64,

    /// The stack of KV update counts per execution frame for proper revert handling.
    ///
    /// This stack allows the counter to properly handle nested calls and reverts
    /// by maintaining separate counts for each execution frame.
    kv_update_stack: Vec<u64>,
}

impl Default for KVUpdateCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl KVUpdateCounter {
    /// Creates a new `KVUpdateCounter` with zero initial count.
    ///
    /// The internal stack is pre-allocated with capacity for the maximum
    /// expected call depth to avoid reallocations during execution.
    ///
    /// # Returns
    ///
    /// Returns a new `KVUpdateCounter` instance.
    pub(crate) fn new() -> Self {
        Self {
            total_count: 0,
            kv_update_stack: Vec::with_capacity(constants::equivalence::STACK_LIMIT),
        }
    }

    /// Resets the counter to its initial state.
    ///
    /// This method clears both the total count and the frame stack,
    /// preparing the counter for a new transaction or block.
    pub fn reset(&mut self) {
        self.total_count = 0;
        self.kv_update_stack.clear();
    }

    /// Returns the current total number of key-value updates.
    ///
    /// # Returns
    ///
    /// Returns the total count of KV operations performed so far.
    #[inline]
    pub fn current_count(&self) -> u64 {
        self.total_count
    }

    /// Checks if the current KV update count exceeds the configured limit.
    ///
    /// # Returns
    ///
    /// Returns `true` if the current count exceeds the `MINI_REX` KV update limit.
    #[inline]
    pub fn exceeds_limit(&self) -> bool {
        self.total_count > constants::mini_rex::TX_KV_UPDATE_LIMIT
    }

    /// Hook called when a new execution frame is initialized.
    ///
    /// This method creates a new frame entry in the KV update stack if the frame
    /// is successfully initialized, allowing proper tracking of nested calls.
    ///
    /// # Arguments
    ///
    /// * `init_frame_result` - The frame initialization result
    pub(crate) fn on_frame_init(
        &mut self,
        init_frame_result: &FrameInitResult<'_, EthFrame<EthInterpreter>>,
    ) {
        if let FrameInitResult::Item(_) = init_frame_result {
            // the frame is successfully initialized, so we push a new frame to the kv update
            // stack.
            self.kv_update_stack.push(0);
        }
    }

    /// Hook called when an execution frame returns.
    ///
    /// This method handles the completion of an execution frame, properly managing
    /// the KV update stack and adjusting the total count based on whether the frame
    /// was reverted or completed successfully.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame execution result
    pub(crate) fn on_frame_return(&mut self, result: &FrameResult) {
        let size_to_discard = self.kv_update_stack.pop().expect("kv update stack is empty");
        if result.interpreter_result().is_revert() {
            // discard the current frame's kv update
            self.total_count -= size_to_discard;
        } else {
            // merge the current frame's kv update into the previous frame or do nothing if the
            // current frame is the last frame.
            if let Some(previous_count) = self.kv_update_stack.last_mut() {
                *previous_count += size_to_discard;
            }
        }
    }

    /// Records an update to a storage slot, using the account's address and the slot as the
    /// key. We do an estimation here by counting every sstore regardless of the uniqueness of
    /// whether the storage slot is warm or cold.
    pub(crate) fn on_sstore(&mut self, _address: Address, _slot: U256) {
        self.total_count += 1;
        self.update_current_frame_count(1);
    }

    /// Records the bytes originated from the `CREATE`/`CREATE2` opcodes. We do an estimation here
    /// by counting every account info update regardless of whether the account exists or not.
    pub(crate) fn on_create(&mut self, created_address: Address) {
        self.record_account_info_update(created_address);
    }

    /// Records the bytes originated from the `CALL` opcodes. We do an estimation here by counting
    /// every account info update regardless of whether the account is warm or cold.
    pub(crate) fn on_call(&mut self, target_address: Address, has_transfer: bool) {
        if has_transfer {
            self.record_account_info_update(target_address);
        }
    }

    /// Records an update to an account info, using the account's address as the key.
    /// We do an estimation here by counting every account info update regardless of whether the
    /// account is warm or cold.
    fn record_account_info_update(&mut self, _address: Address) {
        self.total_count += 1;
        self.update_current_frame_count(1);
    }

    fn update_current_frame_count(&mut self, n: u64) {
        let count = self.kv_update_stack.last_mut().expect("kv update stack is empty");
        *count += n;
    }
}

/// A tracker for the total data size (in bytes) generated from transaction execution.
///
/// This struct provides comprehensive tracking of all data generated during transaction
/// execution, including transaction data, logs, storage operations, and account updates.
/// It maintains frame-aware tracking to properly handle nested calls and reverts.
///
/// ## Tracked Data Types
///
/// The tracker monitors the following types of data:
/// - Intrinsic transaction data (gas limit, value, signature, etc.)
/// - Transaction calldata
/// - Access list data
/// - EIP-7702 authorization list data
/// - Log topics and data
/// - Storage slot keys and values
/// - Updated account information
/// - Created contract code
///
/// ## Frame Management
///
/// The tracker maintains a stack of frame-specific data sizes to properly handle
/// nested calls and reverts. When a frame is reverted, its data is discarded;
/// when a frame completes successfully, its data is merged into the parent frame.
///
/// # Fields
///
/// * `total_size` - The current total data size generated
/// * `frame_size_stack` - Stack for tracking data size per execution frame
#[derive(Debug)]
pub struct DataSizeTracker {
    /// The current total data size generated from the transaction execution.
    total_size: u64,

    /// The stack of data sizes to discard when a message call frame is reverted.
    ///
    /// This stack allows the tracker to properly handle nested calls and reverts
    /// by maintaining separate data size counts for each execution frame.
    frame_size_stack: Vec<u64>,
}

impl Default for DataSizeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSizeTracker {
    /// Creates a new `DataSizeTracker` with zero initial data size.
    ///
    /// The internal stack is pre-allocated with capacity for the maximum
    /// expected call depth to avoid reallocations during execution.
    ///
    /// # Returns
    ///
    /// Returns a new `DataSizeTracker` instance.
    pub(crate) fn new() -> Self {
        Self {
            total_size: 0,
            frame_size_stack: Vec::with_capacity(constants::equivalence::STACK_LIMIT),
        }
    }

    /// Resets the tracker to its initial state.
    ///
    /// This method clears both the total size and the frame stack,
    /// preparing the tracker for a new transaction or block.
    pub fn reset(&mut self) {
        self.total_size = 0;
        self.frame_size_stack.clear();
    }

    /// Returns the current total data size generated from transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the total data size in bytes generated so far.
    #[inline]
    pub fn current_size(&self) -> u64 {
        self.total_size
    }

    /// Checks if the current data size exceeds the configured limit.
    ///
    /// # Returns
    ///
    /// Returns `true` if the current size exceeds the `MINI_REX` data limit.
    #[inline]
    pub fn exceeds_limit(&self) -> bool {
        self.total_size > constants::mini_rex::TX_DATA_LIMIT
    }

    /// Records the data size of a transaction at the start of execution.
    ///
    /// This method calculates and records the total data size of the transaction,
    /// including intrinsic data, calldata, access list, and authorization list.
    ///
    /// # Arguments
    ///
    /// * `tx` - The transaction being executed
    pub(crate) fn on_tx_start(&mut self, tx: &crate::MegaTransaction) {
        // 110 bytes for the intrinsic data of a transaction, including the gas limit, value,
        // signature, gas price, etc.
        let mut size = 110;
        // bytes for the calldata of a transaction
        size += tx.input().len() as u64;
        // bytes for the access list of a transaction
        size += tx
            .access_list()
            .map(|item| item.map(|access| access.size() as u64).sum::<u64>())
            .unwrap_or_default();
        // bytes for the EIP-7702 authorization list of a transaction (101 bytes per authorization)
        size += tx.authorization_list_len() as u64 * 101;
        self.total_size += size;
        // the transaction data is non-discardable when the frame (or the transaction) is reverted
    }

    /// Hook called when a new execution frame is initialized.
    ///
    /// This method creates a new frame entry in the data size stack if the frame
    /// is successfully initialized, allowing proper tracking of nested calls.
    ///
    /// # Arguments
    ///
    /// * `init_frame_result` - The frame initialization result
    pub(crate) fn on_frame_init(
        &mut self,
        init_frame_result: &FrameInitResult<'_, EthFrame<EthInterpreter>>,
    ) {
        if let FrameInitResult::Item(_) = init_frame_result {
            // the frame is successfully initialized, so we push a new frame to the frame size stack
            self.frame_size_stack.push(0);
        }
    }

    /// Hook called when an execution frame returns.
    ///
    /// This method handles the completion of an execution frame, including
    /// recording created contract code and properly managing the data size stack
    /// based on whether the frame was reverted or completed successfully.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame execution result
    pub(crate) fn on_frame_return(&mut self, result: &FrameResult) {
        let size_to_discard = self.frame_size_stack.pop().expect("frame size stack is empty");
        if result.interpreter_result().is_revert() {
            // discard the current frame's discardable data
            self.total_size -= size_to_discard;
        } else {
            // record the created contract code in `DataSizeTracker` if the frame result is a
            // `CreateOutcome`.
            if let FrameResult::Create(outcome) = result {
                self.record_created_contract_code(outcome.result.output.len() as u64);
            }

            // merge the current frame's discardable data into the previous frame or do nothing if
            // the current frame is the last frame.
            if let Some(previous_size) = self.frame_size_stack.last_mut() {
                *previous_size += size_to_discard;
            }
        }
    }

    /// Records the data size generated by LOG operations.
    ///
    /// This method calculates and records the data size for log operations,
    /// including both topics and data.
    ///
    /// # Arguments
    ///
    /// * `num_topics` - Number of log topics
    /// * `data_size` - Size of log data in bytes
    pub(crate) fn on_log(&mut self, num_topics: u64, data_size: u64) {
        println!("topic {num_topics}, data_size {data_size}");
        let size = num_topics * 32 + data_size;
        self.total_size += size;
        self.update_current_frame_discardable_size(size);
    }

    /// Records the data size generated by SLOAD operations.
    ///
    /// This method calculates and records the estimated data size for storage
    /// read operations, including address, key, value, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The contract address (unused but kept for interface consistency)
    /// * `_key` - The storage slot key (unused but kept for interface consistency)
    pub(crate) fn on_sload(&mut self, _address: Address, _key: U256) {
        // address (20 bytes) + key (32 bytes) + value (32 bytes) + salt key (8 bytes) + witness
        // multipler (3) * internal trie node (32 + 8 bytes)
        let size = 20 + 32 + 32 + 8 + 3 * (32 + 8);
        self.total_size += size;
        // the SLOAD data is non-discardable when the frame is reverted
    }

    /// Records the data size generated by SSTORE operations.
    ///
    /// This method calculates and records the estimated data size for storage
    /// write operations, including address, key, value, salt data, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The contract address (unused but kept for interface consistency)
    /// * `_key` - The storage slot key (unused but kept for interface consistency)
    pub(crate) fn on_sstore(&mut self, _address: Address, _key: U256) {
        // address (20 bytes) + key (32 bytes) + value (32 bytes) + salt key (8 bytes) + salt value
        // delta (84 bytes) + witness multipler (3) * internal trie node (32 + 8 bytes)
        let size = 20 + 32 + 32 + 8 + 84 + 3 * (32 + 8);
        self.total_size += size;
        // the SSTORE data should be discarded when the frame is reverted
        self.update_current_frame_discardable_size(size);
    }

    /// Records the data size generated by CREATE/CREATE2 operations.
    ///
    /// This method records the estimated data size for contract creation operations,
    /// including account information updates.
    ///
    /// # Arguments
    ///
    /// * `created_address` - The address of the created contract
    pub(crate) fn on_create(&mut self, created_address: Address) {
        // accound creation results in an account info update
        self.record_account_info_update(created_address);
    }

    /// Records the bytes originated from the `CALL` opcodes. We do an estimation here by counting
    /// every account info update regardless of whether the account is warm or cold.
    pub(crate) fn on_call(&mut self, target_address: Address, has_transfer: bool) {
        if has_transfer {
            self.record_account_info_update(target_address);
        }
    }

    /// Records the data size generated by account information updates.
    ///
    /// This internal method calculates and records the estimated data size for
    /// account updates, including address, nonce, balance, code hash, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The account address (unused but kept for interface consistency)
    fn record_account_info_update(&mut self, _address: Address) {
        // address (20 bytes) + nonce (8 bytes) + balance (32 bytes) + code hash (32 bytes) + salt
        // key (8 bytes) + salt value delta (92 bytes) + witness multipler (3) * internal trie node
        // (32 + 8 bytes)
        let size = 20 + 8 + 32 + 32 + 8 + 92 + 3 * (32 + 8);
        self.total_size += size;
        // the account info should be discarded when the frame is reverted
        self.update_current_frame_discardable_size(size);
    }

    /// Records the data size of created contract code.
    ///
    /// This internal method records the size of contract code created during
    /// CREATE/CREATE2 operations.
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the created contract code in bytes
    fn record_created_contract_code(&mut self, size: u64) {
        self.total_size += size;
        // the created contract code should be discarded when the frame is reverted
        self.update_current_frame_discardable_size(size);
    }

    /// Updates the current frame's discardable data size.
    ///
    /// This internal method adds the specified size to the current frame's
    /// discardable data size in the stack.
    ///
    /// # Arguments
    ///
    /// * `size` - The data size to add to the current frame
    fn update_current_frame_discardable_size(&mut self, size: u64) {
        let discarded = self.frame_size_stack.last_mut().expect("frame size stack is empty");
        *discarded += size;
    }
}

/// Creates a `FrameResult` indicating that the limit is exceeded.
///
/// This utility function creates a frame result that signals limit exceeded,
/// consuming all remaining gas and discarding any output.
///
/// # Arguments
///
/// * `gas_limit` - The gas limit of the transaction
/// * `return_memory_offset` - The memory offset of the return value if the frame is a call frame.
///   `None` if the frame is a create frame
///
/// # Returns
///
/// A `FrameResult` indicating that the limit is exceeded with
/// [`AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT`] instruction result.
pub(crate) fn exceeding_limit_frame_result(
    gas_limit: u64,
    return_memory_offset: Option<Range<usize>>,
) -> FrameResult {
    match return_memory_offset {
        None => FrameResult::Create(CreateOutcome::new(
            InterpreterResult::new(
                AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                Bytes::new(),
                Gas::new_spent(gas_limit),
            ),
            None,
        )),
        Some(return_memory_offset) => FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                Bytes::new(),
                Gas::new_spent(gas_limit),
            ),
            return_memory_offset,
        )),
    }
}

/// Marks an existing frame result as exceeding the limit.
///
/// This utility function modifies an existing frame result to indicate that
/// limits have been exceeded, consuming all remaining gas and discarding output.
///
/// # Arguments
///
/// * `result` - The frame result to modify
///
/// # Returns
///
/// Returns the modified frame result with limit exceeded status.
pub(crate) fn mark_frame_result_as_exceeding_limit(mut result: FrameResult) -> FrameResult {
    // spend all the gas and discard all the refund
    let gas = result.gas_mut();
    gas.spend_all();
    gas.set_refund(0);

    // mark the instruction result as exceeding the limit and discard the output
    match result {
        FrameResult::Call(ref mut outcome) => {
            outcome.result.result = AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT;
            outcome.result.output = Bytes::new();
        }
        FrameResult::Create(ref mut outcome) => {
            outcome.result.result = AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT;
            outcome.result.output = Bytes::new();
        }
    }

    result
}
