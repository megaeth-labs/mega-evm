use core::cmp::min;

use crate::{
    constants::{
        self,
        equivalence::{CALL_STIPEND, WARM_SSTORE_RESET, WARM_STORAGE_READ_COST},
    },
    force_limit_remaining_gas, AdditionalLimit, ExternalEnvs, HostExt, MegaContext, MegaSpecId,
};
use alloy_evm::Database;
use alloy_primitives::{keccak256, Bytes, Log, LogData, B256};
use revm::{
    bytecode::opcode::{
        BASEFEE, BLOBBASEFEE, BLOBHASH, BLOCKHASH, CALL, COINBASE, CREATE, CREATE2, DIFFICULTY,
        GASLIMIT, LOG0, LOG1, LOG2, LOG3, LOG4, NUMBER, SELFDESTRUCT, SSTORE, TIMESTAMP,
    },
    context::{ContextTr, CreateScheme, Host, JournalTr},
    handler::instructions::{EthInstructions, InstructionProvider},
    interpreter::{
        as_usize_or_fail, check,
        gas::{self, cost_per_word, warm_cold_cost_with_delegation},
        gas_or_fail,
        instructions::{
            self, contract::get_memory_input_and_out_ranges, control, utility::IntoAddress,
        },
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, LoopControl, MemoryTr, RuntimeFlag, StackTr},
        popn, require_non_staticcall, resize_memory, CallInput, CallInputs, CallScheme, CallValue,
        CreateInputs, FrameInput, InstructionContext, InstructionResult, InstructionTable,
        InterpreterAction, InterpreterTypes,
    },
    primitives::{self},
};

/// `MegaethInstructions` is the instruction table for `MegaETH`.
///
/// This instruction table customizes certain opcodes for `MegaETH` specifications:
/// - LOG opcodes with 100x gas cost increase and data size limit enforcement
/// - SELFDESTRUCT opcode completely disabled (halts with `InvalidFEOpcode`)
/// - SSTORE opcode with dynamically-scaled gas cost and data/KV limit enforcement
/// - CREATE/CREATE2 opcodes with dynamically-scaled + flat gas cost and limit enforcement
/// - CALL opcode with dynamically-scaled new account gas cost and oracle access detection
/// - Block environment opcodes (TIMESTAMP, NUMBER, etc.) with immediate gas limiting to prevent
///   `DoS`
///
/// # Assumptions
///
/// This instruction table is only used when the `MINI_REX` spec is enabled, so we can safely assume
/// that all features before and including Mini-Rex are enabled.
#[derive(Clone)]
pub struct MegaInstructions<DB: Database, ExtEnvs: ExternalEnvs> {
    spec: MegaSpecId,
    inner: EthInstructions<EthInterpreter, MegaContext<DB, ExtEnvs>>,
}

