//! The instruction table of the Satin engine.
//!
//! Every opcode runs revm's own instruction, except the ones that can read volatile data.
//!
//! The opcodes that write state the resource limits count are revm's own, and are metered inside
//! revm's instructions, through two Host hooks (see the `host` module). Three write it themselves —
//! `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT` — and revm calls the completed-write hook at the end
//! of each, once the write is in the journal and every charge the opcode makes has been made: the
//! Host counts the write, holds it to the limits and charges the history it costs there, and a
//! failed opcode never reaches the hook, so it counts nothing. Four write it by starting a frame —
//! `CALL`, `CALLCODE`, `CREATE` and `CREATE2` — and revm calls the frame-start hook at every call
//! and creation, once the opcode took the gas it forwards and before it builds the frame's input:
//! the Host charges the caller the history of the write records the start makes there, so the
//! frame inherits the reservoir the charge left, and a caller that cannot pay starts no frame.
//!
//! Every opcode that can read volatile data — the block-environment opcodes, the account opcodes,
//! `SLOAD`, the four calls and `SELFDESTRUCT` ([`READ_OPCODES`]) — runs in a wrapper that decides
//! whether the read is refused and settles the reads its Host calls made (see the `access`
//! module):
//!
//! 1. in a frame whose volatile-data access is off, refuse the read before the opcode charges or
//!    checks anything, when what it would read is volatile: the frame reverts with
//!    `VolatileDataAccessDisabled` on the gas it held when it reached the opcode;
//! 2. charge the opcode's static gas, as its own charge;
//! 3. run revm's instruction as a read, whose Host calls mark the reads they load;
//! 4. once the opcode completed, commit the reads, which holds the frame's spendable gas to what
//!    the compute limit leaves it.
//!
//! The opcode's static gas is the wrapper's to charge, after the refusal is decided, so the
//! instruction table carries none for these opcodes: the interpreter charges an opcode's static
//! gas before it runs the instruction, which would make a frame short of it run out of gas
//! rather than be refused, and would charge a refused read. A frame whose access is on runs every
//! wrapped opcode on the same charges, in the same order, as revm's own: its static gas, then the
//! instruction. The static gas the wrappers charge is Satin's ([`satin_gas_table`]), or the
//! fixture fork's under the test tooling's neutral configuration.
//!
//! An opcode volatile whatever its operands — the block-environment opcodes, `BLOCKHASH` among
//! them — is refused before its operands are read. One volatile for the account or the storage it
//! reads is refused only once every operand it takes is on the stack, so one that fails on its
//! operands fails as it would with access on; it is then refused when its target operand, its
//! frame's own address, or a call target's one-hop EIP-7702 delegate is the block beneficiary, and
//! an `SLOAD` when its frame's storage is the Oracle's. The delegate is read for the decision: the
//! call target's account and code are loaded, so the read is in the transaction's state, and left
//! as warm or cold as they were.
//!
//! The Oracle's `SLOAD` is the one read made before any of its gas is charged: the slot is loaded,
//! the oracle service asked and the read marked, at the compute before the opcode, and only then
//! is the cold access charged, its warm base included, as one charge. A frame that cannot pay it
//! runs out of gas, and gas detention's crossing stops it, with the read marked all the same.
//! Every other `SLOAD` is revm's: its static gas, then its cold access, each its own charge.
//!
//! The Host observes a load only while a wrapper runs its opcode. `CREATE` and `CREATE2` are not
//! wrapped: they load their creator's account and the created address's through the Host, to
//! check the creator's balance and nonce and to price the created account, and those loads are no
//! volatile reads, neither refused nor marked, whichever of the two accounts is the block
//! beneficiary's: a value call's check of its caller's balance is no read either.
//!
//! An opcode whose charge fails with an out-of-gas halts its frame, and the halt zeroes the gas
//! the frame had left before the frame returns. The revm fork records that gas first, before
//! every deduction that fails — an opcode's static gas included — and before a halt on a cold
//! access the frame cannot pay; gas detention reads the record where the frame ends, and does not
//! count what a halt burns as compute. The one hook that writes the record itself is the
//! frame-start hook, when the caller cannot pay a start: it hands the forward back to the caller
//! first.
//!
//! The static gas table is Amsterdam's ([`satin_gas_table`]).
//!
//! The table also carries the Amsterdam opcodes (`DUPN`, `SWAPN`, `EXCHANGE`, `SLOTNUM`), which
//! the base spec gates off: Satin takes the Amsterdam schedule, so it takes the opcodes with it.
//! They are installed before the wrappers go in, because installing them after would replace a
//! wrapped entry with an unwrapped one. `CLZ` needs no installation — its own gate is Osaka, so
//! the base spec already has it.

