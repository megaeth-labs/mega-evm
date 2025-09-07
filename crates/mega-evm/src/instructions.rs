use core::{cell::RefCell, cmp::min};
use std::{collections::hash_map::Entry, rc::Rc, sync::Arc};

use crate::{
    constants::{
        self,
        equivalence::{CALL_STIPEND, LOG, LOGDATA, WARM_SSTORE_RESET, WARM_STORAGE_READ_COST},
        mini_rex::SSTORE_SET_GAS,
    },
    slot_to_bucket_id, AdditionalLimit, Context, DataSizeTracker, ExternalEnvOracle, HostExt,
    KVUpdateCounter, SpecId,
};
use alloy_evm::Database;
use alloy_primitives::{Address, BlockNumber, Bytes, Log, LogData, B256, U256};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, CREATE2, LOG0, LOG1, LOG2, LOG3, LOG4, SELFDESTRUCT, SLOAD, SSTORE,
    },
    context::{CreateScheme, Host},
    handler::instructions::{EthInstructions, InstructionProvider},
    interpreter::{
        _count, as_usize_or_fail, check,
        gas::{self, cost_per_word, warm_cold_cost_with_delegation, KECCAK256WORD},
        gas_or_fail,
        instructions::{
            contract::{calc_call_gas, get_memory_input_and_out_ranges},
            control,
            utility::IntoAddress,
        },
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, LoopControl, MemoryTr, RuntimeFlag, StackTr},
        popn, popn_top, require_non_staticcall, resize_memory, tri, CallInput, CallInputs,
        CallScheme, CallValue, CreateInputs, FrameInput, InstructionContext, InstructionResult,
        InstructionTable, Interpreter, InterpreterAction, InterpreterTypes,
    },
    primitives::{self, HashMap},
};
use salt::{constant::MIN_BUCKET_SIZE, BucketId};

/// `MegaethInstructions` is the instruction table for `MegaETH`.
///
/// This instruction table customizes certain opcodes for `MegaETH` specifications:
/// - LOG opcodes with quadratic data cost after Mini-Rex
/// - SELFDESTRUCT opcode disabled after Mini-Rex to prevent contract destruction
/// - SSTORE opcode with increased gas cost and data bomb
/// - SLOAD opcode with data bomb
/// - CREATE and CREATE2 opcode with increased gas cost, data bomb, and kv update bomb
/// - CALL opcode with data bomb and kv update bomb
///
/// # Assumptions
///
/// In Mega-EVM, we fork the standard EVM at Prague, so we can safely assume that all features
/// before and including Prague are enabled.
#[derive(Clone)]
pub struct Instructions<DB: Database, Oracle: ExternalEnvOracle> {
    spec: SpecId,
    inner: EthInstructions<EthInterpreter, Context<DB, Oracle>>,
}

impl<DB: Database, Oracle: ExternalEnvOracle> core::fmt::Debug for Instructions<DB, Oracle> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethInstructions").field("spec", &self.spec).finish_non_exhaustive()
    }
}

impl<DB: Database, Oracle: ExternalEnvOracle> Instructions<DB, Oracle> {
    /// Create a new `MegaethInstructions` with the given spec id.
    pub fn new(spec: SpecId) -> Self {
        let this = Self { spec, inner: EthInstructions::new_mainnet() };
        this.with_spec(spec)
    }

    fn with_spec(mut self, spec: SpecId) -> Self {
        if spec.is_enabled(SpecId::MINI_REX) {
            // Override the LOG instructions and use our own implementation with quadratic data cost
            self.inner.insert_instruction(LOG0, log_with_data_bomb::<0, _>);
            self.inner.insert_instruction(LOG1, log_with_data_bomb::<1, _>);
            self.inner.insert_instruction(LOG2, log_with_data_bomb::<2, _>);
            self.inner.insert_instruction(LOG3, log_with_data_bomb::<3, _>);
            self.inner.insert_instruction(LOG4, log_with_data_bomb::<4, _>);

            // Disallow SELFDESTRUCT opcode in Mini-Rex spec
            // This prevents contracts from being permanently destroyed
            // When executed, it will halt with InvalidFEOpcode
            self.inner.insert_instruction(SELFDESTRUCT, control::invalid);

            // Override the SSTORE instruction
            self.inner.insert_instruction(SSTORE, sstore_with_bomb);

            // Override the SLOAD instruction
            self.inner.insert_instruction(SLOAD, sload_with_bomb);

            // Override the CREATE and CREATE2 instructions
            self.inner.insert_instruction(CREATE, create_with_bomb::<_, false, _>);
            self.inner.insert_instruction(CREATE2, create_with_bomb::<_, true, _>);

            // Override the CALL instruction
            self.inner.insert_instruction(CALL, call_with_bomb);
        }
        self
    }
}