impl<DB: Database, ExtEnvs: ExternalEnvs> core::fmt::Debug for MegaInstructions<DB, ExtEnvs> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethInstructions").field("spec", &self.spec).finish_non_exhaustive()
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvs> MegaInstructions<DB, ExtEnvs> {
    /// Create a new `MegaethInstructions` with the given spec id.
    pub fn new(spec: MegaSpecId) -> Self {
        let this = Self { spec, inner: EthInstructions::new_mainnet() };
        this.with_spec(spec)
    }

    fn with_spec(mut self, spec: MegaSpecId) -> Self {
        if spec.is_enabled(MegaSpecId::MINI_REX) {
            // Override the LOG instructions with 100x gas cost increase and data limit enforcement
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

            // Override the CREATE and CREATE2 instructions
            self.inner.insert_instruction(CREATE, create_with_bomb::<_, false, _>);
            self.inner.insert_instruction(CREATE2, create_with_bomb::<_, true, _>);

            // Override the CALL instruction
            self.inner.insert_instruction(CALL, call_with_bomb);

            // Override block environment opcodes to immediately limit gas upon access
            // This prevents DoS attacks using sensitive block environment data
            self.inner.insert_instruction(TIMESTAMP, timestamp_limit_gas);
            self.inner.insert_instruction(NUMBER, block_number_limit_gas);
            self.inner.insert_instruction(COINBASE, coinbase_limit_gas);
            self.inner.insert_instruction(DIFFICULTY, difficulty_limit_gas);
            self.inner.insert_instruction(GASLIMIT, gas_limit_opcode_limit_gas);
            self.inner.insert_instruction(BASEFEE, basefee_limit_gas);
            self.inner.insert_instruction(BLOCKHASH, blockhash_limit_gas);
            self.inner.insert_instruction(BLOBBASEFEE, blobbasefee_limit_gas);
            self.inner.insert_instruction(BLOBHASH, blobhash_limit_gas);
        }
        self
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvs> InstructionProvider for MegaInstructions<DB, ExtEnvs> {
    type Context = MegaContext<DB, ExtEnvs>;
    type InterpreterTypes = EthInterpreter;

    fn instruction_table(&self) -> &InstructionTable<Self::InterpreterTypes, Self::Context> {
        self.inner.instruction_table()
    }
}

/// `LOG` opcode implementation modified from `revm` with increased gas costs and data size limit
/// enforcement.
///
/// # Differences from the standard EVM
///
/// 1. **Increased Gas Costs**: LOG topics cost 100x more (37,500 vs 375), LOG data costs 100x more
///    (800 vs 8 per byte)
/// 2. **Data Size Limit**: Checks if total transaction data size exceeds `TX_DATA_LIMIT` (3.125 MB)
/// 3. **Limit Enforcement**: Halts with `OutOfGas` when data limit exceeded, consuming all
///    remaining gas
///
/// # Assumptions
///
/// This alternative implementation of `LOG` is only used when the `MINI_REX` spec is enabled.
pub fn log_with_data_bomb<const N: usize, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, impl InterpreterTypes>,
) {
    require_non_staticcall!(context.interpreter);

    popn!([offset, len], context.interpreter);
    let len = as_usize_or_fail!(context.interpreter, len);
    // MegaETH modification: calculate the increased gas cost for log topics and data
    let log_cost = {
        let topic_cost = constants::mini_rex::LOG_TOPIC_GAS.checked_mul(N as u64);
        let data_cost = constants::mini_rex::LOG_DATA_GAS.checked_mul(len as u64);
        topic_cost
            .and_then(|topic| data_cost.and_then(|cost| cost.checked_add(topic)))
            .and_then(|cost| cost.checked_add(constants::equivalence::LOG))
    };
    gas_or_fail!(context.interpreter, log_cost);
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

/// `SSTORE` opcode implementation modified from `revm` with dynamically-scaled gas costs and limit
/// enforcement.
///
/// # Differences from the standard EVM
///
/// 1. **Dynamic Gas Costs**: Base cost 2,000,000 gas, multiplied by `bucket_capacity /
///    MIN_BUCKET_SIZE`
/// 2. **Data Size Tracking**: Adds 40 bytes when original ≠ new value AND first write to slot
/// 3. **KV Update Tracking**: Adds 1 KV update when original ≠ new value AND first write to slot
/// 4. **Limit Enforcement**: Halts with `OutOfGas` when data (3.125 MB) or KV (1,000) limits
///    exceeded
/// 5. **Refund Logic**: Refunds data/KV when slot reset to original value
///
/// # Assumptions
///
/// This alternative implementation of `SSTORE` is only used when the `MINI_REX` spec is enabled.
/// so we can safely assume that all features before and including Mini-Rex are enabled.
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
        let Ok(sstore_set_gas) = context.host.sstore_set_gas(target_address, index) else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return;
        };
        sstore_set_gas
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

    // KV update bomb and data bomb (only when first writing non-zero value to originally zero
    // slot): check if the number of key-value updates or the total data size will exceed the
    // limit, if so, halt.
    if context
        .host
        .additional_limit()
        .borrow_mut()
        .on_sstore(target_address, index, loaded_data)
        .exceeded_limit()
    {
        context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
    }
}

