use core::cmp::min;

use crate::{
    constants::{self},
    AdditionalLimit, ExternalEnvs, HostExt, MegaContext, MegaSpecId,
};
use alloy_evm::Database;
use alloy_primitives::{keccak256, Address, Bytes, U256};
use revm::{
    context::{ContextTr, JournalTr},
    handler::instructions::{EthInstructions, InstructionProvider},
    interpreter::{
        as_usize_or_fail, gas, gas_or_fail,
        instructions::{self, control, utility::IntoAddress},
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, LoopControl, MemoryTr, RuntimeFlag},
        FrameInput, Instruction, InstructionContext, InstructionResult, InstructionTable,
        InterpreterAction, InterpreterTypes, SStoreResult, Stack,
    },
    primitives::{hash_map::Entry, StorageKey, KECCAK_EMPTY},
    state::{Account, Bytecode, EvmStorageSlot},
    Journal,
};

/// `MegaInstructions` is the instruction table for `MegaETH`.
///
/// This instruction table implements a multi-dimensional gas model and customizes certain opcodes
/// for `MegaETH` specifications:
///
/// # Multi-Dimensional Gas Model
///
/// All instructions track gas usage across multiple dimensions:
/// - **Compute Gas**: Standard EVM operation costs (arithmetic, control flow, memory, etc.)
/// - **Storage Gas**: Dynamic costs for persistent storage operations (SSTORE, CREATE, CALL with
///   transfer)
/// - **Log Storage Gas**: Additional costs for persisting event logs (10x standard costs)
///
/// This separation allows for independent pricing and limiting of different resource types.
///
/// # Customized Opcodes
///
/// ## LOG Opcodes (LOG0-LOG4)
/// - Compute gas: Standard EVM costs (375 + 375×topics + `8×data_bytes`)
/// - Storage gas: 10x multiplier (3,750×topics + `80×data_bytes`)
/// - Data limit enforcement: Halts when total transaction data exceeds 3.125 MB
///
/// ## SELFDESTRUCT Opcode
/// - Completely disabled in Mini-Rex spec
/// - Halts with `InvalidFEOpcode` to prevent permanent contract destruction
///
/// ## SSTORE Opcode
/// - Compute gas: Standard EIP-2200/EIP-2929 costs
/// - Storage gas: Dynamic bucket-based costs only when setting zero → non-zero
/// - Data/KV limit enforcement: Tracks 40 bytes + 1 KV update per storage slot modification
///
/// ## CREATE/CREATE2 Opcodes
/// - Compute gas: Standard costs (32,000 for CREATE, 6 gas/word for CREATE2 hashing)
/// - Storage gas: Dynamic bucket-based costs for new account creation
/// - Gas forwarding: 98/100 rule (2% withheld vs. standard 1.5%)
/// - Data/KV tracking: 40 bytes + 1 KV update per account creation
///
/// ## CALL-like Opcode
/// - Compute gas: Standard call costs
/// - Storage gas: Dynamic bucket-based costs for new account creation (when transferring to empty
///   account)
/// - Gas forwarding: 98/100 rule (2% withheld vs. standard 1.5%)
/// - Oracle detection: Applies gas detention when calling oracle contracts
/// - Data/KV tracking: 40 bytes + 2 KV updates when transferring to empty account
///
/// ## Volatile Data Access Opcodes
/// Block environment opcodes (TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE,
/// BLOCKHASH, BLOBBASEFEE, BLOBHASH) and beneficiary-accessing opcodes (BALANCE, EXTCODESIZE,
/// EXTCODECOPY, EXTCODEHASH) implement immediate gas detention to prevent `DoS` attacks.
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
            MegaSpecId::MINI_REX => EthInstructions::new(mini_rex::instruction_table::<
                EthInterpreter,
                MegaContext<DB, ExtEnvs>,
            >()),
            MegaSpecId::REX => EthInstructions::new(rex::instruction_table::<
                EthInterpreter,
                MegaContext<DB, ExtEnvs>,
            >()),
        };
        Self { spec, inner: instruction_table }
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvs> InstructionProvider for MegaInstructions<DB, ExtEnvs> {
    type Context = MegaContext<DB, ExtEnvs>;
    type InterpreterTypes = EthInterpreter;

    fn instruction_table(&self) -> &InstructionTable<Self::InterpreterTypes, Self::Context> {
        self.inner.instruction_table()
    }
}

mod rex {
    use super::*;

    /// Returns the instruction table for the `REX` spec.
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
    >() -> [Instruction<WIRE, H>; 256]
    where
        WIRE::Stack: StackInspectTr,
    {
        use revm::bytecode::opcode::*;
        let mut table = mini_rex::instruction_table::<WIRE, H>();

        // Mini-Rex mistakenly not modifying these three call-like opcodes. They are fixed in Rex
        table[CALLCODE as usize] = forward_gas_ext::call_code;
        table[DELEGATECALL as usize] = forward_gas_ext::delegate_call;
        table[STATICCALL as usize] = forward_gas_ext::static_call;

        table
    }
}

/// Macro to record compute gas and check if the limit has been exceeded. If the limit is exceeded,
/// the interpreter halts and returns.
macro_rules! compute_gas {
    ($interpreter:expr, $additional_limit:expr, $gas_used:expr $(,$ret:expr)?) => {
        if $additional_limit.record_compute_gas($gas_used).exceeded_limit() {
            $interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
            return $($ret)?;
        }
    };
}