impl<DB: Database, Oracle: ExternalEnvOracle> InstructionProvider for Instructions<DB, Oracle> {
    type Context = Context<DB, Oracle>;
    type InterpreterTypes = EthInterpreter;

    fn instruction_table(&self) -> &InstructionTable<Self::InterpreterTypes, Self::Context> {
        self.inner.instruction_table()
    }
}

/// `LOG` opcode implementation modified from `revm` to support data bomb. The only difference is
/// that after all the logic of the [standard EVM](revm::interpreter::instructions::host::log), we
/// check if the total data size exceeds the limit.
pub fn log_with_data_bomb<const N: usize, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, impl InterpreterTypes>,
) {
    require_non_staticcall!(context.interpreter);

    popn!([offset, len], context.interpreter);
    let len = as_usize_or_fail!(context.interpreter, len);
    gas_or_fail!(context.interpreter, gas::log_cost(N as u8, len as u64));
    let data = if len == 0 {
        Bytes::new()
    } else {
        let offset = as_usize_or_fail!(context.interpreter, offset);
        resize_memory!(context.interpreter, offset, len);
        Bytes::copy_from_slice(context.interpreter.memory.slice_len(offset, len).as_ref())
    };
    if context.interpreter.stack.len() < N {
        context.interpreter.halt(InstructionResult::StackUnderflow);
        return;
    }
    let Some(topics) = context.interpreter.stack.popn::<N>() else {
        context.interpreter.halt(InstructionResult::StackUnderflow);
        return;
    };

    let log = Log {
        address: context.interpreter.input.target_address(),
        data: LogData::new(topics.into_iter().map(B256::from).collect(), data)
            .expect("LogData should have <=4 topics"),
    };

    context.host.log(log);

    /* The above logic is the same as the standard EVM's. The below is the data bomb logic. */

    // Record the size of the log topics and data. If the total data size exceeds the limit, we
    // halt.
    if context.host.additional_limit().borrow_mut().on_log(N as u64, len as u64).exceeded_limit() {
        context.interpreter.halt(InstructionResult::MemoryLimitOOG);
    }
}

/// `SSTORE` opcode implementation modified from `revm` to support increased gas cost and data bomb.
/// The difference from the standard EVM are:
/// - The gas cost for setting a storage slot to a non-zero value is increased to [`SSTORE_SET_GAS`]
///   initially, and doubles as the corresponding SALT bucket capacity doubles.
/// - The data bomb. When the total amount of data generated by the current transaction exceeds
/// - The kv update bomb. When the number of unique key-value updates exceeds the limit, the
///   transaction will error and halt (consuming all remaining gas).
///
/// Mega-evm forks the standard EVM at Prague, so we can safely assume that Berlin, Istanbul, and
/// Frontier hardforks are enabled.
pub fn sstore_with_bomb<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    require_non_staticcall!(context.interpreter);

    popn!([index, value], context.interpreter);

    let target_address = context.interpreter.input.target_address();
    let Some(state_load) = context.host.sstore(target_address, index, value) else {
        context.interpreter.halt(InstructionResult::FatalExternalError);
        return;
    };

    // EIP-1706 Disable SSTORE with gasleft lower than call stipend. EIP-1706 is guaranteed to be
    // enabled in mega-evm.
    if context.interpreter.gas.remaining() <= CALL_STIPEND {
        context.interpreter.halt(InstructionResult::ReentrancySentryOOG);
        return;
    }

    // We directly use EIP-2200 to calculate the gas cost, since it is guaranteed to be enabled in
    // mega-evm.
    // In addition, we increase the gas cost for setting a storage slot to a non-zero value. Other
    // gas costs are the same as the standard EVM.
    let loaded_data = &state_load.data;
    let gas_cost = if loaded_data.is_new_eq_present() {
        WARM_STORAGE_READ_COST
    } else if loaded_data.is_original_eq_present() && loaded_data.is_original_zero() {
        // dynamically calculate the gas cost based on the SALT bucket capacity
        context.host.sstore_set_gas(target_address, index)
    } else if loaded_data.is_original_eq_present() {
        WARM_SSTORE_RESET
    } else {
        WARM_STORAGE_READ_COST
    };
    revm::interpreter::gas!(context.interpreter, gas_cost);

    context
        .interpreter
        .gas
        .record_refund(gas::sstore_refund(context.interpreter.runtime_flag.spec_id(), loaded_data));

    // KV update bomb and data bomb: check if the number of key-value updates or the total data size
    // will exceed the limit, if so, halt.
    if context
        .host
        .additional_limit()
        .borrow_mut()
        .on_sstore(target_address, index)
        .exceeded_limit()
    {
        context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
    }
}

