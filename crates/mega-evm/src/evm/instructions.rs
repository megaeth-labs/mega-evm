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
//! `SLOAD`, the four calls and `SELFDESTRUCT` — runs in a wrapper that settles the reads its Host
//! calls made (see the `access` module):
//!
//! 1. keep the frame's gas, when the frame's reads are refused;
//! 2. run revm's instruction as a read, whose Host calls observe or refuse the reads;
//! 3. on a refusal, hand the frame back the gas it had before the opcode and revert it with
//!    `VolatileDataAccessDisabled`; otherwise, once the opcode completed, commit the reads, which
//!    holds the frame's spendable gas to what the compute limit leaves it.
//!
//! The Host observes or refuses a load only while a wrapper runs its opcode. `CREATE` and
//! `CREATE2` are not wrapped: they load their creator's account and the created address's through
//! the Host, to check the creator's balance and nonce and to price the created account, and those
//! loads are no volatile reads, neither refused nor marked, whichever of the two accounts is the
//! block beneficiary's: a value call's check of its caller's balance is no read either.
//!
//! An opcode whose charge fails with an out-of-gas halts its frame, and the halt zeroes the gas
//! the frame had left before the frame returns. The revm fork records that gas first, before
//! every deduction that fails — an opcode's static gas, which the interpreter's step loop charges
//! before the opcode runs, included — and before a halt on a cold access the frame cannot pay;
//! gas detention reads the record where the frame ends, and does not count what a halt burns as
//! compute. The one hook that writes the record itself is the frame-start hook, when the caller
//! cannot pay a start: it hands the forward back to the caller first.
//!
//! A wrapper keeps the static gas the table charges for its opcode, so the gas schedule is
//! unchanged. The static gas table is Amsterdam's ([`satin_gas_table`]).
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
        instructions::{block_info, contract, host},
        interpreter::EthInterpreter,
        interpreter_types::LoopControl,
        FrameInput, Gas, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
        Interpreter, InterpreterAction,
    },
    primitives::hardfork::SpecId,
    Database,
};

use crate::{
    volatile_data_access_disabled_revert_data, ExternalEnvTypes, MegaContext, VolatileDataAccess,
};

use super::{input_gas_limit, satin_gas_table, value_call_stipend, MegaInstructions};

/// The context an instruction of the Satin engine runs with.
type Ctx<'a, DB, ExtEnvs> = InstructionContext<'a, MegaContext<DB, ExtEnvs>, EthInterpreter>;

/// An instruction of the Satin engine.
type InstructionFn<DB, ExtEnvs> = fn(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult;

/// The Satin instruction table: revm's for the base spec, with the Amsterdam opcodes activated and
/// the opcodes that can read volatile data wrapped, on the Satin static gas table.
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
    for (opcode, wrapper) in wrappers {
        let static_gas = instructions.gas_table()[opcode as usize];
        instructions.insert_instruction(opcode, Instruction::new(wrapper), static_gas);
    }
    instructions
}

/// `CALL`, `CALLCODE`, `DELEGATECALL` and `STATICCALL`, committing the read of the block
/// beneficiary's account when the callee, or the EIP-7702 delegate it runs, is the beneficiary.
fn call<const KIND: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    read_volatile(context, contract::call::<KIND, _, _>)
}

/// Runs `inner` and settles the reads of volatile data its Host calls made: a refused read
/// reverts the frame with `VolatileDataAccessDisabled`, and a read the opcode completed is
/// committed, capping the frame at what the compute limit leaves it.
///
/// A refusal hands the frame back the gas it had when the wrapper started, which is after the
/// interpreter charged the opcode's static gas: the Host refuses before it loads anything, but an
/// opcode may charge part of its dynamic gas before its load (a copy's memory, a call's value
/// transfer), and none of that is owed for a read that did not happen.
///
/// A read the opcode did not complete — the opcode failed after the load — is dropped: it reached
/// no computation, and the frame is failing anyway.
#[inline(always)]
fn read_volatile<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
    inner: impl FnOnce(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    host.detention.begin_read();
    let gas = host.detention.is_refusing().then_some(interpreter.gas);
    let result = inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
    host.detention.end_read();
    if !host.detention.has_reads() {
        return result;
    }
    settle_reads(interpreter, host, result, gas)
}