/// `CREATE`/`CREATE2` opcode implementation modified from `revm` with increased gas costs and limit
/// enforcement.
///
/// # Differences from the standard EVM
///
/// 1. **Dynamic New Account Gas**: Base 2,000,000 gas, multiplied by `bucket_capacity /
///    MIN_BUCKET_SIZE`
/// 2. **Additional Create Gas**: Flat 2,000,000 gas fee on top of new account cost
/// 3. **Data/KV Tracking**: Account creation adds 40 bytes data and 1 KV update
/// 4. **Contract Code Tracking**: Deployed bytecode size added to transaction data
/// 5. **Limit Enforcement**: Halts when data or KV limits exceeded
///
/// # Assumptions
///
/// This alternative implementation of `CREATE`/`CREATE2` is only used when the `MINI_REX` spec is
/// enabled, so we can safely assume that all features before and including `MINI_REX` are enabled.
pub fn create_with_bomb<
    WIRE: InterpreterTypes,
    const IS_CREATE2: bool,
    H: HostExt + ContextTr + ?Sized,
>(
    context: InstructionContext<'_, H, WIRE>,
) {
    require_non_staticcall!(context.interpreter);

    let target_address = context.interpreter.input.target_address();

    // EIP-1014: Skinny CREATE2
    if IS_CREATE2 {
        check!(context.interpreter, PETERSBURG);
    }

    popn!([value, code_offset, len], context.interpreter);
    let initcode_len = as_usize_or_fail!(context.interpreter, len);

    let mut code = Bytes::new();
    if initcode_len != 0 {
        // EIP-3860: Limit and meter initcode
        // Limit is set as double of max contract bytecode size
        if initcode_len > context.host.max_initcode_size() {
            context.interpreter.halt(InstructionResult::CreateInitCodeSizeLimit);
            return;
        }
        revm::interpreter::gas!(context.interpreter, gas::initcode_cost(initcode_len));

        let code_offset = as_usize_or_fail!(context.interpreter, code_offset);
        resize_memory!(context.interpreter, code_offset, initcode_len);
        code = Bytes::copy_from_slice(
            context.interpreter.memory.slice_len(code_offset, initcode_len).as_ref(),
        );
    }

    // EIP-1014: Skinny CREATE2
    // The gas cost of CREATE is retrieved from the host, increased to
    // [`CREATE_GAS`](constants::mini_rex::CREATE_GAS) initially, and doubling as the
    // corresponding SALT bucket capacity doubles.
    let scheme = if IS_CREATE2 {
        popn!([salt], context.interpreter);

        // calculate the created address
        let init_code_hash = keccak256(&code);
        let created_address = target_address.create2(salt.to_be_bytes(), init_code_hash);
        // MegaETH modification: gas cost for creating a new account
        let Ok(new_account_gas) = context.host.new_account_gas(created_address) else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return;
        };
        let create2_cost = cost_per_word(initcode_len, constants::equivalence::KECCAK256WORD)
            .and_then(|cost| new_account_gas.checked_add(cost))
            // MegaETH modification: add additional gas cost for creating a new contract
            .and_then(|cost| cost.checked_add(constants::mini_rex::CREATE_GAS));
        gas_or_fail!(context.interpreter, create2_cost);
        CreateScheme::Create2 { salt }
    } else {
        // calculate the created address
        let Ok(creater) = context.host.journal_mut().load_account(target_address) else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return;
        };
        let created_address = target_address.create(creater.data.info.nonce);
        // MegaETH modification: gas cost for creating a new account
        let Ok(new_account_gas) = context.host.new_account_gas(created_address) else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return;
        };
        // MegaETH modification: add additional gas cost for creating a new contract
        let create_cost = new_account_gas.checked_add(constants::mini_rex::CREATE_GAS);
        gas_or_fail!(context.interpreter, create_cost);
        CreateScheme::Create
    };

    let mut gas_limit = context.interpreter.gas.remaining();

    // EIP-150: Gas cost changes for IO-heavy operations
    // MegaETH modification: Take remaining gas and keep 98/100 of it.
    gas_limit -= gas_limit * 2 / 100;

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
}