mod mini_rex {
    use super::*;

    /// Returns the instruction table for the `MINI_REX` spec.
    pub(super) const fn instruction_table<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
    >() -> [Instruction<WIRE, H>; 256] {
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
        table[BALANCE as usize] = volatile_data_ext::balance;
        table[ORIGIN as usize] = compute_gas_ext::origin;
        table[CALLER as usize] = compute_gas_ext::caller;
        table[CALLVALUE as usize] = compute_gas_ext::callvalue;
        table[CALLDATALOAD as usize] = compute_gas_ext::calldataload;
        table[CALLDATASIZE as usize] = compute_gas_ext::calldatasize;
        table[CALLDATACOPY as usize] = compute_gas_ext::calldatacopy;
        table[CODESIZE as usize] = compute_gas_ext::codesize;
        table[CODECOPY as usize] = compute_gas_ext::codecopy;

        table[GASPRICE as usize] = compute_gas_ext::gasprice;
        table[EXTCODESIZE as usize] = volatile_data_ext::extcodesize;
        table[EXTCODECOPY as usize] = volatile_data_ext::extcodecopy;
        table[EXTCODEHASH as usize] = volatile_data_ext::extcodehash;
        table[RETURNDATASIZE as usize] = compute_gas_ext::returndatasize;
        table[RETURNDATACOPY as usize] = compute_gas_ext::returndatacopy;
        table[BLOCKHASH as usize] = volatile_data_ext::blockhash;
        table[COINBASE as usize] = volatile_data_ext::coinbase;
        table[TIMESTAMP as usize] = volatile_data_ext::timestamp;
        table[NUMBER as usize] = volatile_data_ext::block_number;
        table[DIFFICULTY as usize] = volatile_data_ext::difficulty;
        table[GASLIMIT as usize] = volatile_data_ext::gas_limit_opcode;
        table[CHAINID as usize] = compute_gas_ext::chainid;
        table[SELFBALANCE as usize] = compute_gas_ext::selfbalance;
        table[BASEFEE as usize] = volatile_data_ext::basefee;
        table[BLOBBASEFEE as usize] = volatile_data_ext::blobbasefee;
        table[BLOBHASH as usize] = volatile_data_ext::blobhash;

        table[POP as usize] = compute_gas_ext::pop;
        table[MLOAD as usize] = compute_gas_ext::mload;
        table[MSTORE as usize] = compute_gas_ext::mstore;
        table[MSTORE8 as usize] = compute_gas_ext::mstore8;
        table[SLOAD as usize] = compute_gas_ext::sload;
        table[SSTORE as usize] = additional_limit_ext::sstore;
        table[JUMP as usize] = compute_gas_ext::jump;
        table[JUMPI as usize] = compute_gas_ext::jumpi;
        table[PC as usize] = compute_gas_ext::pc;
        table[MSIZE as usize] = compute_gas_ext::msize;
        table[GAS as usize] = compute_gas_ext::gas;
        table[JUMPDEST as usize] = compute_gas_ext::jumpdest;
        table[TLOAD as usize] = compute_gas_ext::tload;
        table[TSTORE as usize] = compute_gas_ext::tstore;
        table[MCOPY as usize] = compute_gas_ext::mcopy;

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

        table[LOG0 as usize] = additional_limit_ext::log::<0, _, _>;
        table[LOG1 as usize] = additional_limit_ext::log::<1, _, _>;
        table[LOG2 as usize] = additional_limit_ext::log::<2, _, _>;
        table[LOG3 as usize] = additional_limit_ext::log::<3, _, _>;
        table[LOG4 as usize] = additional_limit_ext::log::<4, _, _>;

        table[CREATE as usize] = forward_gas_ext::create;
        table[CREATE2 as usize] = forward_gas_ext::create2;
        table[CALL as usize] = forward_gas_ext::call;
        table[CALLCODE as usize] = forward_gas_ext::call_code;
        table[DELEGATECALL as usize] = compute_gas_ext::delegate_call;
        table[STATICCALL as usize] = compute_gas_ext::static_call;

        table[INVALID as usize] = compute_gas_ext::invalid;
        table[RETURN as usize] = compute_gas_ext::ret;
        table[REVERT as usize] = compute_gas_ext::revert;
        table[SELFDESTRUCT as usize] = control::invalid;

        table
    }
}

/// Call-like and create-like opcode handlers with 98/100 gas forwarding rule.
///
/// This module provides wrapper implementations for CALL, CALLCODE, DELEGATECALL, STATICCALL,
/// CREATE, and CREATE2 opcodes that enforce the 98/100 gas forwarding rule (retaining 2% of
/// remaining gas in the parent call instead of the standard 1/64).
///
/// The wrappers:
/// 1. Check for value transfer (CALL and CALLCODE only) to account for call stipend
/// 2. Call the underlying opcode implementation
/// 3. Cap the forwarded gas to 98% of the parent's remaining gas
/// 4. Preserve the call stipend (2300 gas) when value is transferred (CALL/CALLCODE only)
/// 5. Support both Call and Create frame types
pub mod forward_gas_ext {
    use super::*;