use revm::{
    bytecode::opcode::{
        BALANCE, BASEFEE, BLOBBASEFEE, BLOCKHASH, CALL, CALLCODE, COINBASE, DELEGATECALL,
        DIFFICULTY, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, GASLIMIT, NUMBER, SELFBALANCE,
        SELFDESTRUCT, SLOAD, SLOTNUM, STATICCALL, TIMESTAMP,
    },
    context_interface::Host,
    handler::instructions::EthInstructions,
    interpreter::{
        enable_amsterdam_opcodes, instruction_table,
        instructions::{block_info, contract, host, utility::IntoAddress, GasTable},
        interpreter::EthInterpreter,
        interpreter_types::LoopControl,
        FrameInput, Gas, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
        Interpreter, InterpreterAction,
    },
    primitives::hardfork::SpecId,
    Database,
};

use crate::{
    system::ORACLE_CONTRACT_ADDRESS, volatile_data_access_disabled_revert_data, ExternalEnvTypes,
    MegaContext, VolatileDataAccess,
};

use super::{input_gas_limit, satin_gas_table, value_call_stipend, MegaInstructions};

/// The context an instruction of the Satin engine runs with.
type Ctx<'a, DB, ExtEnvs> = InstructionContext<'a, MegaContext<DB, ExtEnvs>, EthInterpreter>;

