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
    context::{transaction::AuthorizationTr, Transaction},
    handler::{evm::FrameInitResult, EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, CallOutcome, CreateOutcome, FrameInput, Gas,
        InstructionResult, InterpreterResult,
    },
};

use crate::{constants, DynamicGasCost, MegaHaltReason};

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
        if self.data_size_tracker.exceeds_limit(self.data_limit) {
            AdditionalLimitResult::ExceedsDataLimit {
                limit: self.data_limit,
                used: self.data_size_tracker.current_size(),
            }
        } else if self.kv_update_counter.exceeds_limit(self.kv_update_limit) {
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
    pub fn is_exceeding_limit_result(&self, instruction_result: InstructionResult) -> bool {
        instruction_result == Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT &&
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

        // record the transaction data size and kv update count
        self.data_size_tracker.record_tx_data(tx);
        self.data_size_tracker.record_eip7702_account_info_update(tx);
        self.data_size_tracker.record_tx_sender_info_update(tx.caller());
        self.kv_update_counter.on_tx_start(tx);

        self.check_limit()
    }

    pub(crate) fn on_new_frame_initialized(
        &mut self,
        target_address: Option<Address>,
        transfer_or_create: bool,
    ) {
    }

    /// Hook called when a new execution frame to be run is initialized.
    ///
    /// This method sets up tracking for a new execution frame, allowing
    /// proper handling of nested calls and reverts.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the frame. `None` if the create frame failed to
    ///   initialize.
    /// * `call_bytecode_address` - The bytecode address of the call frame. `None` if the frame is
    ///   create.
    /// * `transfer_or_create` - Whether the frame has a value transfer or it is create frame
    /// * `init_result` - The frame initialization result
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after frame initialization.
    pub(crate) fn on_frame_init(
        &mut self,
        target_address: Option<Address>,
        call_bytecode_address: Option<Address>,
        transfer_or_create: bool,
        frame_init: &FrameInitResult<'_, EthFrame<EthInterpreter>>,
    ) -> AdditionalLimitResult {
        match &frame_init {
            FrameInitResult::Item(frame) => match &frame.input {
                FrameInput::Empty => unreachable!(),
                FrameInput::Call(call_inputs) => {
                    // only if the call transfers value, we need to record the account info update
                    // induced by the call
                    if call_inputs.transfers_value() {
                        self.data_size_tracker
                            .record_transfer_call_account_info_update(call_inputs.target_address);
                    }
                }
                FrameInput::Create(create_inputs) => {
                    let created_address = frame
                        .data
                        .created_address()
                        .expect("created address is none for create frame");
                    self.data_size_tracker.record_account_info_update(created_address);
                }
            },
            FrameInitResult::Result(frame_result) => {
                if !frame_result.interpreter_result().is_ok() {
                    // if frame result is not ok, indicating the nested call reverts, so we just
                    // return. The `frame_size_stack` will be popped in `on_frame_return`.
                    return AdditionalLimitResult::WithinLimit;
                }
                match frame_result {
                    FrameResult::Call(call_outcome) => {
                        self.data_size_tracker.on_call(
                            target_address.expect("target address is none for call frame"),
                            transfer_or_create,
                        );
                    }
                    FrameResult::Create(_) => {
                        self.data_size_tracker.on_create(
                            target_address.expect("target address is none for create frame"),
                        );
                    }
                }
            }
        }

        {
            // a new frame starts, we push a new frame to the frame size stack.
            self.data_size_tracker.frame_size_stack.push((
                0,
                target_address.unwrap_or_default(),
                transfer_or_create,
            ));
            match &frame_init {
                FrameInitResult::Item(frame) => match &frame.input {
                    FrameInput::Empty => unreachable!(),
                    FrameInput::Call(_) => {
                        self.data_size_tracker.on_call(
                            target_address.expect("target address is none for call frame"),
                            transfer_or_create,
                        );
                    }
                    FrameInput::Create(_) => {
                        this.on_create(
                            target_address.expect("target address is none for create frame"),
                        );
                    }
                },
                FrameInitResult::Result(frame_result) => {
                    if !frame_result.interpreter_result().is_ok() {
                        // if frame result is not ok, indicating the nested call reverts, so we just
                        // return. The `frame_size_stack` will be popped in `on_frame_return`.
                        return;
                    }
                    match frame_result {
                        FrameResult::Call(_) => {
                            this.on_call(
                                target_address.expect("target address is none for call frame"),
                                transfer_or_create,
                            );
                        }
                        FrameResult::Create(_) => {
                            this.on_create(
                                target_address.expect("target address is none for create frame"),
                            );
                        }
                    }
                }
            }
        };
        self.kv_update_counter.on_frame_init(target_address, transfer_or_create, frame_init);

        self.check_limit()
    }

    /// Hook called when the first frame of EVM execution is transient. Transient frame is a frame
    /// that does not require any code execution (e.g., call to an EOA). It is called when the
    /// frame initialization result is directly a `FrameResult`. This function is only called
    /// for the first frame because if the first frame is transient, its frame result will not
    /// be processed by the `on_frame_return` hooked in `frame_return_result`.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the transient frame. `None` if the create frame
    ///   failed to initialize.
    /// * `transfer_or_create` - Whether the transient frame has a transfer or it is create frame
    /// * `frame_result` - The frame execution result
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the transient frame.
    pub(crate) fn on_transient_first_frame(
        &mut self,
        target_address: Option<Address>,
        transfer_or_create: bool,
        frame_result: &FrameResult,
    ) -> AdditionalLimitResult {
        // if the frame result is not ok, meaning that no changes are made in this frame, so we
        // just return.
        if !frame_result.interpreter_result().is_ok() {
            return self.check_limit();
        }
        self.data_size_tracker.on_transient_first_frame(
            target_address,
            transfer_or_create,
            frame_result,
        );
        self.kv_update_counter.on_transient_first_frame(
            target_address,
            transfer_or_create,
            frame_result,
        );

        self.check_limit()
    }

    /// Hook called when the create frame returns a result.
    ///
    /// # Arguments
    ///
    /// * `interpreter_result` - The interpreter result of the create frame
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the create frame result.
    pub(crate) fn on_create_frame_result(
        &mut self,
        interpreter_result: &InterpreterResult,
    ) -> AdditionalLimitResult {
        // do nothing if the create frame result is not ok
        if interpreter_result.is_ok() {
            // record the data size for the created contract code
            self.data_size_tracker
                .record_created_contract_code(interpreter_result.output.len() as u64);
        }

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

    /// Hook called when a cold SSTORE operation is executed.
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
    pub(crate) fn on_cold_sstore(&mut self, address: Address, key: U256) -> AdditionalLimitResult {
        self.data_size_tracker.on_cold_sstore(address, key);
        self.kv_update_counter.on_cold_sstore(address, key);

        self.check_limit()
    }

    /// Hook called when a cold SLOAD operation is executed.
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
    pub(crate) fn on_cold_sload(&mut self, address: Address, key: U256) -> AdditionalLimitResult {
        self.data_size_tracker.on_cold_sload(address, key);

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

impl AdditionalLimitResult {
    /// Returns the [`MegaHaltReason`] if the limit has been exceeded, otherwise returns `None`.
    pub fn maybe_halt_reason(&self) -> Option<MegaHaltReason> {
        match self {
            Self::ExceedsDataLimit { .. } => Some(MegaHaltReason::DataLimitExceeded),
            Self::ExceedsKVUpdateLimit { .. } => Some(MegaHaltReason::KVUpdateLimitExceeded),
            Self::WithinLimit => None,
        }
    }
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
    /// The second element in the tuple indicates the callee address (target address) of the
    /// current frame. The third element in the tuple indicates whether the current frame's
    /// callee's account info is updated or not.
    ///
    /// This stack allows the counter to properly handle nested calls and reverts
    /// by maintaining separate counts for each execution frame.
    kv_update_stack: Vec<(u64, Address, bool)>,
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
    /// # Arguments
    ///
    /// * `limit` - The KV update limit to check
    ///
    /// # Returns
    ///
    /// Returns `true` if the current count exceeds the KV update limit.
    #[inline]
    pub fn exceeds_limit(&self, limit: u64) -> bool {
        self.total_count > limit
    }

    /// Records the KV update originated from the transaction.
    ///
    /// This method records the KV update originated from the transaction, including the caller
    /// and the 7702 authorization of each account. Here we do an over estimation by assuming all
    /// 7702 authorizations are valid.
    ///
    /// # Arguments
    ///
    /// * `tx` - The transaction
    pub(crate) fn on_tx_start(&mut self, tx: &crate::MegaTransaction) {
        // the caller itself needs one update on its account info
        self.record_account_info_update(tx.caller());
        for authorization in tx.authorization_list() {
            // the 7702 authorization of each account needs one update on its account info
            if let Some(authority) = authorization.authority() {
                self.record_account_info_update(authority);
            }
        }
    }

    /// Hook called when a new execution frame is initialized.
    ///
    /// This method creates a new frame entry in the KV update stack if the frame
    /// is successfully initialized, allowing proper tracking of nested calls.
    /// It also records the KV update originated from the call or create frame.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the frame, `None` if the create frame failed to
    ///   initialize. create.
    /// * `transfer_or_create` - Whether the frame has a value transfer or it is create frame
    /// * `init_frame` - The initialized frame
    pub(crate) fn on_frame_init(
        &mut self,
        target_address: Option<Address>,
        transfer_or_create: bool,
        init_frame: &FrameInitResult<'_, EthFrame<EthInterpreter>>,
    ) {
        // the frame is successfully initialized, so we push a new frame to the kv update
        // stack.
        self.kv_update_stack.push((0, target_address.unwrap_or_default(), transfer_or_create));
        match &init_frame {
            FrameInitResult::Item(frame) => match &frame.input {
                FrameInput::Empty => unreachable!(),
                FrameInput::Call(_) => {
                    self.on_call(
                        target_address.expect("target address is none for call frame"),
                        transfer_or_create,
                    );
                }
                FrameInput::Create(_) => {
                    self.on_create(
                        target_address.expect("target address is none for create frame"),
                    );
                }
            },
            FrameInitResult::Result(frame_result) => {
                if !frame_result.interpreter_result().is_ok() {
                    // if frame result is not ok, indicating the nested call reverts, so we just
                    // return. The `kv_update_stack` will be popped in `on_frame_return`.
                    return;
                }
                match frame_result {
                    FrameResult::Call(_) => {
                        self.on_call(
                            target_address.expect("target address is none for call frame"),
                            transfer_or_create,
                        );
                    }
                    FrameResult::Create(_) => {
                        self.on_create(
                            target_address.expect("target address is none for create frame"),
                        );
                    }
                }
            }
        }
    }

    /// Hook called when the first frame of EVM execution is transient. Transient frame is a frame
    /// that does not require any code execution (e.g., call to an EOA). It is called when the
    /// frame initialization result is directly a `FrameResult`. This function is only called
    /// for the first frame because if the first frame is transient, its frame result will not
    /// be processed by the `on_frame_return` hooked in `frame_return_result`.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the transient frame. `None` if the create frame
    ///   failed to initialize.
    /// * `transfer_or_create` - Whether the transient frame has a transfer or it is create frame
    /// * `frame_result` - The frame execution result
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the transient frame.
    pub(crate) fn on_transient_first_frame(
        &mut self,
        target_address: Option<Address>,
        transfer_or_create: bool,
        frame_result: &FrameResult,
    ) {
        // if the frame result is not ok, meaning that no changes are made in this frame, so we
        // just return.
        if !frame_result.interpreter_result().is_ok() {
            return;
        }
        // if the frame result is ok, meaning that some changes are made in this frame, so we need
        // to record the changes.
        match &frame_result {
            FrameResult::Call(_) => {
                self.on_call(
                    target_address.expect("target address is none for call frame"),
                    transfer_or_create,
                );
            }
            FrameResult::Create(outcome) => {
                self.on_create(outcome.address.expect("created address is none for create frame"));
            }
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
        let (size_to_discard, _, _) = self.kv_update_stack.pop().expect("kv update stack is empty");
        if result.interpreter_result().is_ok() {
            // merge the current frame's kv update into the previous frame
            self.update_current_frame_count(size_to_discard);
        } else {
            // discard the current frame's kv update
            self.total_count -= size_to_discard;
        }
    }

    /// Records a cold update to a storage slot, using the account's address and the slot as the
    /// key. We do an estimation here by counting every sstore regardless of the uniqueness of
    /// whether the storage slot is warm or cold.
    pub(crate) fn on_cold_sstore(&mut self, _address: Address, _slot: U256) {
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
    pub(crate) fn on_call(&mut self, target_address: Address, transfer_or_create: bool) {
        if transfer_or_create {
            // we also need to update the caller if the current frame's target address (i.e., the
            // caller) is not updated
            if let Some((_, caller, updated)) = self.kv_update_stack.last() {
                if !updated {
                    self.record_account_info_update(*caller);
                }
            }
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

    /// Updates the current frame's KV update count.
    ///
    /// This internal method adds the specified number of KV updates to the current frame's
    /// KV update count in the stack. If there is no current frame, meaning that we are at the
    /// beginning of the transaction or the end of the transaction, the changes will not be
    /// reverted (e.g., the caller's nonce will still be updated, even if the transaction is
    /// reverted).
    ///
    /// # Arguments
    ///
    /// * `n` - The number of KV updates to add
    fn update_current_frame_count(&mut self, n: u64) {
        if let Some((count, _, _)) = self.kv_update_stack.last_mut() {
            *count += n;
        }
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
    /// The second element in the tuple indicates the callee address (target address) of the
    /// current frame. The third element in the tuple indicates whether the current frame's
    /// callee's account info is updated or not.
    ///
    /// This stack allows the tracker to properly handle nested calls and reverts
    /// by maintaining separate data size counts for each execution frame.
    frame_size_stack: Vec<(u64, Address, bool)>,
}

impl Default for DataSizeTracker {
    fn default() -> Self {
        Self::new()
    }
}

/* data size constants */
#[allow(clippy::doc_markdown)]
#[allow(unused)]
impl DataSizeTracker {
    /// The number of bytes for the salt key.
    pub(crate) const SALT_KEY: u64 = 8;
    /// The number of bytes for the address.
    pub(crate) const ADDRESS: u64 = 20;
    /// The number of bytes for the nonce.
    pub(crate) const NONCE: u64 = 8;
    /// The number of bytes for the balance.
    pub(crate) const BALANCE: u64 = 32;
    /// The number of bytes for the code hash.
    pub(crate) const CODE_HASH: u64 = 32;
    /// The number of bytes for the storage slot key.
    pub(crate) const SLOT_KEY: u64 = 32;
    /// The number of bytes for the storage slot value.
    pub(crate) const SLOT_VALUE: u64 = 32;
    /// The number of bytes for the witness of one key-value pair.
    pub(crate) const WITNESS_OVERHEAD: u64 = 3 * (32 + 8);
    /// The number of bytes for the salt value of the account info.
    pub(crate) const SALT_VALUE_ACCOUNT_INFO: u64 = Self::ADDRESS + Self::NONCE + Self::BALANCE;
    /// The number of bytes for the salt value of the storage slot.
    pub(crate) const SALT_VALUE_STORAGE_SLOT: u64 =
        Self::ADDRESS + Self::SLOT_KEY + Self::SLOT_VALUE;
    /// The number of bytes for the salt value delta of the account info. We assume the XOR delta
    /// of address, nonce, and code hash is very small, so we can ignore them. The only significant
    /// delta is the balance. We over-estimate it to 32 bytes.
    pub(crate) const SALT_VALUE_DELTA_ACCOUNT_INFO: u64 = 32;
    /// The number of bytes for the salt value XOR delta of the storage slot. We over-estimate it to
    /// 32 bytes.
    pub(crate) const SALT_VALUE_DELTA_STORAGE_SLOT: u64 = 32;
    /// The originated data size for reading an account info.
    pub(crate) const ACCOUNT_INFO_WRITE: u64 = Self::SALT_KEY + Self::SALT_VALUE_DELTA_ACCOUNT_INFO;
    /// The originated data size for writing a storage slot.
    pub(crate) const STORAGE_SLOT_WRITE: u64 = Self::SALT_KEY + Self::SALT_VALUE_DELTA_STORAGE_SLOT;
    /// The number of bytes for the each EIP-7702 authorization.
    pub(crate) const AUTHORIZATION: u64 = 101;
    /// The number of bytes for the base transaction data.
    pub(crate) const BASE_TX: u64 = 110;
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
    /// # Arguments
    ///
    /// * `limit` - The data limit to check
    ///
    /// # Returns
    ///
    /// Returns `true` if the current size exceeds the data limit.
    #[inline]
    pub fn exceeds_limit(&self, limit: u64) -> bool {
        self.total_size > limit
    }

    /// Records the data size of a transaction at the start of execution.
    pub(crate) fn record_tx_data(&mut self, tx: &crate::MegaTransaction) {
        // 110 bytes for the intrinsic data of a transaction, including the gas limit, value,
        // signature, gas price, etc.
        let mut size = Self::BASE_TX;
        // bytes for the calldata of a transaction
        size += tx.input().len() as u64;
        // bytes for the access list of a transaction
        size += tx
            .access_list()
            .map(|item| item.map(|access| access.size() as u64).sum::<u64>())
            .unwrap_or_default();
        // bytes for the EIP-7702 authorization list of a transaction (101 bytes per authorization)
        size += tx.authorization_list_len() as u64 * Self::AUTHORIZATION;
        self.total_size += size;
        // tx data are non-discardable when the frame (or the transaction) is reverted
    }

    /// Records the data size generated by the EIP-7702 authority account info update.
    pub(crate) fn record_eip7702_account_info_update(&mut self, tx: &crate::MegaTransaction) {
        // the 7702 authorization of each account needs one update on its account info
        for authorization in tx.authorization_list() {
            let authority = authorization.authority();
            if let Some(authority) = authority {
                self.record_account_info_update(authority);
            }
        }
    }

    /// Records the data size generated by the caller's account info update.
    pub(crate) fn record_tx_sender_info_update(&mut self, sender: Address) {
        // the caller itself needs one update on its account info, e.g., nonce, balance, etc.
        self.record_account_info_update(sender);
    }

    /// Hook called when a new execution frame is initialized.
    ///
    /// This method creates a new frame entry in the data size stack if the frame
    /// is successfully initialized, allowing proper tracking of nested calls.
    /// It also records the data size originated from the call or create frame.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the frame. `None` if the create frame failed to
    ///   initialize.
    /// * `call_bytecode_address` - The bytecode address of the call frame. `None` if the frame is
    ///   create.
    /// * `transfer_or_create` - Whether the frame has a value transfer or it is create frame
    /// * `init_frame_result` - The frame initialization result. If this is `FrameResult`, it is
    ///   assumed that the initialized frame is not the first frame.
    pub(crate) fn on_frame_init(
        &mut self,
        target_address: Option<Address>,
        call_bytecode_address: Option<Address>,
        transfer_or_create: bool,
        frame_init_result: &FrameInitResult<'_, EthFrame<EthInterpreter>>,
    ) {
        // a new frame starts, we push a new frame to the frame size stack.
        self.frame_size_stack.push((0, target_address.unwrap_or_default(), transfer_or_create));
        match &frame_init_result {
            FrameInitResult::Item(frame) => match &frame.input {
                FrameInput::Empty => unreachable!(),
                FrameInput::Call(_) => {
                    self.on_call(
                        target_address.expect("target address is none for call frame"),
                        transfer_or_create,
                    );
                }
                FrameInput::Create(_) => {
                    self.on_create(
                        target_address.expect("target address is none for create frame"),
                    );
                }
            },
            FrameInitResult::Result(frame_result) => {
                if !frame_result.interpreter_result().is_ok() {
                    // if frame result is not ok, indicating the nested call reverts, so we just
                    // return. The `frame_size_stack` will be popped in `on_frame_return`.
                    return;
                }
                match frame_result {
                    FrameResult::Call(_) => {
                        self.on_call(
                            target_address.expect("target address is none for call frame"),
                            transfer_or_create,
                        );
                    }
                    FrameResult::Create(_) => {
                        self.on_create(
                            target_address.expect("target address is none for create frame"),
                        );
                    }
                }
            }
        }
    }

    /// Hook called when the first frame of EVM execution is transient. Transient frame is a frame
    /// that does not require any code execution (e.g., call to an EOA). It is called when the
    /// frame initialization result is directly a `FrameResult`. This function is only called for
    /// the first frame because if the first frame is transient, its frame result will not be
    /// processed by the `on_frame_return` hooked in `frame_return_result`.
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the transient frame. `None` if the create frame
    ///   failed to initialize.
    /// * `transfer_or_create` - Whether the transient frame has a transfer or it is create frame
    /// * `frame_result` - The frame execution result
    ///
    /// # Returns
    ///
    /// Returns the result of the limit check after recording the transient frame.
    pub(crate) fn on_transient_first_frame(
        &mut self,
        target_address: Option<Address>,
        transfer_or_create: bool,
        frame_result: &FrameResult,
    ) {
        // the target address is always read, to check if it already has code, for instance.
        // FIXME: if the create frame failed to initialize, the target address is `None`, but we
        // still need to record an account info read. We pass an default zero address here, it
        // should be ok since the target address is not used in `record_account_info_read`.
        self.record_account_info_read(target_address.unwrap_or_default());

        // if the frame result is not ok, meaning that no changes are made in this frame, so we
        // just return.
        if !frame_result.interpreter_result().is_ok() {
            return;
        }
        // if the frame result is ok, meaning that some changes are made in this frame, so we need
        // to record the changes.
        match &frame_result {
            FrameResult::Call(_) => {
                self.on_call(
                    target_address.expect("target address is none for call frame"),
                    transfer_or_create,
                );
            }
            FrameResult::Create(outcome) => {
                self.on_create(outcome.address.expect("created address is none for create frame"));
            }
        }
    }

    /// Hook called when an execution frame returns.
    ///
    /// This method handles the completion of an execution frame, properly managing the data size
    /// stack based on whether the frame was reverted or completed successfully.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame execution result
    pub(crate) fn on_frame_return(&mut self, result: &FrameResult) {
        let (size_to_discard, _, _) =
            self.frame_size_stack.pop().expect("frame size stack is empty");
        if result.interpreter_result().is_ok() {
            // merge the current frame's discardable data into the previous frame or do nothing if
            // the current frame is the last frame.
            if let Some((previous_size, _, _)) = self.frame_size_stack.last_mut() {
                *previous_size += size_to_discard;
            }
        } else {
            // discard the current frame's discardable data
            self.total_size -= size_to_discard;
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
        let size = num_topics * 32 + data_size;
        self.total_size += size;
        self.update_current_frame_discardable_size(size);
    }

    /// Records the data size generated by cold SLOAD operations.
    ///
    /// This method calculates and records the estimated data size for storage
    /// read operations, including address, key, value, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The contract address (unused but kept for interface consistency)
    /// * `_key` - The storage slot key (unused but kept for interface consistency)
    pub(crate) fn on_cold_sload(&mut self, _address: Address, _key: U256) {
        // address (20 bytes) + key (32 bytes) + value (32 bytes) + salt key (8 bytes) + witness
        // multipler (3) * internal trie node (32 + 8 bytes)
        let size = 20 + 32 + 32 + 8 + 3 * (32 + 8);
        self.total_size += size;
        // the SLOAD data is non-discardable when the frame is reverted
    }

    /// Records the data size generated by cold SSTORE operations.
    ///
    /// This method calculates and records the estimated data size for storage
    /// write operations, including address, key, value, salt data, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The contract address (unused but kept for interface consistency)
    /// * `_key` - The storage slot key (unused but kept for interface consistency)
    pub(crate) fn on_cold_sstore(&mut self, _address: Address, _key: U256) {
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

    /// Records the account info update induced by a transfer call (i.e., non-zero transfer value).
    ///
    /// # Arguments
    ///
    /// * `target_address` - The target address of the call.
    pub(crate) fn record_transfer_call_account_info_update(&mut self, target_address: Address) {
        // we also need to update the caller if the current frame's target address (i.e., the
        // caller) is not updated
        if let Some((_, caller, updated)) = self.frame_size_stack.last() {
            if !updated {
                self.record_account_info_update(*caller);
            }
        }
        self.record_account_info_update(target_address);
    }

    /// Records the bytes originated from the `CALL` opcodes. We do an estimation here by counting
    /// every account info update regardless of whether the account is warm or cold.
    #[deprecated]
    pub(crate) fn on_call(&mut self, target_address: Address, transfer_or_create: bool) {
        if transfer_or_create {
            // we also need to update the caller if the current frame's target address (i.e., the
            // caller) is not updated
            if let Some((_, caller, updated)) = self.frame_size_stack.last() {
                if !updated {
                    self.record_account_info_update(*caller);
                }
            }
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
        // if the last frame creates a contract, we don't need to record the data size for future
        // discard. This is because at this point the transaction execution ends and we know the
        // transaction didn't revert.
        if !self.frame_size_stack.is_empty() {
            // the created contract code should be discarded when the frame is reverted
            self.update_current_frame_discardable_size(size);
        }
    }

    /// Updates the current frame's discardable data size.
    ///
    /// This internal method adds the specified size to the current frame's
    /// discardable data size in the stack. If there is no current frame, meaning that we are at the
    /// beginning of the transaction or the end of the transaction, the changes will not be
    /// reverted (e.g., the caller's nonce will still be updated, even if the transaction is
    /// reverted).
    ///
    /// # Arguments
    ///
    /// * `size` - The data size to add to the current frame
    fn update_current_frame_discardable_size(&mut self, size: u64) {
        if let Some((discarded, _, _)) = self.frame_size_stack.last_mut() {
            *discarded += size;
        }
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
pub(crate) fn create_exceeding_limit_frame_result(
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