    /// Macro to wrap call-like and create-like opcodes with 98/100 gas forwarding rule.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Optionally checks for value transfer (`has_transfer`) for CALL opcode
    /// 2. Calls the wrapped opcode handler
    /// 3. Caps the forwarded gas to 98/100 of the remaining gas
    /// 4. Adjusts for call stipend if value is being transferred
    /// 5. Supports both Call and Create frame types
    ///
    /// # Parameters
    /// - `$fn_name`: Name of the generated function
    /// - `$opcode_name`: String name of the opcode (for documentation)
    /// - `$wrapped_fn`: Path to the wrapped instruction implementation
    /// - `$has_transfer_logic`: Expression to determine if value is being transferred (e.g.,
    ///   `has_transfer` or `false`)
    macro_rules! wrap_gas_cap {
        ($fn_name:ident, $opcode_name:expr, $wrapped_fn:path, $has_transfer_logic:expr) => {
            #[doc = concat!("`", $opcode_name, "` opcode with 98/100 gas forwarding rule.")]
            #[inline]
            pub fn $fn_name<
                WIRE: InterpreterTypes<Stack: StackInspectTr>,
                H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
            >(
                mut context: InstructionContext<'_, H, WIRE>,
            ) {
                // Determine if there's a value transfer (only applies to CALL opcode).
                let has_transfer = $has_transfer_logic(&context);

                // Call the wrapped opcode handler.
                let ctx = InstructionContext::<'_, H, WIRE> {
                    interpreter: &mut context.interpreter,
                    host: &mut context.host,
                };
                $wrapped_fn(ctx);

                // Cap the forwarded gas to the child call/create to the 98/100 of the remaining
                // gas.
                match context.interpreter.bytecode.action() {
                    Some(InterpreterAction::NewFrame(FrameInput::Call(call_inputs))) => {
                        // The forwarded gas to the child call should be further restricted to the
                        // 98/100 of the remaining gas. Here, we first recover the total
                        // gas left in the parent call and then cap the child call gas
                        // limit if necessary.

                        // We recover the forwarded gas to the child call from the parent call.
                        let child_gas = call_inputs.gas_limit as u128;
                        // There may be a call stipend if there is value to be transferred.
                        let transfer_gas_stipend =
                            if has_transfer { gas::CALL_STIPEND as u128 } else { 0 };
                        let forwarded_gas = child_gas - transfer_gas_stipend; // Safe from underflow

                        // Recover the remaining gas in the parent call before forwarding to the
                        // child call.
                        let parent_original_gas_left =
                            context.interpreter.gas.remaining() as u128 + forwarded_gas;

                        // Calculate the amount of gas that should be returned to the parent call
                        // under the 98/100 rule.
                        let forwarded_gas_cap = parent_original_gas_left * 98 / 100;
                        let capped_forwarded_gas = min(forwarded_gas, forwarded_gas_cap);
                        let gas_to_return = forwarded_gas - capped_forwarded_gas; // Safe from underflow

                        // Recalculate the child gas
                        let new_child_gas = capped_forwarded_gas + transfer_gas_stipend;

                        //  Return the gas to the parent call.
                        context.interpreter.gas.erase_cost(gas_to_return as u64);

                        // Set the child call gas limit to the capped value.
                        call_inputs.gas_limit = new_child_gas as u64;
                    }
                    Some(InterpreterAction::NewFrame(FrameInput::Create(create_inputs))) => {
                        // The forwarded gas to the child create should be further restricted to the
                        // 98/100 of the remaining gas. CREATE opcodes don't have a call
                        // stipend, so the logic is simpler.

                        // We recover the forwarded gas from the parent call.
                        let child_gas = create_inputs.gas_limit as u128;
                        let forwarded_gas = child_gas; // No stipend for CREATE

                        // Recover the remaining gas in the parent call before forwarding to the
                        // child create.
                        let parent_original_gas_left =
                            context.interpreter.gas.remaining() as u128 + forwarded_gas;

                        // Calculate the amount of gas that should be returned to the parent call
                        // under the 98/100 rule.
                        let forwarded_gas_cap = parent_original_gas_left * 98 / 100;
                        let capped_forwarded_gas = min(forwarded_gas, forwarded_gas_cap);
                        let gas_to_return = forwarded_gas - capped_forwarded_gas; // Safe from underflow

                        //  Return the gas to the parent call.
                        context.interpreter.gas.erase_cost(gas_to_return as u64);

                        // Set the child create gas limit to the capped value.
                        create_inputs.gas_limit = capped_forwarded_gas as u64;
                    }
                    _ => {}
                }
            }
        };
    }

    // Helper function to check if CALL has value transfer
    #[inline]
    fn check_call_has_transfer<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ?Sized,
    >(
        context: &InstructionContext<'_, H, WIRE>,
    ) -> bool {
        if let Some(value) = context.interpreter.stack.inspect::<2>() {
            !value.is_zero()
        } else {
            false
        }
    }

    // Helper function for opcodes without value transfer
    #[inline]
    fn no_transfer<WIRE: InterpreterTypes<Stack: StackInspectTr>, H: HostExt + ?Sized>(
        _context: &InstructionContext<'_, H, WIRE>,
    ) -> bool {
        false
    }

    wrap_gas_cap!(call, "CALL", volatile_data_ext::call, check_call_has_transfer);
    wrap_gas_cap!(call_code, "CALLCODE", volatile_data_ext::call_code, check_call_has_transfer);
    wrap_gas_cap!(delegate_call, "DELEGATECALL", volatile_data_ext::delegate_call, no_transfer);
    wrap_gas_cap!(static_call, "STATICCALL", volatile_data_ext::static_call, no_transfer);
    wrap_gas_cap!(create, "CREATE", storage_gas_ext::create::<WIRE, false, H>, no_transfer);
    wrap_gas_cap!(create2, "CREATE2", storage_gas_ext::create::<WIRE, true, H>, no_transfer);
}