/// `SLOAD` opcode implementation modified from `revm` to support data bomb. The difference from
/// the standard EVM is that we check if the total data size exceeds the limit.
pub fn sload_with_bomb<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    popn_top!([], index, context.interpreter);

    let target_address = context.interpreter.input.target_address();

    let Some(value) = context.host.sload(target_address, *index) else {
        context.interpreter.halt(InstructionResult::FatalExternalError);
        return;
    };

    revm::interpreter::gas!(
        context.interpreter,
        gas::sload_cost(context.interpreter.runtime_flag.spec_id(), value.is_cold)
    );
    *index = value.data;

    // The data bomb: check if the total data size exceeds the limit, if so, halt.
    if context
        .host
        .additional_limit()
        .borrow_mut()
        .on_sload(target_address, *index)
        .exceeded_limit()
    {
        context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
    }
}

/// `CREATE`/`CREATE2` opcode implementation modified from `revm` to support increased gas cost,
/// data bomb, and kv update bomb.
///
/// # Difference from the standard EVM
///
/// - The gas cost for creating a new contract is increased to
///   [`CREATE_GAS`](constants::mini_rex::CREATE_GAS) initially, and doubles as the corresponding
///   SALT bucket capacity doubles.
/// - The data bomb. This opcode creates a new account and generates contract code, so more data is
///   resulted as the result of transaction execution. If the total data size exceeds the limit, the
///   transaction will error and halt (consuming all remaining gas).
/// - The kv update bomb. This opcode creates a new account, so more unique key-value updates are
///   generated as the result of transaction execution. If the number of unique key-value updates
///   exceeds the limit, the transaction will error and halt (consuming all remaining gas).
///
/// # Assumptions
///
/// In Mega-EVM, we fork the standard EVM at Prague, so we can safely assume that all features
/// before and including Prague are enabled.
pub fn create_with_bomb<WIRE: InterpreterTypes, const IS_CREATE2: bool, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    require_non_staticcall!(context.interpreter);

    let target_address = context.interpreter.input.target_address();

    // EIP-1014: Skinny CREATE2
    if IS_CREATE2 {
        check!(context.interpreter, PETERSBURG);
    }

    popn!([value, code_offset, len], context.interpreter);
    let len = as_usize_or_fail!(context.interpreter, len);

    let mut code = Bytes::new();
    if len != 0 {
        // EIP-3860: Limit and meter initcode
        // Limit is set as double of max contract bytecode size
        if len > context.host.max_initcode_size() {
            context.interpreter.halt(InstructionResult::CreateInitCodeSizeLimit);
            return;
        }
        revm::interpreter::gas!(context.interpreter, gas::initcode_cost(len));

        let code_offset = as_usize_or_fail!(context.interpreter, code_offset);
        resize_memory!(context.interpreter, code_offset, len);
        code =
            Bytes::copy_from_slice(context.interpreter.memory.slice_len(code_offset, len).as_ref());
    }

    // EIP-1014: Skinny CREATE2
    // The gas cost of CREATE is retrieved from the host, increased to
    // [`CREATE_GAS`](constants::mini_rex::CREATE_GAS) initially, and doubling as the
    // corresponding SALT bucket capacity doubles.
    let scheme = if IS_CREATE2 {
        popn!([salt], context.interpreter);
        let create_gas = context.host.new_account_gas(target_address);
        let create2_cost = cost_per_word(len, constants::equivalence::KECCAK256WORD)
            .and_then(|cost| create_gas.checked_add(cost));
        gas_or_fail!(context.interpreter, create2_cost);
        CreateScheme::Create2 { salt }
    } else {
        let create_gas = context.host.new_account_gas(target_address);
        revm::interpreter::gas!(context.interpreter, create_gas);
        CreateScheme::Create
    };

    let mut gas_limit = context.interpreter.gas.remaining();

    // EIP-150: Gas cost changes for IO-heavy operations
    // Take remaining gas and deduce l64 part of it.
    gas_limit -= gas_limit / 64;

    revm::interpreter::gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context.interpreter.bytecode.set_action(InterpreterAction::NewFrame(FrameInput::Create(
        Box::new(CreateInputs {
            caller: target_address,
            scheme,
            value,
            init_code: code,
            gas_limit,
        }),
    )));

    // The kv update bomb and data bomb: check if the number of unique key-value updates or the
    // total data size will exceed the limit, if so, halt.
    if context.host.additional_limit().borrow_mut().on_create(target_address).exceeded_limit() {
        context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
    }
}