/// An instruction of the Satin engine.
type InstructionFn<DB, ExtEnvs> = fn(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult;

/// The opcodes that can read volatile data, which run in a read wrapper that charges their static
/// gas itself.
pub(crate) const READ_OPCODES: [u8; 20] = [
    SELFDESTRUCT,
    CALL,
    CALLCODE,
    DELEGATECALL,
    STATICCALL,
    COINBASE,
    TIMESTAMP,
    NUMBER,
    DIFFICULTY,
    GASLIMIT,
    BASEFEE,
    BLOBBASEFEE,
    SLOTNUM,
    BLOCKHASH,
    BALANCE,
    SELFBALANCE,
    EXTCODESIZE,
    EXTCODECOPY,
    EXTCODEHASH,
    SLOAD,
];

/// The Satin static opcode prices, which the read wrappers charge for their opcodes.
pub(crate) const SATIN_GAS_TABLE: GasTable = satin_gas_table();

/// The Satin instruction table: revm's for the base spec, with the Amsterdam opcodes activated and
/// the opcodes that can read volatile data wrapped, on the Satin static gas table, less the static
/// gas of the wrapped opcodes, which their wrappers charge.
pub(crate) fn mega_instructions<DB: Database, ExtEnvs: ExternalEnvTypes>(
    spec: SpecId,
) -> MegaInstructions<DB, ExtEnvs> {
    let mut table = instruction_table();
    enable_amsterdam_opcodes(&mut table);
    let mut instructions = EthInstructions::new(table, satin_gas_table(), spec);
    let wrappers: [(u8, InstructionFn<DB, ExtEnvs>); 20] = [
        (SELFDESTRUCT, selfdestruct::<DB, ExtEnvs>),
        (CALL, call::<CALL, DB, ExtEnvs>),
        (CALLCODE, call::<CALLCODE, DB, ExtEnvs>),
        (DELEGATECALL, call::<DELEGATECALL, DB, ExtEnvs>),
        (STATICCALL, call::<STATICCALL, DB, ExtEnvs>),
        (COINBASE, coinbase::<DB, ExtEnvs>),
        (TIMESTAMP, timestamp::<DB, ExtEnvs>),
        (NUMBER, number::<DB, ExtEnvs>),
        (DIFFICULTY, difficulty::<DB, ExtEnvs>),
        (GASLIMIT, gaslimit::<DB, ExtEnvs>),
        (BASEFEE, basefee::<DB, ExtEnvs>),
        (BLOBBASEFEE, blobbasefee::<DB, ExtEnvs>),
        (SLOTNUM, slotnum::<DB, ExtEnvs>),
        (BLOCKHASH, blockhash::<DB, ExtEnvs>),
        (BALANCE, balance::<DB, ExtEnvs>),
        (SELFBALANCE, selfbalance::<DB, ExtEnvs>),
        (EXTCODESIZE, extcodesize::<DB, ExtEnvs>),
        (EXTCODECOPY, extcodecopy::<DB, ExtEnvs>),
        (EXTCODEHASH, extcodehash::<DB, ExtEnvs>),
        (SLOAD, sload::<DB, ExtEnvs>),
    ];
    debug_assert!(wrappers.iter().map(|&(opcode, _)| opcode).eq(READ_OPCODES));
    for (opcode, wrapper) in wrappers {
        instructions.insert_instruction(opcode, Instruction::new(wrapper), 0);
    }
    instructions
}

/// `table` as the interpreter charges it with the read wrappers installed: the static gas of every
/// opcode the wrappers run is theirs to charge, so the interpreter's entry is zero.
pub(crate) fn without_read_static_gas(mut table: GasTable) -> GasTable {
    for opcode in READ_OPCODES {
        table[opcode as usize] = 0;
    }
    table
}

/// `CALL`, `CALLCODE`, `DELEGATECALL` and `STATICCALL`, committing the read of the block
/// beneficiary's account when the callee, or the EIP-7702 delegate it runs, is the beneficiary.
fn call<const KIND: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    read_volatile::<KIND, _, _>(context, contract::call::<KIND, _, _>)
}

/// Runs the opcode `OPCODE`, which can read volatile data, as `inner`, and settles the reads of
/// volatile data its Host calls made.
///
/// In a frame whose volatile-data access is off, the read is decided first
/// ([`refused_read`]): a refused read reverts the frame with `VolatileDataAccessDisabled` on the
/// gas it held when it reached the opcode, before the opcode charges or checks anything. Otherwise
/// the opcode's static gas is charged, as its own charge, as the interpreter charges every other
/// opcode's, and `inner` runs; a read the opcode completed is committed, capping the frame at what
/// the compute limit leaves it.
///
/// A read the opcode did not complete — the opcode failed after the load — is dropped: it reached
/// no computation, and the frame is failing anyway.
#[inline(always)]
fn read_volatile<const OPCODE: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
    inner: impl FnOnce(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    let gas = if host.detention.is_refusing() {
        if let Some(refused) = refused_read::<OPCODE, _, _>(interpreter, host)? {
            return Err(refuse(interpreter, refused));
        }
        Some(interpreter.gas)
    } else {
        None
    };
    charge_static_gas(interpreter, host.read_static_gas(OPCODE))?;
    host.detention.begin_read();
    let result = inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
    host.detention.end_read();
    if !host.detention.has_reads() {
        return result;
    }
    settle_reads(interpreter, host, result, gas)
}