/** Volatile data access opcode handlers with compute gas limit enforcement.

These custom instruction handlers override opcodes that access volatile data (block environment,
beneficiary account data) to lower the compute gas limit. This prevents `DoS` attacks while
allowing storage operations to continue with full transaction gas.

# Compute Gas Limit Enforcement

When volatile data is accessed:
1. The opcode executes normally (calls host method, processes data)
2. If this is the first volatile data access in the transaction:
   - The compute gas limit is lowered based on the type:
     * Block environment or beneficiary: `BLOCK_ENV_ACCESS_REMAINING_GAS` (20M compute gas)
     * Oracle contract: `ORACLE_ACCESS_REMAINING_GAS` (1M compute gas)
3. Most restrictive limit wins: If additional volatile data with different limit is accessed,
   the minimum (most restrictive) limit is applied, regardless of access order
4. All subsequent compute operations are limited by this compute gas limit
5. Storage operations (SSTORE, account creation) continue with full transaction gas

This approach:
- Prevents `DoS` attacks by limiting compute operations after volatile data access
- Allows storage operations to continue normally (not limited by volatile data access)
- Works across nested calls through the compute gas tracking mechanism
- Order-independent: accessing oracle then block env OR block env then oracle both result in
  the same final compute gas limit (the minimum of the two)

# Two Categories of Opcodes

## Block Environment Opcodes (Always Volatile)
These opcodes ALWAYS access volatile data and apply 20M compute gas limit:
- TIMESTAMP, NUMBER, COINBASE, DIFFICULTY, GASLIMIT, BASEFEE, BLOCKHASH, BLOBBASEFEE, BLOBHASH

## Account-Accessing Opcodes (Conditionally Volatile)
These opcodes only SOMETIMES access volatile data (20M compute gas limit when volatile):
- `BALANCE(beneficiary_address)` → volatile, applies 20M compute gas limit
- `BALANCE(other_address)` → not volatile, no limit
- EXTCODESIZE/EXTCODECOPY/EXTCODEHASH → same conditional behavior

For conditional opcodes:
- The Host methods detect when they access beneficiary/volatile accounts
- Compute gas limit lowering only occurs if volatile data is actually accessed
- Regular account accesses don't trigger limit changes
*/
pub mod volatile_data_ext {
    use super::*;
    /// Macro to create opcode handlers that lower compute gas limit on volatile data access.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Calls the original instruction implementation from revm
    /// 2. Lowers the compute gas limit if volatile data was accessed
    ///
    /// The limit enforcement is managed by `VolatileDataAccessTracker` which tracks accessed types
    /// and `AdditionalLimit.set_compute_gas_limit()` which enforces the limit:
    /// - On first volatile data access: lowers the compute gas limit
    /// - On subsequent accesses: may further lower the limit if a more restrictive type is accessed
    macro_rules! wrap_op_detain_gas {
    ($fn_name:ident, $opcode_name:expr, $original_fn:path) => {
        #[doc = concat!("`", $opcode_name, "` opcode with compute gas limit enforcement on volatile data access.")]
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

            // Lower compute gas limit if volatile data was accessed.
            // This is a no-op if no volatile data was accessed or if limit is already lower.
            let compute_gas_limit = volatile_data_tracker.borrow().get_compute_gas_limit();
            if let Some(limit) = compute_gas_limit {
                context.host.additional_limit().borrow_mut().set_compute_gas_limit(limit);
            }
        }
    };
}

    wrap_op_detain_gas!(timestamp, "TIMESTAMP", compute_gas_ext::timestamp);
    wrap_op_detain_gas!(block_number, "NUMBER", compute_gas_ext::number);
    wrap_op_detain_gas!(difficulty, "DIFFICULTY", compute_gas_ext::difficulty);
    wrap_op_detain_gas!(gas_limit_opcode, "GASLIMIT", compute_gas_ext::gaslimit);
    wrap_op_detain_gas!(basefee, "BASEFEE", compute_gas_ext::basefee);
    wrap_op_detain_gas!(coinbase, "COINBASE", compute_gas_ext::coinbase);
    wrap_op_detain_gas!(blockhash, "BLOCKHASH", compute_gas_ext::blockhash);
    wrap_op_detain_gas!(blobbasefee, "BLOBBASEFEE", compute_gas_ext::blobbasefee);
    wrap_op_detain_gas!(blobhash, "BLOBHASH", compute_gas_ext::blobhash);
    wrap_op_detain_gas!(balance, "BALANCE", compute_gas_ext::balance);
    wrap_op_detain_gas!(extcodesize, "EXTCODESIZE", compute_gas_ext::extcodesize);
    wrap_op_detain_gas!(extcodecopy, "EXTCODECOPY", compute_gas_ext::extcodecopy);
    wrap_op_detain_gas!(extcodehash, "EXTCODEHASH", compute_gas_ext::extcodehash);

    /// Macro to create call-like opcode handlers that check for oracle access and apply gas
    /// detention.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Extracts the call target address from the stack (at position N from top)
    /// 2. Calls the original call-like instruction implementation
    /// 3. Checks if calling the oracle contract and applies compute gas limit if needed
    ///
    /// # Oracle Contract Detection
    ///
    /// The generated `CALL`, `CALLCODE`, `DELEGATECALL`, and `STATICCALL` implementations add:
    ///
    /// **Oracle Detection**: Detects oracle contract calls and applies gas detention:
    /// - Detects when calling oracle contracts
    /// - Applies compute gas limit to remaining gas (`ORACLE_ACCESS_REMAINING_GAS` = 1M gas)
    /// - Integrates with global gas detention mechanism (most restrictive limit wins)
    ///
    /// **Compute Gas Limit Enforcement**:
    /// 1. Oracle access is detected and marked via `VolatileDataAccessTracker`
    /// 2. Current interpreter's remaining compute gas is limited
    /// 3. If already detained by a more restrictive limit (e.g., block env access at 20M),
    ///    detention is a no-op
    ///
    /// # Parameters
    /// - `$fn_name`: Name of the generated function
    /// - `$opcode_name`: String name of the opcode (for documentation)
    /// - `$original_fn`: Path to the original instruction implementation
    /// - `$target_stack_pos`: Position of target address on stack (0 = top, 1 = second from top,
    ///   etc.)
    macro_rules! wrap_call_op_oracle_check {
        ($fn_name:ident, $opcode_name:expr, $original_fn:path, $target_stack_pos:expr) => {
            #[doc = concat!("`", $opcode_name, "` opcode with oracle access detection and compute gas limit enforcement.")]
            #[inline]
            pub fn $fn_name<WIRE: InterpreterTypes<Stack: StackInspectTr>, H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized>(
                mut context: InstructionContext<'_, H, WIRE>,
            ) {
                // Get the call target address from the stack.
                let Some(target) = context.interpreter.stack.inspect::<$target_stack_pos>() else {
                    context.interpreter.halt(InstructionResult::StackUnderflow);
                    return;
                };
                let target = target.into_address();

                let ctx = InstructionContext::<'_, H, WIRE> {
                    interpreter: &mut context.interpreter,
                    host: &mut context.host,
                };
                $original_fn(ctx);

                // Check if calling the oracle contract and mark it as accessed.
                // If so, lower the compute gas limit.
                let additional_limit = context.host.additional_limit().clone();
                let mut additional_limit = additional_limit.borrow_mut();
                let mut volatile_data_tracker = context.host.volatile_data_tracker().borrow_mut();
                if volatile_data_tracker.check_and_mark_oracle_access(&target) {
                    if let Some(compute_gas_limit) = volatile_data_tracker.get_compute_gas_limit() {
                        additional_limit.set_compute_gas_limit(compute_gas_limit);
                    }
                }
            }
        };
    }

    wrap_call_op_oracle_check!(call, "CALL", storage_gas_ext::call, 1);
    wrap_call_op_oracle_check!(call_code, "CALLCODE", storage_gas_ext::call_code, 1);
    wrap_call_op_oracle_check!(delegate_call, "DELEGATECALL", compute_gas_ext::delegate_call, 1);
    wrap_call_op_oracle_check!(static_call, "STATICCALL", compute_gas_ext::static_call, 1);
}