/// `CALL` opcode implementation modified from `revm` to support data bomb and kv update bomb.
///
/// # Difference from the standard EVM
///
/// - The data bomb. This opcode creates a new account and generates contract code, so more data is
///   resulted as the result of transaction execution. If the total data size exceeds the limit, the
///   transaction will error and halt (consuming all remaining gas).
/// - The kv update bomb. This opcode creates a new account, so more unique key-value updates are
///   generated as the result of transaction execution. If the number of unique key-value updates
///   exceeds the limit, the transaction will error and halt (consuming all remaining gas).
///
/// # Assumptions
///
/// In Mega-EVM, we fork the standard EVM at Prague, so we can safely assume that all features
/// before and including Prague are enabled.
pub fn call_with_bomb<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    popn!([local_gas_limit, to, value], context.interpreter);
    let to = to.into_address();
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let has_transfer = !value.is_zero();
    if context.interpreter.runtime_flag.is_static() && has_transfer {
        context.interpreter.halt(InstructionResult::CallNotAllowedInsideStatic);
        return;
    }

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(account_load) = context.host.load_account_delegated(to) else {
        context.interpreter.halt(InstructionResult::FatalExternalError);
        return;
    };

    let call_cost = {
        let is_empty = account_load.data.is_empty;
        // Account access.
        let mut gas = warm_cold_cost_with_delegation(account_load);

        // Transfer value cost
        if has_transfer {
            gas += constants::equivalence::CALLVALUE;
        }

        // New account cost
        if is_empty {
            // EIP-161: State trie clearing (invariant-preserving alternative)
            // Account only if there is value transferred.
            if has_transfer {
                gas += context.host.new_account_gas(to);
            }
        }

        gas
    };
    revm::interpreter::gas!(context.interpreter, call_cost);

    // EIP-150: Gas cost changes for IO-heavy operations
    // Take l64 part of gas_limit
    let mut gas_limit = min(context.interpreter.gas.remaining_63_of_64_parts(), local_gas_limit);

    revm::interpreter::gas!(context.interpreter, gas_limit);

    // Add call stipend if there is value to be transferred.
    if has_transfer {
        gas_limit = gas_limit.saturating_add(gas::CALL_STIPEND);
    }

    // Call host to interact with target contract
    context.interpreter.bytecode.set_action(InterpreterAction::NewFrame(FrameInput::Call(
        Box::new(CallInputs {
            input: CallInput::SharedBuffer(input),
            gas_limit,
            target_address: to,
            caller: context.interpreter.input.target_address(),
            bytecode_address: to,
            value: CallValue::Transfer(value),
            scheme: CallScheme::Call,
            is_static: context.interpreter.runtime_flag.is_static(),
            return_memory_offset,
        }),
    )));

    // The kv update bomb and data bomb: check if the number of unique key-value updates or the
    // total data size will exceed the limit, if so, halt.
    if context.host.additional_limit().borrow_mut().on_call(to, has_transfer).exceeded_limit() {
        context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
    }
}