/// Charges an opcode's static gas `gas` as the interpreter charges it before every other
/// instruction: an out-of-gas the frame halts on, whose failure the fork records.
#[inline(always)]
fn charge_static_gas(
    interpreter: &mut Interpreter<EthInterpreter>,
    gas: u64,
) -> InstructionExecResult {
    if interpreter.gas.record_cost_unsafe(gas) {
        return Err(InstructionResult::OutOfGas);
    }
    Ok(())
}

/// What the opcode `OPCODE` would read that the running frame, whose volatile-data access is off,
/// is refused, or `None` when it reads nothing volatile; decided from the opcode, its operands and
/// its frame, before the opcode charges or checks anything.
///
/// - The block-environment opcodes, `BLOCKHASH` included, are volatile whatever their operands:
///   they are refused before their operands are read.
/// - `BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, the four calls and `SELFDESTRUCT` are
///   volatile only for the account they read: with fewer operands on the stack than the opcode
///   takes, nothing is refused, and the opcode fails on them as it would with access on. They are
///   refused when their target operand is the block beneficiary, `SELFDESTRUCT` also when its frame
///   runs as the beneficiary, and a call also when its target's one-hop EIP-7702 delegate is the
///   beneficiary. `SELFBALANCE` is refused when its frame runs as the beneficiary.
///
/// The delegate is read for the decision ([`call_target_delegate`]), and the read is required: a
/// database error fails the transaction with its cause, whatever the call would have done.
///
/// [`call_target_delegate`]: MegaContext::call_target_delegate
#[cold]
#[inline(never)]
fn refused_read<const OPCODE: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &Interpreter<EthInterpreter>,
    host: &mut MegaContext<DB, ExtEnvs>,
) -> Result<Option<VolatileDataAccess>, InstructionResult> {
    let block_env = match OPCODE {
        NUMBER => Some(VolatileDataAccess::BLOCK_NUMBER),
        TIMESTAMP => Some(VolatileDataAccess::TIMESTAMP),
        COINBASE => Some(VolatileDataAccess::COINBASE),
        DIFFICULTY => Some(VolatileDataAccess::PREV_RANDAO),
        GASLIMIT => Some(VolatileDataAccess::GAS_LIMIT),
        BASEFEE => Some(VolatileDataAccess::BASE_FEE),
        BLOBBASEFEE => Some(VolatileDataAccess::BLOB_BASE_FEE),
        SLOTNUM => Some(VolatileDataAccess::SLOT_NUM),
        BLOCKHASH => Some(VolatileDataAccess::BLOCK_HASH),
        _ => None,
    };
    if block_env.is_some() {
        return Ok(block_env);
    }
    let beneficiary = host.inner.block.beneficiary;
    let stack = &interpreter.stack;
    // The `n`th operand from the top, as the address the opcode reads it as.
    let address = |n: usize| stack.peek(n).ok().map(IntoAddress::into_address);
    let own = interpreter.input.target_address;
    let reads_beneficiary = match OPCODE {
        SELFBALANCE => own == beneficiary,
        BALANCE | EXTCODESIZE | EXTCODEHASH => address(0) == Some(beneficiary),
        EXTCODECOPY => stack.len() >= 4 && address(0) == Some(beneficiary),
        SELFDESTRUCT => {
            address(0).is_some_and(|target| target == beneficiary || own == beneficiary)
        }
        CALL | CALLCODE | DELEGATECALL | STATICCALL => {
            let operands = if matches!(OPCODE, CALL | CALLCODE) { 7 } else { 6 };
            match address(1) {
                Some(target) if stack.len() >= operands => {
                    target == beneficiary || host.call_target_delegate(target)? == Some(beneficiary)
                }
                _ => false,
            }
        }
        _ => false,
    };
    Ok(reads_beneficiary.then_some(VolatileDataAccess::BENEFICIARY_BALANCE))
}