/// Extends opcodes with additional limit (kv update limit, data limit, etc.) enforcement.
pub mod additional_limit_ext {
    use super::*;

    /// `SSTORE` opcode implementation with data size and KV update limit enforcement.
    ///
    /// This wrapper adds limit tracking on top of [`storage_gas_ext::sstore`], which handles
    /// compute gas tracking and storage gas costs.
    ///
    /// # Data Size and KV Update Tracking
    ///
    /// When first writing non-zero value to originally-zero slot:
    /// - Adds 40 bytes to transaction data size
    /// - Adds 1 KV update count
    ///
    /// # Limit Enforcement
    ///
    /// Halts with `OutOfGas` when data (3.125 MB) or KV (1,000) limits exceeded.
    ///
    /// # Refund Logic
    ///
    /// Refunds data/KV when slot reset to original value.
    pub fn sstore<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) {
        // Load storage slot values before executing the instruction
        let target_address = context.interpreter.input.target_address();
        let Some(index) = context.interpreter.stack.inspect::<0>() else {
            context.interpreter.halt(InstructionResult::StackUnderflow);
            return;
        };
        let storage_slot = context.host.journal_mut().inspect_storage(target_address, index);
        let (original_value, present_value) = storage_slot
            .map(|slot| (slot.original_value(), slot.present_value()))
            .unwrap_or_default();
        let Some(new_value) = context.interpreter.stack.inspect::<1>() else {
            context.interpreter.halt(InstructionResult::StackUnderflow);
            return;
        };
        let loaded_data = SStoreResult { original_value, present_value, new_value };

        // Execute the original SSTORE instruction
        storage_gas_ext::sstore(InstructionContext {
            interpreter: &mut *context.interpreter,
            host: &mut *context.host,
        });

        // KV update bomb and data bomb (only when first writing non-zero value to originally zero
        // slot): check if the number of key-value updates or the total data size will exceed the
        // limit, if so, halt.
        let additional_limit = context.host.additional_limit();
        let mut additional_limit = additional_limit.borrow_mut();
        if additional_limit.on_sstore(target_address, index, &loaded_data).exceeded_limit() {
            context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
        }
    }

    /// `LOG` opcode implementation with data size limit enforcement.
    ///
    /// This wrapper adds data limit tracking on top of [`storage_gas_ext::log`], which handles
    /// compute gas tracking and storage gas costs.
    ///
    /// # Data Size Limit Enforcement
    ///
    /// After log emission, checks if total transaction data size exceeds `TX_DATA_LIMIT` (3.125
    /// MB). Halts when data limit exceeded.
    pub fn log<
        const N: usize,
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) {
        // Get the log data length before executing the instruction
        let Some(len) = context.interpreter.stack.inspect::<1>() else {
            context.interpreter.halt(InstructionResult::StackUnderflow);
            return;
        };
        let len = as_usize_or_fail!(context.interpreter, len);

        // Execute the original LOG instruction
        storage_gas_ext::log::<N, WIRE, H>(InstructionContext {
            interpreter: &mut *context.interpreter,
            host: &mut *context.host,
        });

        // Record the size of the log topics and data. If the total data size exceeds the limit, we
        // halt.
        let additional_limit = context.host.additional_limit();
        let mut additional_limit = additional_limit.borrow_mut();
        if additional_limit.on_log(N as u64, len as u64).exceeded_limit() {
            context.interpreter.halt(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT);
        }
    }

    // TODO: wrap sstore and log opcode handlers, maybe also call-like and create-like opcode
    // handlers?
}