/// The slow half of [`read_volatile`]: the Host observed or refused a read.
#[inline(never)]
fn settle_reads<DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &mut MegaContext<DB, ExtEnvs>,
    result: InstructionExecResult,
    gas: Option<Gas>,
) -> InstructionExecResult {
    let observed = host.detention.take_observed();
    if let Some(refused) = host.detention.take_refused() {
        return Err(refuse(interpreter, refused, gas));
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
/// it had before the opcode (`gas`, when the wrapper kept it).
#[cold]
#[inline(never)]
fn refuse(
    interpreter: &mut Interpreter<EthInterpreter>,
    refused: VolatileDataAccess,
    gas: Option<Gas>,
) -> InstructionResult {
    // The Host refuses a load before the opcode builds a frame, so no action is pending.
    if let Some(gas) = gas {
        interpreter.gas = gas;
    }
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
    ($($name:ident => $inner:path, $opcode:literal, $what:literal;)*) => {$(
        #[doc = concat!("`", $opcode, "`, committing the read of ", $what, ".")]
        fn $name<DB: Database, ExtEnvs: ExternalEnvTypes>(
            context: Ctx<'_, DB, ExtEnvs>,
        ) -> InstructionExecResult {
            read_volatile(context, $inner)
        }
    )*};
}

volatile_read! {
    coinbase => block_info::coinbase, "COINBASE", "the field";
    timestamp => block_info::timestamp, "TIMESTAMP", "the field";
    number => block_info::block_number, "NUMBER", "the field";
    difficulty => block_info::difficulty, "PREVRANDAO", "the field";
    gaslimit => block_info::gaslimit, "GASLIMIT", "the field";
    basefee => block_info::basefee, "BASEFEE", "the field";
    blobbasefee => block_info::blob_basefee, "BLOBBASEFEE", "the field";
    slotnum => block_info::slot_num_enabled, "SLOTNUM", "the field";
    balance => host::balance, "BALANCE", "the block beneficiary's account";
    selfbalance => host::selfbalance, "SELFBALANCE", "the block beneficiary's account";
    extcodesize => host::extcodesize, "EXTCODESIZE", "the block beneficiary's account";
    extcodecopy => host::extcodecopy, "EXTCODECOPY", "the block beneficiary's account";
    extcodehash => host::extcodehash, "EXTCODEHASH", "the block beneficiary's account";
    sload => host::sload, "SLOAD", "the Oracle's storage";
    selfdestruct => host::selfdestruct, "SELFDESTRUCT", "the block beneficiary's account";
}

/// `BLOCKHASH`, committing the read of the block number and of the hash.
///
/// revm's instruction reads the block number before the hash, so a refusal is of the number
/// first; it is named for the hash, the kind `BLOCKHASH` reads, as the legacy engine named it.
fn blockhash<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    read_volatile(context, |InstructionContext { interpreter, host }| {
        let result = host::blockhash(InstructionContext { interpreter, host: &mut *host });
        host.detention.name_refusal(VolatileDataAccess::BLOCK_HASH);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
    use revm::bytecode::opcode::{CLZ, DUPN, EXCHANGE, SLOTNUM, SWAPN};

    /// The wrappers keep the static gas the Satin table charges for their opcodes: the whole
    /// table is revm's Amsterdam one.
    ///
    /// Activating the Amsterdam opcodes does not touch it either — their static gas is the same
    /// on every spec, which is why the activation is an instruction-table change alone.
    #[test]
    fn test_wrappers_keep_the_static_gas_table() {
        let mega = mega_instructions::<MemoryDatabase, EmptyExternalEnv>(SpecId::OSAKA);
        assert_eq!(mega.gas_table(), &satin_gas_table());
        assert_eq!(mega.spec, SpecId::OSAKA);
        for (opcode, gas) in [(DUPN, 3), (SWAPN, 3), (EXCHANGE, 3), (SLOTNUM, 2), (CLZ, 5)] {
            assert_eq!(mega.gas_table()[opcode as usize], gas, "opcode {opcode:#04x}");
        }
    }
}