/// `CALL` opcode implementation modified from `revm` with increased new account gas costs and limit
/// enforcement.
///
/// # Differences from the standard EVM
///
/// 1. **Dynamic New Account Gas**: When calling empty account with transfer, base 2,000,000 gas
///    multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
/// 2. **Data/KV Tracking**: Value transfers to empty accounts add 40 bytes data and 2 KV updates
///    (caller + callee)
/// 3. **Limit Enforcement**: Operations halt when transaction data or KV limits exceeded
///
/// # Assumptions
///
/// This alternative implementation of `CALL` is only used when the `MINI_REX` spec is enabled, so
/// we can safely assume that all features before and including Mini-Rex are enabled.
pub fn call_with_bomb<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    popn!([local_gas_limit, to, value], context.interpreter);
    let to = to.into_address();
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    // Check if calling the oracle contract and mark it as accessed
    context.host.sensitive_data_tracker().borrow_mut().check_and_mark_oracle_access(&to);

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
                let Ok(new_account_gas) = context.host.new_account_gas(to) else {
                    context.interpreter.halt(InstructionResult::FatalExternalError);
                    return;
                };
                gas += new_account_gas;
            }
        }

        gas
    };
    revm::interpreter::gas!(context.interpreter, call_cost);

    // EIP-150: Gas cost changes for IO-heavy operations
    // MegaETH modification: replace 63/64 rule with 98/100 rule
    let remaining_gas =
        context.interpreter.gas.remaining() - context.interpreter.gas.remaining() * 2 / 100;
    let mut gas_limit = min(remaining_gas, local_gas_limit);

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
}

/* BELOW are: block environment opcode handlers with immediate gas limiting.

These custom instruction handlers override the standard EVM block environment opcodes
(TIMESTAMP, NUMBER, etc.) to immediately limit remaining gas after accessing sensitive
block environment data. This prevents DoS attacks using block environment information.

# Gas Limiting Behavior

When any block environment opcode executes:
1. The opcode executes normally (calls host method, pushes result to stack)
2. Gas is **immediately** limited to `SENSITIVE_DATA_ACCESS_REMAINING_GAS` (10,000)
3. Transaction must complete quickly with limited remaining gas

This is similar to oracle access limiting but happens immediately instead of on frame return.
*/

/// Macro to create block environment opcode handlers with gas limiting.
///
/// This macro generates a wrapper function that:
/// 1. Calls the original instruction implementation from revm
/// 2. Immediately limits remaining gas after execution
///
/// # Arguments
/// * `$fn_name` - Name of the wrapper function to generate
/// * `$opcode_name` - Human-readable opcode name for documentation
/// * `$original_fn` - Path to the original instruction function (e.g.,
///   `instructions::block_info::timestamp`)
macro_rules! wrap_op_force_gas {
    ($fn_name:ident, $opcode_name:expr, $original_fn:path) => {
        #[doc = concat!("`", $opcode_name, "` opcode with immediate gas limiting after execution.")]
        pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
            mut context: InstructionContext<'_, H, WIRE>,
        ) {
            let ctx = InstructionContext::<'_, H, WIRE> {
                interpreter: &mut context.interpreter,
                host: &mut context.host,
            };
            $original_fn(ctx);
            let mut tracker = context.host.sensitive_data_tracker().borrow_mut();
            force_limit_remaining_gas(&mut context.interpreter.gas, &mut tracker);
        }
    };
}

// Generate all block environment opcode handlers with gas limiting
wrap_op_force_gas!(timestamp_limit_gas, "TIMESTAMP", instructions::block_info::timestamp);
wrap_op_force_gas!(block_number_limit_gas, "NUMBER", instructions::block_info::block_number);
wrap_op_force_gas!(difficulty_limit_gas, "DIFFICULTY", instructions::block_info::difficulty);
wrap_op_force_gas!(gas_limit_opcode_limit_gas, "GASLIMIT", instructions::block_info::gaslimit);
wrap_op_force_gas!(basefee_limit_gas, "BASEFEE", instructions::block_info::basefee);
wrap_op_force_gas!(coinbase_limit_gas, "COINBASE", instructions::block_info::coinbase);
wrap_op_force_gas!(blockhash_limit_gas, "BLOCKHASH", instructions::host::blockhash);
wrap_op_force_gas!(blobbasefee_limit_gas, "BLOBBASEFEE", instructions::block_info::blob_basefee);
wrap_op_force_gas!(blobhash_limit_gas, "BLOBHASH", instructions::tx_info::blob_hash);