/// Extends opcodes with storage gas cost on top of `compute_gas_ext`.
pub mod storage_gas_ext {
    use super::*;

    /// Macro to charge storage gas for new account creation before calling the wrapped instruction.
    ///
    /// This macro generates a wrapper function that:
    /// 1. Inspects the target address (stack position 1) and value (stack position 2)
    /// 2. Checks if the target account is empty and value transfer is non-zero
    /// 3. Charges storage gas for new account creation if applicable
    /// 4. Calls the wrapped instruction implementation
    ///
    /// # Call Opcode Behavior
    ///
    /// The generated `CALL` and `CALLCODE` implementations add:
    ///
    /// **Dynamic New Account Gas**: When calling empty account with value transfer:
    /// - Base cost 2,000,000 gas, multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
    ///
    /// # Parameters
    /// - `$fn_name`: Name of the generated function
    /// - `$opcode_name`: String name of the opcode (for documentation)
    /// - `$wrapped_fn`: Path to the wrapped instruction implementation
    macro_rules! wrap_call_with_storage_gas {
        ($fn_name:ident, $opcode_name:expr, $wrapped_fn:path) => {
            #[doc = concat!("`", $opcode_name, "` opcode implementation modified from `revm` with compute gas tracking and dynamically-scaled storage gas costs.")]
            pub fn $fn_name<
                WIRE: InterpreterTypes<Stack: StackInspectTr>,
                H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
            >(
                mut context: InstructionContext<'_, H, WIRE>,
            ) {
                let spec = context.interpreter.runtime_flag.spec_id();
                let Some(to) = context.interpreter.stack.inspect::<1>() else {
                    context.interpreter.halt(InstructionResult::StackUnderflow);
                    return;
                };
                let to = to.into_address();
                let to_account = context.host.journal_mut().inspect_account_delegated(to);
                let is_empty =
                    to_account.map(|account| account.state_clear_aware_is_empty(spec)).unwrap_or(true);
                let Some(value) = context.interpreter.stack.inspect::<2>() else {
                    context.interpreter.halt(InstructionResult::StackUnderflow);
                    return;
                };
                let has_transfer = !value.is_zero();
                // Charge additional storage gas cost for creating a new account
                if is_empty && has_transfer {
                    let Ok(new_account_storage_gas) = context.host.new_account_storage_gas(to) else {
                        context.interpreter.halt(InstructionResult::FatalExternalError);
                        return;
                    };
                    gas!(context.interpreter, new_account_storage_gas);
                }

                // Call the original instruction
                let ctx = InstructionContext::<'_, H, WIRE> {
                    interpreter: &mut context.interpreter,
                    host: &mut context.host,
                };
                $wrapped_fn(ctx);
            }
        };
    }

    wrap_call_with_storage_gas!(call, "CALL", compute_gas_ext::call);
    wrap_call_with_storage_gas!(call_code, "CALLCODE", compute_gas_ext::call_code);

    /// `CREATE`/`CREATE2` opcode implementation modified from `revm` with compute gas tracking and
    /// dynamically-scaled storage gas costs.
    ///
    /// # Differences from the standard EVM
    ///
    /// 1. **Dynamic New Account Gas**: Additional storage gas for new account creation:
    ///    - Base cost 2,000,000 gas, multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
    ///
    /// # Assumptions
    ///
    /// This alternative implementation of `CREATE`/`CREATE2` is only used when the `MINI_REX` spec
    /// is enabled, so we can safely assume that all features before and including `MINI_REX`
    /// are enabled.
    pub fn create<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        const IS_CREATE2: bool,
        H: HostExt + ContextTr + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) {
        // The current execution contract (the caller)
        let creator_address = context.interpreter.input.target_address();
        // Load the creator account from the journal, this must not be cold loaded since the creator
        // must have been warmed when the current frame begins.
        let Ok(creator) = context.host.journal_mut().load_account(creator_address) else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return;
        };

        // Calculate the created address
        let created_address = if IS_CREATE2 {
            let Some(initcode_offset) = context.interpreter.stack.inspect::<1>() else {
                context.interpreter.halt(InstructionResult::StackUnderflow);
                return;
            };
            let Some(initcode_len) = context.interpreter.stack.inspect::<2>() else {
                context.interpreter.halt(InstructionResult::StackUnderflow);
                return;
            };
            let initcode_offset = as_usize_or_fail!(context.interpreter, initcode_offset);
            let initcode_len = as_usize_or_fail!(context.interpreter, initcode_len);
            let code = Bytes::copy_from_slice(
                context.interpreter.memory.slice_len(initcode_offset, initcode_len).as_ref(),
            );
            let initcode_hash = keccak256(&code);

            let Some(salt) = context.interpreter.stack.inspect::<3>() else {
                context.interpreter.halt(InstructionResult::StackUnderflow);
                return;
            };

            creator_address.create2(salt.to_be_bytes(), initcode_hash)
        } else {
            creator_address.create(creator.data.info.nonce)
        };

        // Charge storage gas cost for creating a new account
        let Ok(new_account_storage_gas) = context.host.new_account_storage_gas(created_address)
        else {
            context.interpreter.halt(InstructionResult::FatalExternalError);
            return;
        };
        gas!(context.interpreter, new_account_storage_gas);

        // Call the original create instruction
        if IS_CREATE2 {
            compute_gas_ext::create2(context);
        } else {
            compute_gas_ext::create(context);
        }
    }

    /// `LOG` opcode implementation modified from `revm` with compute gas tracking, increased
    /// storage gas costs, and data size limit enforcement.
    ///
    /// # Differences from the standard EVM
    ///
    /// 1. **Storage Gas Costs**: Additional storage gas charged for log storage:
    ///    - Topic storage: 3,750 gas per topic (10x standard topic cost)
    ///    - Data storage: 80 gas per byte (10x standard data cost)
    ///
    /// # Assumptions
    ///
    /// This alternative implementation of `LOG` is only used when the `MINI_REX` spec is enabled.
    pub fn log<
        const N: usize,
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) {
        let Some(len) = context.interpreter.stack.inspect::<1>() else {
            context.interpreter.halt(InstructionResult::StackUnderflow);
            return;
        };
        let len = as_usize_or_fail!(context.interpreter, len);

        // Charge storage gas cost for log topics and data before instruction execution.
        let log_storage_cost = {
            let topic_cost = constants::mini_rex::LOG_TOPIC_STORAGE_GAS.checked_mul(N as u64);
            let data_cost = constants::mini_rex::LOG_DATA_STORAGE_GAS.checked_mul(len as u64);
            topic_cost.and_then(|topic| data_cost.and_then(|cost| cost.checked_add(topic)))
        };
        gas_or_fail!(context.interpreter, log_storage_cost);

        // Execute the original LOG instruction.
        match N {
            0 => {
                compute_gas_ext::log0(context);
            }
            1 => {
                compute_gas_ext::log1(context);
            }
            2 => {
                compute_gas_ext::log2(context);
            }
            3 => {
                compute_gas_ext::log3(context);
            }
            4 => {
                compute_gas_ext::log4(context);
            }
            _ => {
                context.interpreter.halt(InstructionResult::InvalidFEOpcode);
            }
        }
    }

    /// `SSTORE` opcode implementation modified from `revm` with compute gas tracking and
    /// dynamically-scaled storage gas costs.
    ///
    /// # Differences from the standard EVM
    ///
    /// 1. **Dynamic Storage Gas**: Additional storage gas ONLY when setting originally-zero slot to
    ///    non-zero:
    ///    - Base cost 2,000,000 gas, multiplied by `bucket_capacity / MIN_BUCKET_SIZE`
    ///    - Not charged for updating already-non-zero slots or resetting to zero
    ///
    /// # Assumptions
    ///
    /// This alternative implementation of `SSTORE` is only used when the `MINI_REX` spec is
    /// enabled, so we can safely assume that all features before and including Mini-Rex are
    /// enabled.
    pub fn sstore<
        WIRE: InterpreterTypes<Stack: StackInspectTr>,
        H: HostExt + ContextTr<Journal: JournalInspectTr> + ?Sized,
    >(
        context: InstructionContext<'_, H, WIRE>,
    ) {
        // The address to the underlying execution contract state
        let target_address = context.interpreter.input.target_address();
        // The storage slot to write
        let Some(index) = context.interpreter.stack.inspect::<0>() else {
            context.interpreter.halt(InstructionResult::StackUnderflow);
            return;
        };
        // The storage slot values
        let storage_slot = context.host.journal_mut().inspect_storage(target_address, index);
        let (original_value, present_value) = storage_slot
            .map(|slot| (slot.original_value(), slot.present_value()))
            .unwrap_or_default();
        let Some(new_value) = context.interpreter.stack.inspect::<1>() else {
            context.interpreter.halt(InstructionResult::StackUnderflow);
            return;
        };

        // Charge storage gas cost before the instruction is executed
        if original_value.is_zero() && present_value.is_zero() && !new_value.is_zero() {
            let Ok(sstore_set_storage_gas) =
                context.host.sstore_set_storage_gas(target_address, index)
            else {
                context.interpreter.halt(InstructionResult::FatalExternalError);
                return;
            };
            gas!(context.interpreter, sstore_set_storage_gas);
        }

        // Execute the original SSTORE instruction
        compute_gas_ext::sstore(context);
    }
}