/// The slow half of [`read_volatile`]: the Host observed a read.
///
/// The Host refuses a load only where the wrapper did not refuse the read first, which no opcode
/// reaches: every volatile load an opcode makes is one [`refused_read`] decides. Were one reached,
/// the frame would be refused on the gas it held when it reached the opcode (`gas`).
#[inline(never)]
fn settle_reads<DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &mut MegaContext<DB, ExtEnvs>,
    result: InstructionExecResult,
    gas: Option<Gas>,
) -> InstructionExecResult {
    let observed = host.detention.take_observed();
    let refused = host.detention.take_refused();
    debug_assert!(refused.is_none(), "a read the wrapper did not refuse first: {refused:?}");
    if let Some(refused) = refused {
        // The Host refuses a load before the opcode builds a frame, so no action is pending.
        if let Some(gas) = gas {
            interpreter.gas = gas;
        }
        return Err(refuse(interpreter, refused));
    }
    let completed = match result {
        Ok(()) => true,
        Err(result) => {
            result.is_ok() ||
                matches!(interpreter.bytecode.action(), Some(InterpreterAction::NewFrame(_)))
        }
    };
    if !completed {
        return result;
    }
    // A completed opcode leaves no result pending: a `SELFDESTRUCT`'s is built from the frame's
    // gas once the wrapper returned, and a stop fails the opcode.
    let forwarded = match interpreter.bytecode.action() {
        Some(InterpreterAction::NewFrame(input)) => {
            forwarded_gas(input, host.gas_params().call_stipend())
        }
        _ => 0,
    };
    host.detention.commit_reads(observed, &mut interpreter.gas, forwarded);
    result
}

/// Reverts the running frame with `VolatileDataAccessDisabled` for the read `refused`, on the gas
/// it holds.
#[cold]
#[inline(never)]
fn refuse(
    interpreter: &mut Interpreter<EthInterpreter>,
    refused: VolatileDataAccess,
) -> InstructionResult {
    let result = InstructionResult::Revert;
    interpreter.bytecode.set_action(InterpreterAction::new_return(
        result,
        volatile_data_access_disabled_revert_data(refused),
        interpreter.gas,
    ));
    result
}

/// The regular gas the running frame paid for the frame `input` starts: the frame's limit, less
/// a value call's stipend, which nobody paid.
fn forwarded_gas(input: &FrameInput, call_stipend: u64) -> u64 {
    input_gas_limit(input).saturating_sub(value_call_stipend(input, call_stipend))
}

/// Defines an opcode that reads volatile data — a block-environment field, an account or a slot —
/// as revm's instruction in [`read_volatile`].
macro_rules! volatile_read {
    ($($name:ident => $inner:path, $opcode:ident, $what:literal;)*) => {$(
        #[doc = concat!("`", stringify!($opcode), "`, committing the read of ", $what, ".")]
        fn $name<DB: Database, ExtEnvs: ExternalEnvTypes>(
            context: Ctx<'_, DB, ExtEnvs>,
        ) -> InstructionExecResult {
            read_volatile::<$opcode, _, _>(context, $inner)
        }
    )*};
}

volatile_read! {
    coinbase => block_info::coinbase, COINBASE, "the field";
    timestamp => block_info::timestamp, TIMESTAMP, "the field";
    number => block_info::block_number, NUMBER, "the field";
    difficulty => block_info::difficulty, DIFFICULTY, "the field";
    gaslimit => block_info::gaslimit, GASLIMIT, "the field";
    basefee => block_info::basefee, BASEFEE, "the field";
    blobbasefee => block_info::blob_basefee, BLOBBASEFEE, "the field";
    slotnum => block_info::slot_num_enabled, SLOTNUM, "the field";
    blockhash => host::blockhash, BLOCKHASH, "the block number and of the hash";
    balance => host::balance, BALANCE, "the block beneficiary's account";
    selfbalance => host::selfbalance, SELFBALANCE, "the block beneficiary's account";
    extcodesize => host::extcodesize, EXTCODESIZE, "the block beneficiary's account";
    extcodecopy => host::extcodecopy, EXTCODECOPY, "the block beneficiary's account";
    extcodehash => host::extcodehash, EXTCODEHASH, "the block beneficiary's account";
    selfdestruct => host::selfdestruct, SELFDESTRUCT, "the block beneficiary's account";
}

