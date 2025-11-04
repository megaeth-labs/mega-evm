use core::cmp::min;

use crate::{
    constants::{
        self,
        equivalence::{CALL_STIPEND, WARM_SSTORE_RESET, WARM_STORAGE_READ_COST},
    },
    AdditionalLimit, ExternalEnvs, HostExt, MegaContext, MegaSpecId,
};
use alloy_evm::Database;
use alloy_primitives::{keccak256, Bytes, Log, LogData, B256};
use revm::{
    bytecode::opcode::{
        BALANCE, BASEFEE, BLOBBASEFEE, BLOBHASH, BLOCKHASH, CALL, COINBASE, CREATE, CREATE2,
        DIFFICULTY, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, GASLIMIT, LOG0, LOG1, LOG2, LOG3, LOG4,
        NUMBER, SELFDESTRUCT, SSTORE, TIMESTAMP,
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
        CreateInputs, FrameInput, Instruction, InstructionContext, InstructionResult,
        InstructionTable, InterpreterAction, InterpreterTypes,
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
/// - CALL opcode with dynamically-scaled new account gas cost, oracle access detection, and
///   immediate gas detention for oracle calls
/// - Block environment opcodes (TIMESTAMP, NUMBER, etc.) with immediate gas detention to prevent
///   `DoS` attacks
/// - Beneficiary-accessing opcodes (BALANCE, EXTCODESIZE, etc.) with immediate gas detention when
///   beneficiary is accessed
///
/// # Gas Detention Mechanism
///
/// When volatile data (block environment, beneficiary, or oracle) is accessed, the system
/// implements a global gas detention mechanism:
/// 1. Remaining gas is immediately limited based on the type of volatile data:
///    - Block environment or beneficiary: `BLOCK_ENV_ACCESS_REMAINING_GAS` (20M gas)
///    - Oracle contract: `ORACLE_ACCESS_REMAINING_GAS` (1M gas)
/// 2. Most restrictive limit wins: If multiple volatile data types are accessed, the minimum (most
///    restrictive) limit applies, regardless of access order
/// 3. Detained gas is tracked and refunded at transaction end
/// 4. Users only pay for actual work performed, not for enforcement gas
/// 5. This prevents `DoS` attacks while maintaining fair gas accounting
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
        let instruction_table = match spec {
            MegaSpecId::EQUIVALENCE => EthInstructions::new_mainnet(),
            MegaSpecId::MINI_REX => {
                EthInstructions::new(instruction_table::<EthInterpreter, MegaContext<DB, ExtEnvs>>())
            }
        };
        let this = Self { spec, inner: instruction_table };
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
            self.inner.insert_instruction(TIMESTAMP, volatile_data_ext::timestamp_limit_gas);
            self.inner.insert_instruction(NUMBER, volatile_data_ext::block_number_limit_gas);
            self.inner.insert_instruction(COINBASE, volatile_data_ext::coinbase_limit_gas);
            self.inner.insert_instruction(DIFFICULTY, volatile_data_ext::difficulty_limit_gas);
            self.inner.insert_instruction(GASLIMIT, volatile_data_ext::gas_limit_opcode_limit_gas);
            self.inner.insert_instruction(BASEFEE, volatile_data_ext::basefee_limit_gas);
            self.inner.insert_instruction(BLOCKHASH, volatile_data_ext::blockhash_limit_gas);
            self.inner.insert_instruction(BLOBBASEFEE, volatile_data_ext::blobbasefee_limit_gas);
            self.inner.insert_instruction(BLOBHASH, volatile_data_ext::blobhash_limit_gas);

            // Override beneficiary-accessing opcodes to immediately limit gas when beneficiary is
            // accessed This prevents DoS attacks using beneficiary account data
            self.inner.insert_instruction(BALANCE, volatile_data_ext::balance_limit_gas);
            self.inner.insert_instruction(EXTCODESIZE, volatile_data_ext::extcodesize_limit_gas);
            self.inner.insert_instruction(EXTCODECOPY, volatile_data_ext::extcodecopy_limit_gas);
            self.inner.insert_instruction(EXTCODEHASH, volatile_data_ext::extcodehash_limit_gas);
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

/// Returns the instruction table for the `MegaETH` interpreter.
const fn instruction_table<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
) -> [Instruction<WIRE, H>; 256] {
    use revm::bytecode::opcode::*;
    let mut table = [control::unknown as Instruction<WIRE, H>; 256];

    table[STOP as usize] = compute_gas_ext::stop;
    table[ADD as usize] = compute_gas_ext::add;
    table[MUL as usize] = compute_gas_ext::mul;
    table[SUB as usize] = compute_gas_ext::sub;
    table[DIV as usize] = compute_gas_ext::div;
    table[SDIV as usize] = compute_gas_ext::sdiv;
    table[MOD as usize] = compute_gas_ext::rem;
    table[SMOD as usize] = compute_gas_ext::smod;
    table[ADDMOD as usize] = compute_gas_ext::addmod;
    table[MULMOD as usize] = compute_gas_ext::mulmod;
    table[EXP as usize] = compute_gas_ext::exp;
    table[SIGNEXTEND as usize] = compute_gas_ext::signextend;

    table[LT as usize] = compute_gas_ext::lt;
    table[GT as usize] = compute_gas_ext::gt;
    table[SLT as usize] = compute_gas_ext::slt;
    table[SGT as usize] = compute_gas_ext::sgt;
    table[EQ as usize] = compute_gas_ext::eq;
    table[ISZERO as usize] = compute_gas_ext::iszero;
    table[AND as usize] = compute_gas_ext::bitand;
    table[OR as usize] = compute_gas_ext::bitor;
    table[XOR as usize] = compute_gas_ext::bitxor;
    table[NOT as usize] = compute_gas_ext::not;
    table[BYTE as usize] = compute_gas_ext::byte;
    table[SHL as usize] = compute_gas_ext::shl;
    table[SHR as usize] = compute_gas_ext::shr;
    table[SAR as usize] = compute_gas_ext::sar;
    table[CLZ as usize] = compute_gas_ext::clz;

    table[KECCAK256 as usize] = compute_gas_ext::keccak256;

    table[ADDRESS as usize] = compute_gas_ext::address;
    table[BALANCE as usize] = compute_gas_ext::balance;
    table[ORIGIN as usize] = compute_gas_ext::origin;
    table[CALLER as usize] = compute_gas_ext::caller;
    table[CALLVALUE as usize] = compute_gas_ext::callvalue;
    table[CALLDATALOAD as usize] = compute_gas_ext::calldataload;
    table[CALLDATASIZE as usize] = compute_gas_ext::calldatasize;
    table[CALLDATACOPY as usize] = compute_gas_ext::calldatacopy;
    table[CODESIZE as usize] = compute_gas_ext::codesize;
    table[CODECOPY as usize] = compute_gas_ext::codecopy;

    table[GASPRICE as usize] = compute_gas_ext::gasprice;
    table[EXTCODESIZE as usize] = compute_gas_ext::extcodesize;
    table[EXTCODECOPY as usize] = compute_gas_ext::extcodecopy;
    table[EXTCODEHASH as usize] = compute_gas_ext::extcodehash;

    table[RETURNDATASIZE as usize] = compute_gas_ext::returndatasize;
    table[RETURNDATACOPY as usize] = compute_gas_ext::returndatacopy;
    table[BLOCKHASH as usize] = compute_gas_ext::blockhash;
    table[COINBASE as usize] = compute_gas_ext::coinbase;
    table[TIMESTAMP as usize] = compute_gas_ext::timestamp;
    table[NUMBER as usize] = compute_gas_ext::number;
    table[DIFFICULTY as usize] = compute_gas_ext::difficulty;
    table[GASLIMIT as usize] = compute_gas_ext::gaslimit;
    table[CHAINID as usize] = compute_gas_ext::chainid;
    table[SELFBALANCE as usize] = compute_gas_ext::selfbalance;
    table[BASEFEE as usize] = compute_gas_ext::basefee;
    table[BLOBBASEFEE as usize] = compute_gas_ext::blobbasefee;
    table[BLOBHASH as usize] = compute_gas_ext::blobhash;

    table[POP as usize] = compute_gas_ext::pop;
    table[MLOAD as usize] = compute_gas_ext::mload;
    table[MSTORE as usize] = compute_gas_ext::mstore;
    table[MSTORE8 as usize] = compute_gas_ext::mstore8;
    table[MSIZE as usize] = compute_gas_ext::msize;

    table[SLOAD as usize] = compute_gas_ext::sload;
    table[SSTORE as usize] = compute_gas_ext::sstore;

    table[JUMP as usize] = compute_gas_ext::jump;
    table[JUMPI as usize] = compute_gas_ext::jumpi;
    table[PC as usize] = compute_gas_ext::pc;
    table[MSIZE as usize] = compute_gas_ext::msize;
    table[GAS as usize] = compute_gas_ext::gas;
    table[JUMPDEST as usize] = compute_gas_ext::jumpdest;

    table[PUSH0 as usize] = compute_gas_ext::push0;
    table[PUSH1 as usize] = compute_gas_ext::push1;
    table[PUSH2 as usize] = compute_gas_ext::push2;
    table[PUSH3 as usize] = compute_gas_ext::push3;
    table[PUSH4 as usize] = compute_gas_ext::push4;
    table[PUSH5 as usize] = compute_gas_ext::push5;
    table[PUSH6 as usize] = compute_gas_ext::push6;
    table[PUSH7 as usize] = compute_gas_ext::push7;
    table[PUSH8 as usize] = compute_gas_ext::push8;
    table[PUSH9 as usize] = compute_gas_ext::push9;
    table[PUSH10 as usize] = compute_gas_ext::push10;
    table[PUSH11 as usize] = compute_gas_ext::push11;
    table[PUSH12 as usize] = compute_gas_ext::push12;
    table[PUSH13 as usize] = compute_gas_ext::push13;
    table[PUSH14 as usize] = compute_gas_ext::push14;
    table[PUSH15 as usize] = compute_gas_ext::push15;
    table[PUSH16 as usize] = compute_gas_ext::push16;
    table[PUSH17 as usize] = compute_gas_ext::push17;
    table[PUSH18 as usize] = compute_gas_ext::push18;
    table[PUSH19 as usize] = compute_gas_ext::push19;
    table[PUSH20 as usize] = compute_gas_ext::push20;
    table[PUSH21 as usize] = compute_gas_ext::push21;
    table[PUSH22 as usize] = compute_gas_ext::push22;
    table[PUSH23 as usize] = compute_gas_ext::push23;
    table[PUSH24 as usize] = compute_gas_ext::push24;
    table[PUSH25 as usize] = compute_gas_ext::push25;
    table[PUSH26 as usize] = compute_gas_ext::push26;
    table[PUSH27 as usize] = compute_gas_ext::push27;
    table[PUSH28 as usize] = compute_gas_ext::push28;
    table[PUSH29 as usize] = compute_gas_ext::push29;
    table[PUSH30 as usize] = compute_gas_ext::push30;
    table[PUSH31 as usize] = compute_gas_ext::push31;
    table[PUSH32 as usize] = compute_gas_ext::push32;

    table[DUP1 as usize] = compute_gas_ext::dup1;
    table[DUP2 as usize] = compute_gas_ext::dup2;
    table[DUP3 as usize] = compute_gas_ext::dup3;
    table[DUP4 as usize] = compute_gas_ext::dup4;
    table[DUP5 as usize] = compute_gas_ext::dup5;
    table[DUP6 as usize] = compute_gas_ext::dup6;
    table[DUP7 as usize] = compute_gas_ext::dup7;
    table[DUP8 as usize] = compute_gas_ext::dup8;
    table[DUP9 as usize] = compute_gas_ext::dup9;
    table[DUP10 as usize] = compute_gas_ext::dup10;
    table[DUP11 as usize] = compute_gas_ext::dup11;
    table[DUP12 as usize] = compute_gas_ext::dup12;
    table[DUP13 as usize] = compute_gas_ext::dup13;
    table[DUP14 as usize] = compute_gas_ext::dup14;
    table[DUP15 as usize] = compute_gas_ext::dup15;
    table[DUP16 as usize] = compute_gas_ext::dup16;

    table[SWAP1 as usize] = compute_gas_ext::swap1;
    table[SWAP2 as usize] = compute_gas_ext::swap2;
    table[SWAP3 as usize] = compute_gas_ext::swap3;
    table[SWAP4 as usize] = compute_gas_ext::swap4;
    table[SWAP5 as usize] = compute_gas_ext::swap5;
    table[SWAP6 as usize] = compute_gas_ext::swap6;
    table[SWAP7 as usize] = compute_gas_ext::swap7;
    table[SWAP8 as usize] = compute_gas_ext::swap8;
    table[SWAP9 as usize] = compute_gas_ext::swap9;
    table[SWAP10 as usize] = compute_gas_ext::swap10;
    table[SWAP11 as usize] = compute_gas_ext::swap11;
    table[SWAP12 as usize] = compute_gas_ext::swap12;
    table[SWAP13 as usize] = compute_gas_ext::swap13;
    table[SWAP14 as usize] = compute_gas_ext::swap14;
    table[SWAP15 as usize] = compute_gas_ext::swap15;
    table[SWAP16 as usize] = compute_gas_ext::swap16;

    table[LOG0 as usize] = compute_gas_ext::log0;
    table[LOG1 as usize] = compute_gas_ext::log1;
    table[LOG2 as usize] = compute_gas_ext::log2;
    table[LOG3 as usize] = compute_gas_ext::log3;
    table[LOG4 as usize] = compute_gas_ext::log4;

    table[CREATE as usize] = compute_gas_ext::create;
    table[CREATE2 as usize] = compute_gas_ext::create2;
    table[CALL as usize] = compute_gas_ext::call;
    table[CALLCODE as usize] = compute_gas_ext::call_code;
    table[DELEGATECALL as usize] = compute_gas_ext::delegate_call;
    table[STATICCALL as usize] = compute_gas_ext::static_call;
    table[REVERT as usize] = compute_gas_ext::revert;
    table[INVALID as usize] = compute_gas_ext::invalid;
    table[SELFDESTRUCT as usize] = compute_gas_ext::selfdestruct;

    table[RETURN as usize] = compute_gas_ext::ret;
    table[REVERT as usize] = compute_gas_ext::revert;
    table[SELFDESTRUCT as usize] = compute_gas_ext::selfdestruct;

    table
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
/// 2. **Cold Storage Access (EIP-2929)**: Additional 2,100 gas on first access to storage slot per
///    transaction
/// 3. **Data Size Tracking**: Adds 40 bytes when original ≠ new value AND first write to slot
/// 4. **KV Update Tracking**: Adds 1 KV update when original ≠ new value AND first write to slot
/// 5. **Limit Enforcement**: Halts with `OutOfGas` when data (3.125 MB) or KV (1,000) limits
///    exceeded
/// 6. **Refund Logic**: Refunds data/KV when slot reset to original value
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
    let mut gas_cost = if loaded_data.is_new_eq_present() {
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

    // EIP-2929: Add cold storage access cost if this is the first access
    if state_load.is_cold {
        gas_cost += constants::equivalence::COLD_SLOAD_COST;
    }

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

    // Check if calling the oracle contract and mark it as accessed. If so, we need to:
    // 1. Limit the forwarded gas to the child call
    // 2. Detain gas from current interpreter to enforce the limit (may be a no-op if limit
    //    unchanged)
    let mut volatile_data_tracker = context.host.volatile_data_tracker().borrow_mut();
    if volatile_data_tracker.check_and_mark_oracle_access(&to) {
        volatile_data_tracker.detain_plain_gas(&mut gas_limit);
        // Detain gas from current interpreter. This is a no-op if interpreter gas is already
        // below the limit, but necessary when oracle access further restricts an existing limit.
        volatile_data_tracker.detain_gas(&mut context.interpreter.gas);
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

/** Volatile data access opcode handlers with immediate gas detention.

These custom instruction handlers override opcodes that access volatile data (block environment,
beneficiary account data) to immediately detain remaining gas. This prevents DoS attacks while
maintaining fair gas accounting through the global gas detention mechanism.

# Gas Detention Mechanism

When volatile data is accessed:
1. The opcode executes normally (calls host method, processes data)
2. If this is the first volatile data access in the transaction:
   - A global gas limit is established based on the type:
     * Block environment or beneficiary: `BLOCK_ENV_ACCESS_REMAINING_GAS` (20M gas)
     * Oracle contract: `ORACLE_ACCESS_REMAINING_GAS` (1M gas)
   - Any gas above this limit is "detained" (tracked but not consumed)
3. Most restrictive limit wins: If additional volatile data with different limit is accessed,
   the minimum (most restrictive) limit is applied, regardless of access order
4. All subsequent opcodes are limited by this global gas limit
5. At transaction end, all detained gas is refunded to the user
6. Users only pay for actual computational work performed

This approach:
- Prevents DoS attacks by limiting execution after volatile data access
- Ensures fair billing by refunding enforcement gas
- Works across nested calls through the global limit mechanism
- Order-independent: accessing oracle then block env OR block env then oracle both result in
  the same final limit (the minimum of the two)

# Two Categories of Opcodes

## Block Environment Opcodes (Always Volatile)
These opcodes ALWAYS access volatile data and apply 20M gas limit:
- TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, BLOCKHASH, BLOBBASEFEE, BLOBHASH

## Account-Accessing Opcodes (Conditionally Volatile)
These opcodes only SOMETIMES access volatile data (20M gas limit when volatile):
- BALANCE(beneficiary_address) → volatile, applies 20M limit
- BALANCE(other_address) → not volatile, no limit
- EXTCODESIZE/EXTCODECOPY/EXTCODEHASH → same conditional behavior

For conditional opcodes:
- The Host methods detect when they access beneficiary/volatile accounts
- Gas detention only occurs if volatile data is actually accessed
- Regular account accesses don't trigger detention
*/
pub mod volatile_data_ext {
    use super::*;
    /// Macro to create opcode handlers with immediate gas detention.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Calls the original instruction implementation from revm
    /// 2. Detains gas from the interpreter to enforce the volatile data access limit
    ///
    /// The detention is managed by `VolatileDataAccessTracker.detain_gas()`, which:
    /// - On first volatile data access: establishes a global gas limit and detains excess gas
    /// - On subsequent accesses: is a no-op since gas is already below the limit
    macro_rules! wrap_op_detain_gas {
    ($fn_name:ident, $opcode_name:expr, $original_fn:path) => {
        #[doc = concat!("`", $opcode_name, "` opcode with immediate gas detention on volatile data access.")]
        #[inline]
        pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
            mut context: InstructionContext<'_, H, WIRE>,
        ) {
            let volatile_data_tracker = context.host.volatile_data_tracker().clone();

            // The volatile data tracker will be marked as accessed in the `Host` hooks.
            let ctx = InstructionContext::<'_, H, WIRE> {
                interpreter: &mut context.interpreter,
                host: &mut context.host,
            };
            $original_fn(ctx);

            // Detain gas from interpreter. This is a no-op if no volatile data was accessed
            // or if gas is already below the limit.
            volatile_data_tracker.borrow_mut().detain_gas(&mut context.interpreter.gas);
        }
    };
}

    wrap_op_detain_gas!(timestamp_limit_gas, "TIMESTAMP", compute_gas_ext::timestamp);
    wrap_op_detain_gas!(block_number_limit_gas, "NUMBER", compute_gas_ext::number);
    wrap_op_detain_gas!(difficulty_limit_gas, "DIFFICULTY", compute_gas_ext::difficulty);
    wrap_op_detain_gas!(gas_limit_opcode_limit_gas, "GASLIMIT", compute_gas_ext::gaslimit);
    wrap_op_detain_gas!(basefee_limit_gas, "BASEFEE", compute_gas_ext::basefee);
    wrap_op_detain_gas!(coinbase_limit_gas, "COINBASE", compute_gas_ext::coinbase);
    wrap_op_detain_gas!(blockhash_limit_gas, "BLOCKHASH", compute_gas_ext::blockhash);
    wrap_op_detain_gas!(blobbasefee_limit_gas, "BLOBBASEFEE", compute_gas_ext::blobbasefee);
    wrap_op_detain_gas!(blobhash_limit_gas, "BLOBHASH", compute_gas_ext::blobhash);
    wrap_op_detain_gas!(balance_limit_gas, "BALANCE", compute_gas_ext::balance);
    wrap_op_detain_gas!(extcodesize_limit_gas, "EXTCODESIZE", compute_gas_ext::extcodesize);
    wrap_op_detain_gas!(extcodecopy_limit_gas, "EXTCODECOPY", compute_gas_ext::extcodecopy);
    wrap_op_detain_gas!(extcodehash_limit_gas, "EXTCODEHASH", compute_gas_ext::extcodehash);
}
/// Compute gas recording implementation. TODO: add more doc
pub mod compute_gas_ext {
    use super::*;
    /// TODO: add more doc
    macro_rules! wrap_op_compute_gas {
        ($fn_name:ident, $opcode_name:expr, $original_fn:path) => {
            #[doc = concat!("`", $opcode_name, "` opcode with compute gas tracking.")]
            #[inline]
            pub fn $fn_name<WIRE: InterpreterTypes, H: HostExt + ?Sized>(
                mut context: InstructionContext<'_, H, WIRE>,
            ) {
                let gas_before = context.interpreter.gas.remaining();

                let ctx = InstructionContext::<'_, H, WIRE> {
                    interpreter: &mut context.interpreter,
                    host: &mut context.host,
                };
                $original_fn(ctx);

                let gas_after = context.interpreter.gas.remaining();
                let gas_used = gas_before - gas_after;
                context
                    .host
                    .additional_limit()
                    .borrow_mut()
                    .compute_gas_tracker
                    .record_gas_used(gas_used);
            }
        };
    }

    wrap_op_compute_gas!(stop, "STOP", instructions::control::stop);
    wrap_op_compute_gas!(add, "ADD", instructions::arithmetic::add);
    wrap_op_compute_gas!(mul, "MUL", instructions::arithmetic::mul);
    wrap_op_compute_gas!(sub, "SUB", instructions::arithmetic::sub);
    wrap_op_compute_gas!(div, "DIV", instructions::arithmetic::div);
    wrap_op_compute_gas!(sdiv, "SDIV", instructions::arithmetic::sdiv);
    wrap_op_compute_gas!(rem, "MOD", instructions::arithmetic::rem);
    wrap_op_compute_gas!(smod, "SMOD", instructions::arithmetic::smod);
    wrap_op_compute_gas!(addmod, "ADDMOD", instructions::arithmetic::addmod);
    wrap_op_compute_gas!(mulmod, "MULMOD", instructions::arithmetic::mulmod);
    wrap_op_compute_gas!(exp, "EXP", instructions::arithmetic::exp);
    wrap_op_compute_gas!(signextend, "SIGNEXTEND", instructions::arithmetic::signextend);

    wrap_op_compute_gas!(lt, "LT", instructions::bitwise::lt);
    wrap_op_compute_gas!(gt, "GT", instructions::bitwise::gt);
    wrap_op_compute_gas!(slt, "SLT", instructions::bitwise::slt);
    wrap_op_compute_gas!(sgt, "SGT", instructions::bitwise::sgt);
    wrap_op_compute_gas!(eq, "EQ", instructions::bitwise::eq);
    wrap_op_compute_gas!(iszero, "ISZERO", instructions::bitwise::iszero);
    wrap_op_compute_gas!(bitand, "AND", instructions::bitwise::bitand);
    wrap_op_compute_gas!(bitor, "OR", instructions::bitwise::bitor);
    wrap_op_compute_gas!(bitxor, "XOR", instructions::bitwise::bitxor);
    wrap_op_compute_gas!(not, "NOT", instructions::bitwise::not);
    wrap_op_compute_gas!(byte, "BYTE", instructions::bitwise::byte);
    wrap_op_compute_gas!(shl, "SHL", instructions::bitwise::shl);
    wrap_op_compute_gas!(shr, "SHR", instructions::bitwise::shr);
    wrap_op_compute_gas!(sar, "SAR", instructions::bitwise::sar);
    wrap_op_compute_gas!(clz, "CLZ", instructions::bitwise::clz);

    wrap_op_compute_gas!(keccak256, "KECCAK256", instructions::system::keccak256);

    wrap_op_compute_gas!(address, "ADDRESS", instructions::system::address);
    wrap_op_compute_gas!(balance, "BALANCE", instructions::host::balance);
    wrap_op_compute_gas!(origin, "ORIGIN", instructions::tx_info::origin);
    wrap_op_compute_gas!(caller, "CALLER", instructions::system::caller);
    wrap_op_compute_gas!(callvalue, "CALLVALUE", instructions::system::callvalue);
    wrap_op_compute_gas!(calldataload, "CALLDATALOAD", instructions::system::calldataload);
    wrap_op_compute_gas!(calldatasize, "CALLDATASIZE", instructions::system::calldatasize);
    wrap_op_compute_gas!(calldatacopy, "CALLDATACOPY", instructions::system::calldatacopy);
    wrap_op_compute_gas!(codesize, "CODESIZE", instructions::system::codesize);
    wrap_op_compute_gas!(codecopy, "CODECOPY", instructions::system::codecopy);

    wrap_op_compute_gas!(gasprice, "GASPRICE", instructions::tx_info::gasprice);
    wrap_op_compute_gas!(extcodesize, "EXTCODESIZE", instructions::host::extcodesize);
    wrap_op_compute_gas!(extcodecopy, "EXTCODECOPY", instructions::host::extcodecopy);
    wrap_op_compute_gas!(returndatasize, "RETURNDATASIZE", instructions::system::returndatasize);
    wrap_op_compute_gas!(returndatacopy, "RETURNDATACOPY", instructions::system::returndatacopy);
    wrap_op_compute_gas!(extcodehash, "EXTCODEHASH", instructions::host::extcodehash);
    wrap_op_compute_gas!(blockhash, "BLOCKHASH", instructions::host::blockhash);
    wrap_op_compute_gas!(coinbase, "COINBASE", instructions::block_info::coinbase);
    wrap_op_compute_gas!(timestamp, "TIMESTAMP", instructions::block_info::timestamp);
    wrap_op_compute_gas!(number, "NUMBER", instructions::block_info::block_number);
    wrap_op_compute_gas!(difficulty, "DIFFICULTY", instructions::block_info::difficulty);
    wrap_op_compute_gas!(gaslimit, "GASLIMIT", instructions::block_info::gaslimit);
    wrap_op_compute_gas!(chainid, "CHAINID", instructions::block_info::chainid);
    wrap_op_compute_gas!(selfbalance, "SELFBALANCE", instructions::host::selfbalance);
    wrap_op_compute_gas!(basefee, "BASEFEE", instructions::block_info::basefee);
    wrap_op_compute_gas!(blobhash, "BLOBHASH", instructions::tx_info::blob_hash);
    wrap_op_compute_gas!(blobbasefee, "BLOBBASEFEE", instructions::block_info::blob_basefee);

    wrap_op_compute_gas!(pop, "POP", instructions::stack::pop);
    wrap_op_compute_gas!(mload, "MLOAD", instructions::memory::mload);
    wrap_op_compute_gas!(mstore, "MSTORE", instructions::memory::mstore);
    wrap_op_compute_gas!(mstore8, "MSTORE8", instructions::memory::mstore8);
    wrap_op_compute_gas!(sload, "SLOAD", instructions::host::sload);
    wrap_op_compute_gas!(sstore, "SSTORE", instructions::host::sstore);
    wrap_op_compute_gas!(jump, "JUMP", instructions::control::jump);
    wrap_op_compute_gas!(jumpi, "JUMPI", instructions::control::jumpi);
    wrap_op_compute_gas!(pc, "PC", instructions::control::pc);
    wrap_op_compute_gas!(msize, "MSIZE", instructions::memory::msize);
    wrap_op_compute_gas!(gas, "GAS", instructions::system::gas);
    wrap_op_compute_gas!(jumpdest, "JUMPDEST", instructions::control::jumpdest);
    wrap_op_compute_gas!(tload, "TLOAD", instructions::host::tload);
    wrap_op_compute_gas!(tstore, "TSTORE", instructions::host::tstore);
    wrap_op_compute_gas!(mcopy, "MCOPY", instructions::memory::mcopy);

    wrap_op_compute_gas!(push0, "PUSH0", instructions::stack::push0);
    wrap_op_compute_gas!(push1, "PUSH1", instructions::stack::push::<1, _, _>);
    wrap_op_compute_gas!(push2, "PUSH2", instructions::stack::push::<2, _, _>);
    wrap_op_compute_gas!(push3, "PUSH3", instructions::stack::push::<3, _, _>);
    wrap_op_compute_gas!(push4, "PUSH4", instructions::stack::push::<4, _, _>);
    wrap_op_compute_gas!(push5, "PUSH5", instructions::stack::push::<5, _, _>);
    wrap_op_compute_gas!(push6, "PUSH6", instructions::stack::push::<6, _, _>);
    wrap_op_compute_gas!(push7, "PUSH7", instructions::stack::push::<7, _, _>);
    wrap_op_compute_gas!(push8, "PUSH8", instructions::stack::push::<8, _, _>);
    wrap_op_compute_gas!(push9, "PUSH9", instructions::stack::push::<9, _, _>);
    wrap_op_compute_gas!(push10, "PUSH10", instructions::stack::push::<10, _, _>);
    wrap_op_compute_gas!(push11, "PUSH11", instructions::stack::push::<11, _, _>);
    wrap_op_compute_gas!(push12, "PUSH12", instructions::stack::push::<12, _, _>);
    wrap_op_compute_gas!(push13, "PUSH13", instructions::stack::push::<13, _, _>);
    wrap_op_compute_gas!(push14, "PUSH14", instructions::stack::push::<14, _, _>);
    wrap_op_compute_gas!(push15, "PUSH15", instructions::stack::push::<15, _, _>);
    wrap_op_compute_gas!(push16, "PUSH16", instructions::stack::push::<16, _, _>);
    wrap_op_compute_gas!(push17, "PUSH17", instructions::stack::push::<17, _, _>);
    wrap_op_compute_gas!(push18, "PUSH18", instructions::stack::push::<18, _, _>);
    wrap_op_compute_gas!(push19, "PUSH19", instructions::stack::push::<19, _, _>);
    wrap_op_compute_gas!(push20, "PUSH20", instructions::stack::push::<20, _, _>);
    wrap_op_compute_gas!(push21, "PUSH21", instructions::stack::push::<21, _, _>);
    wrap_op_compute_gas!(push22, "PUSH22", instructions::stack::push::<22, _, _>);
    wrap_op_compute_gas!(push23, "PUSH23", instructions::stack::push::<23, _, _>);
    wrap_op_compute_gas!(push24, "PUSH24", instructions::stack::push::<24, _, _>);
    wrap_op_compute_gas!(push25, "PUSH25", instructions::stack::push::<25, _, _>);
    wrap_op_compute_gas!(push26, "PUSH26", instructions::stack::push::<26, _, _>);
    wrap_op_compute_gas!(push27, "PUSH27", instructions::stack::push::<27, _, _>);
    wrap_op_compute_gas!(push28, "PUSH28", instructions::stack::push::<28, _, _>);
    wrap_op_compute_gas!(push29, "PUSH29", instructions::stack::push::<29, _, _>);
    wrap_op_compute_gas!(push30, "PUSH30", instructions::stack::push::<30, _, _>);
    wrap_op_compute_gas!(push31, "PUSH31", instructions::stack::push::<31, _, _>);
    wrap_op_compute_gas!(push32, "PUSH32", instructions::stack::push::<32, _, _>);

    wrap_op_compute_gas!(dup1, "DUP1", instructions::stack::dup::<1, _, _>);
    wrap_op_compute_gas!(dup2, "DUP2", instructions::stack::dup::<2, _, _>);
    wrap_op_compute_gas!(dup3, "DUP3", instructions::stack::dup::<3, _, _>);
    wrap_op_compute_gas!(dup4, "DUP4", instructions::stack::dup::<4, _, _>);
    wrap_op_compute_gas!(dup5, "DUP5", instructions::stack::dup::<5, _, _>);
    wrap_op_compute_gas!(dup6, "DUP6", instructions::stack::dup::<6, _, _>);
    wrap_op_compute_gas!(dup7, "DUP7", instructions::stack::dup::<7, _, _>);
    wrap_op_compute_gas!(dup8, "DUP8", instructions::stack::dup::<8, _, _>);
    wrap_op_compute_gas!(dup9, "DUP9", instructions::stack::dup::<9, _, _>);
    wrap_op_compute_gas!(dup10, "DUP10", instructions::stack::dup::<10, _, _>);
    wrap_op_compute_gas!(dup11, "DUP11", instructions::stack::dup::<11, _, _>);
    wrap_op_compute_gas!(dup12, "DUP12", instructions::stack::dup::<12, _, _>);
    wrap_op_compute_gas!(dup13, "DUP13", instructions::stack::dup::<13, _, _>);
    wrap_op_compute_gas!(dup14, "DUP14", instructions::stack::dup::<14, _, _>);
    wrap_op_compute_gas!(dup15, "DUP15", instructions::stack::dup::<15, _, _>);
    wrap_op_compute_gas!(dup16, "DUP16", instructions::stack::dup::<16, _, _>);

    wrap_op_compute_gas!(swap1, "SWAP1", instructions::stack::swap::<1, _, _>);
    wrap_op_compute_gas!(swap2, "SWAP2", instructions::stack::swap::<2, _, _>);
    wrap_op_compute_gas!(swap3, "SWAP3", instructions::stack::swap::<3, _, _>);
    wrap_op_compute_gas!(swap4, "SWAP4", instructions::stack::swap::<4, _, _>);
    wrap_op_compute_gas!(swap5, "SWAP5", instructions::stack::swap::<5, _, _>);
    wrap_op_compute_gas!(swap6, "SWAP6", instructions::stack::swap::<6, _, _>);
    wrap_op_compute_gas!(swap7, "SWAP7", instructions::stack::swap::<7, _, _>);
    wrap_op_compute_gas!(swap8, "SWAP8", instructions::stack::swap::<8, _, _>);
    wrap_op_compute_gas!(swap9, "SWAP9", instructions::stack::swap::<9, _, _>);
    wrap_op_compute_gas!(swap10, "SWAP10", instructions::stack::swap::<10, _, _>);
    wrap_op_compute_gas!(swap11, "SWAP11", instructions::stack::swap::<11, _, _>);
    wrap_op_compute_gas!(swap12, "SWAP12", instructions::stack::swap::<12, _, _>);
    wrap_op_compute_gas!(swap13, "SWAP13", instructions::stack::swap::<13, _, _>);
    wrap_op_compute_gas!(swap14, "SWAP14", instructions::stack::swap::<14, _, _>);
    wrap_op_compute_gas!(swap15, "SWAP15", instructions::stack::swap::<15, _, _>);
    wrap_op_compute_gas!(swap16, "SWAP16", instructions::stack::swap::<16, _, _>);

    wrap_op_compute_gas!(log0, "LOG0", instructions::host::log::<0, _>);
    wrap_op_compute_gas!(log1, "LOG1", instructions::host::log::<1, _>);
    wrap_op_compute_gas!(log2, "LOG2", instructions::host::log::<2, _>);
    wrap_op_compute_gas!(log3, "LOG3", instructions::host::log::<3, _>);
    wrap_op_compute_gas!(log4, "LOG4", instructions::host::log::<4, _>);

    wrap_op_compute_gas!(create, "CREATE", instructions::contract::create::<_, false, _>);
    wrap_op_compute_gas!(call, "CALL", instructions::contract::call);
    wrap_op_compute_gas!(call_code, "CALLCODE", instructions::contract::call_code);
    wrap_op_compute_gas!(ret, "RETURN", instructions::control::ret);
    wrap_op_compute_gas!(delegate_call, "DELEGATECALL", instructions::contract::delegate_call);
    wrap_op_compute_gas!(create2, "CREATE2", instructions::contract::create::<_, true, _>);
    wrap_op_compute_gas!(static_call, "STATICCALL", instructions::contract::static_call);

    wrap_op_compute_gas!(revert, "REVERT", instructions::control::revert);
    wrap_op_compute_gas!(invalid, "INVALID", instructions::control::invalid);
    wrap_op_compute_gas!(selfdestruct, "SELFDESTRUCT", instructions::host::selfdestruct);
}