/// Compute gas recording implementation. TODO: add more doc
pub mod compute_gas_ext {
    use super::*;

    /// Macro to wrap the original instruction implementation with compute gas tracking.
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

                let mut gas_used = gas_before.saturating_sub(context.interpreter.gas.remaining());
                // Some of the gas may be forwarded to the child call. We need to substract them.
                match context.interpreter.bytecode.action() {
                    Some(InterpreterAction::NewFrame(FrameInput::Call(call_inputs))) => {
                        gas_used = gas_used.saturating_sub(call_inputs.gas_limit);
                    }
                    Some(InterpreterAction::NewFrame(FrameInput::Create(create_inputs))) => {
                        gas_used = gas_used.saturating_sub(create_inputs.gas_limit);
                    }
                    _ => {}
                }
                let mut additional_limit = context.host.additional_limit().borrow_mut();
                compute_gas!(context.interpreter, additional_limit, gas_used);
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

/// Trait to inspect the stack elements.
pub trait StackInspectTr {
    /// Inspect the N-th element of the stack. The top of the stack is the 0-th element.
    /// If the stack is too short, return None.
    fn inspect<const N: usize>(&self) -> Option<U256>;
}

impl StackInspectTr for Stack {
    fn inspect<const N: usize>(&self) -> Option<U256> {
        if N >= self.len() {
            return None;
        }
        let index = self.len() - 1 - N;
        // SAFETY: the index must be within the bounds of the stack
        Some(unsafe { *self.data().get_unchecked(index) })
    }
}

/// Trait to inspect the journal's internal state without marking any accounts or storage slots as
/// warm. To improve performance, when journal does not have the account or storage slot, it will be
/// loaded from the database and cached in the journal. However, since we explicitly mark the
/// account or storage slot as cold, this pre-loading before executing the original instruction will
/// make no difference on gas cost.
pub trait JournalInspectTr {
    /// The database error type.
    type DBError;