/// `SLOAD`: revm's for every storage but the Oracle's, its static gas charged first as its own
/// charge; the Oracle's own frame reads through [`oracle_sload`].
fn sload<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    if context.interpreter.input.target_address == ORACLE_CONTRACT_ADDRESS {
        return oracle_sload(context);
    }
    let InstructionContext { interpreter, host } = context;
    charge_static_gas(interpreter, host.read_static_gas(SLOAD))?;
    host::sload(InstructionContext { interpreter, host })
}

/// `SLOAD` in the Oracle's own frame — a storage read whose owner is the Oracle — which reads
/// before it charges anything.
///
/// The slot operand must be on the stack, or the opcode fails on it as it would with access on.
/// In a frame whose volatile-data access is off the read is then refused, on the gas the frame
/// holds, asking nothing. Otherwise the slot is read
/// ([`read_oracle_slot`](MegaContext::read_oracle_slot)): loaded, the oracle service asked where
/// the execution asks it, and the read marked; and the read is committed at once, at the
/// transaction's compute before the opcode, so the mark stands whatever the charge then does.
/// Only then is the cold access charged, `COLD_SLOAD_COST` with its warm base, as one charge:
/// a frame whose regular gas cannot pay it runs out of gas at the `SLOAD`, and one whose
/// spendable part cannot pay it while the whole can is gas detention's crossing, judged on the
/// whole charge and stopped with none of it made.
#[inline(never)]
fn oracle_sload<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    let Some(key) = interpreter.stack.top().map(|top| *top) else {
        return Err(InstructionResult::StackUnderflow);
    };
    if host.detention.is_refusing() {
        return Err(refuse(interpreter, VolatileDataAccess::ORACLE));
    }
    host.detention.begin_read();
    let read = host.read_oracle_slot(key);
    host.detention.end_read();
    let value = read?;
    let observed = host.detention.take_observed();
    if !observed.is_empty() {
        host.detention.commit_reads(observed, &mut interpreter.gas, 0);
    }
    let cold_access =
        host.read_static_gas(SLOAD) + host.gas_params().cold_storage_additional_cost();
    if !interpreter.gas.record_regular_cost(cold_access) {
        return Err(InstructionResult::OutOfGas);
    }
    if let Some(top) = interpreter.stack.top() {
        *top = value;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
    use revm::bytecode::opcode::{CLZ, DUPN, EXCHANGE, SLOTNUM, SWAPN};

    /// The table charges the Satin static gas of every opcode but the ones the read wrappers run,
    /// which charge it themselves: the whole table, with their entries, is revm's Amsterdam one.
    ///
    /// Activating the Amsterdam opcodes does not touch it either — their static gas is the same
    /// on every spec, which is why the activation is an instruction-table change alone.
    #[test]
    fn test_the_table_and_the_wrappers_charge_the_static_gas_table() {
        let mega = mega_instructions::<MemoryDatabase, EmptyExternalEnv>(SpecId::OSAKA);
        assert_eq!(mega.gas_table(), &without_read_static_gas(satin_gas_table()));
        let mut charged = *mega.gas_table();
        for opcode in READ_OPCODES {
            assert_eq!(charged[opcode as usize], 0, "opcode {opcode:#04x}: the wrapper's");
            charged[opcode as usize] = SATIN_GAS_TABLE[opcode as usize];
        }
        assert_eq!(charged, satin_gas_table());
        assert_eq!(mega.spec, SpecId::OSAKA);
        for (opcode, gas) in [(DUPN, 3), (SWAPN, 3), (EXCHANGE, 3), (SLOTNUM, 2), (CLZ, 5)] {
            assert_eq!(charged[opcode as usize], gas, "opcode {opcode:#04x}");
        }
    }
}