    /// Inspect the account at the given address without marking it as warm. If the account is
    /// EIP-7702 type, it should inspect the delegated account.
    fn inspect_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<&mut Account, Self::DBError>;

    /// Inspect the storage at the given address and key without marking it as warm.
    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError>;
}

impl<DB: revm::Database> JournalInspectTr for Journal<DB> {
    type DBError = <DB as revm::Database>::Error;

    fn inspect_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<&mut Account, Self::DBError> {
        let transaction_id = self.transaction_id;
        let account = match self.inner.state.entry(address) {
            Entry::Occupied(entry) => {
                let account = entry.into_mut();
                if account.info.code_hash != KECCAK_EMPTY && account.info.code.is_none() {
                    // Load code if not loaded before
                    account.info.code = Some(self.database.code_by_hash(account.info.code_hash)?);
                }
                account
            }
            Entry::Vacant(entry) => {
                let mut account = self
                    .database
                    .basic(address)?
                    .map(|info| info.into())
                    .unwrap_or_else(|| Account::new_not_existing(transaction_id));
                // delibrately mark the account as cold since we are only inpecting it, not warming
                // it.
                account.mark_cold();
                entry.insert(account)
            }
        };

        // If the account is a delegated account, inspect the delegated account.
        if let Some(Bytecode::Eip7702(code)) = &account.info.code {
            let address = code.address();
            return self.inspect_account_delegated(address);
        }

        // Load account again to bypass the borrow checker.
        let account = self.inner.state.get_mut(&address).unwrap();
        Ok(account)
    }

    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError> {
        let transaction_id = self.transaction_id;
        let account = self.inspect_account_delegated(address)?;
        if account.storage.contains_key(&key) {
            // Slot already exists, return reference to it
            // Need to reload account to satisfy borrow checker
            let account = self.inspect_account_delegated(address)?;
            return Ok(account.storage.get(&key).unwrap());
        }
        // Slot doesn't exist, load from DB and insert
        let slot = self.database.storage(address, key)?;
        let mut slot = EvmStorageSlot::new(slot, transaction_id);
        // delibrately mark the slot as cold since we are only inpecting it, not warming it
        slot.mark_cold();
        // Load account again to bypass the borrow checker and insert the slot
        let account = self.inspect_account_delegated(address)?;
        account.storage.insert(key, slot);
        // Return reference to the newly inserted slot
        Ok(account.storage.get(&key).expect("slot should exist"))
    }
}
