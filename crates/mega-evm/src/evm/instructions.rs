//! The instruction table of the Satin engine.
//!
//! Every opcode runs revm's own instruction, except the four that write state the resource limits
//! count by starting a frame — `CALL`, `CALLCODE`, `CREATE` and `CREATE2` — and the ones that can
//! read volatile data.
//!
//! The opcodes that write state themselves — `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT` — are
//! revm's own: revm calls the Host's completed-write hook at the end of each, once the write is in
//! the journal and every charge the opcode makes has been made, and the Host counts the write,
//! holds it to the limits and charges the history it costs there (see the `host` module). A failed
//! opcode never reaches the hook, so it counts nothing. `SELFDESTRUCT` is wrapped for the volatile
//! read it can make alone.
//!
//! The four that start a frame run in a wrapper that charges their frame for the write records the
//! frame it starts makes — a value transfer's sender and recipient, a creation's creator nonce and
//! created account — before the frame runs, so the gas it forwards is not reduced by them and its
//! allowance is free for what the recipient does. What the frame does not keep goes back to the
//! caller when it returns. The frame the opcode is suspending on carries the caller's reservoir,
//! which the charge has just moved, so the wrapper writes the reservoir it left into the frame's
//! input; and a caller that cannot pay the charge drops that frame before it fails, because an
//! interpreter halts on an instruction's error only when no frame is pending.
//!
//! Every opcode that can read volatile data — the block-environment opcodes, the account opcodes,
//! `SLOAD`, the four calls and `SELFDESTRUCT` — runs in a wrapper that settles the reads its Host
//! calls made (see the `access` module):
//!
//! 1. discard any read observed or refused before the opcode;
//! 2. keep the frame's gas, when the frame's reads are refused;
//! 3. run the instruction (or the wrapper above it), whose Host calls observe or refuse the reads;
//! 4. on a refusal, hand the frame back the gas it had before the opcode and revert it with
//!    `VolatileDataAccessDisabled`; otherwise, once the opcode completed, commit the reads, which
//!    holds the frame's spendable gas to what the compute limit leaves it.
//!
//! An opcode whose charge fails with an out-of-gas halts its frame, and the halt zeroes the gas
//! the frame had left before the frame returns. The revm fork records that gas first, before
//! every deduction that fails — an opcode's static gas, which the interpreter's step loop charges
//! before the opcode runs, included — and before a halt on a cold access the frame cannot pay;
//! gas detention reads the record where the frame ends, and does not count what a halt burns as
//! compute. The one wrapper that writes the record is the one that drops a frame its opcode was
//! starting: it hands the frame's forward back to its caller first.
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
        BALANCE, BASEFEE, BLOBBASEFEE, BLOCKHASH, CALL, CALLCODE, COINBASE, CREATE, CREATE2,
        DELEGATECALL, DIFFICULTY, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, GASLIMIT, NUMBER,
        SELFBALANCE, SELFDESTRUCT, SLOAD, SLOTNUM, STATICCALL, TIMESTAMP,
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
    charge_write_records_history, volatile_data_access_disabled_revert_data, ExternalEnvTypes,
    MegaContext, VolatileDataAccess,
};

use super::{input_gas_limit, satin_gas_table, value_call_stipend, MegaInstructions};

/// The context an instruction of the Satin engine runs with.
type Ctx<'a, DB, ExtEnvs> = InstructionContext<'a, MegaContext<DB, ExtEnvs>, EthInterpreter>;

/// An instruction of the Satin engine.
type InstructionFn<DB, ExtEnvs> = fn(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult;

/// The Satin instruction table: revm's for the base spec, with the Amsterdam opcodes activated,
/// the opcodes that start a frame whose start the limits count wrapped, and the opcodes that can
/// read volatile data wrapped, on the Satin static gas table.
pub(crate) fn mega_instructions<DB: Database, ExtEnvs: ExternalEnvTypes>(
    spec: SpecId,
) -> MegaInstructions<DB, ExtEnvs> {
    let mut table = instruction_table();
    enable_amsterdam_opcodes(&mut table);
    let mut instructions = EthInstructions::new(table, satin_gas_table(), spec);
    let wrappers: [(u8, InstructionFn<DB, ExtEnvs>); 22] = [
        (SELFDESTRUCT, selfdestruct::<DB, ExtEnvs>),
        (CALL, call::<CALL, DB, ExtEnvs>),
        (CALLCODE, call::<CALLCODE, DB, ExtEnvs>),
        (DELEGATECALL, volatile_call::<DELEGATECALL, DB, ExtEnvs>),
        (STATICCALL, volatile_call::<STATICCALL, DB, ExtEnvs>),
        (CREATE, create::<false, DB, ExtEnvs>),
        (CREATE2, create::<true, DB, ExtEnvs>),
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

/// Runs `inner` and charges the frame the history of the write records the frame `inner` starts
/// will make.
///
/// The charge is made after revm's instruction has computed the gas it forwards, so it comes out
/// of what the caller kept rather than out of what the callee gets. An opcode that starts no frame
/// — a creation the balance, the nonce or the depth refuses, an out-of-gas — makes no records and
/// is charged nothing. Every frame an opcode starts is charged, revm deciding the start only once
/// the frame's init asks it: a start revm then refuses — a value the caller cannot fund, a frame
/// past the call-stack limit — makes no records, and gives the charge back when it returns. A
/// charge the caller cannot pay fails the opcode with an out-of-gas, which takes the frame the
/// opcode was suspending on with it ([`abandon_frame`]), even where revm would have refused the
/// start.
///
/// revm's instruction has also charged the caller the state gas of the account the frame would
/// add — a value transfer's new recipient, a created account; a frame revm refuses gives it back.
///
/// The frame inherits the reservoir the charge left, not the one the caller held before it
/// ([`inherit_reservoir`]).
#[inline(always)]
fn charge_frame_start<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
    inner: InstructionFn<DB, ExtEnvs>,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    let result = inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
    // An opcode that starts a frame suspends with the frame's input as its action; one that ends
    // otherwise — a creation the balance cannot fund, an out-of-gas — leaves no such action and
    // makes no records.
    let records = match interpreter.bytecode.action() {
        Some(InterpreterAction::NewFrame(input)) => {
            host.additional_limit.frame_start_records(input)
        }
        _ => return result,
    };
    if !host.prices_history() {
        return result;
    }
    let charged = charge_write_records_history(
        interpreter.gas.tracker_mut(),
        records.on_lane,
        u64::from(records.caller),
    );
    let Some((on_lane, caller)) = charged else { return abandon_frame(interpreter, host) };
    inherit_reservoir(interpreter);
    host.additional_limit.stage_frame_charge(records, on_lane, caller);
    result
}

/// Fails the running opcode with an out-of-gas and drops the frame it was suspending on.
///
/// The four frame-starting opcodes set the frame's input as the interpreter's action inside
/// revm's instruction, before this wrapper runs. An interpreter halts on an instruction's error
/// only when no action is pending, so a frame left pending here would start anyway — and make
/// write records nobody paid for, at any gas limit at which the caller keeps less after the
/// 63/64 forward than its records cost.
///
/// The gas the opcode forwarded to the dropped frame goes back to the caller, which the halt then
/// burns with the rest: it never ran, so gas detention counts it with what the halt burns rather
/// than as compute. A value call's stipend was never the caller's, and is not handed back.
///
/// The halt's record of what the frame had — the regular gas the revm fork records before a
/// failed charge, which detention reads where the frame ends — is written again once the forward
/// is back: the failed charge recorded the gas left after the forward was taken, and a price that
/// could not be computed failed before any charge.
#[cold]
#[inline(never)]
fn abandon_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &MegaContext<DB, ExtEnvs>,
) -> InstructionExecResult {
    if let InterpreterAction::NewFrame(input) = interpreter.take_next_action() {
        interpreter.gas.erase_cost(forwarded_gas(&input, host.gas_params().call_stipend()));
    }
    interpreter.gas.set_failed_charge_remaining(Some(interpreter.gas.remaining()));
    Err(InstructionResult::OutOfGas)
}

/// Hands the frame the running opcode is suspending on the reservoir its caller has now.
///
/// revm's `CALL`, `CALLCODE`, `CREATE` and `CREATE2` copy the caller's reservoir into the frame's
/// input, and they do it before this wrapper charges anything. A returning frame's reservoir is
/// adopted by its caller rather than merged into it, so a frame that inherited the reservoir as
/// it stood before the charge would hand the charge straight back — and a frame answered without
/// running, whose gas is built from the same field, would hand it back a second time. Writing the
/// post-charge reservoir into the input closes both.
#[inline]
fn inherit_reservoir(interpreter: &mut Interpreter<EthInterpreter>) {
    let reservoir = interpreter.gas.reservoir();
    if let Some(InterpreterAction::NewFrame(input)) = interpreter.bytecode.action() {
        match input {
            FrameInput::Call(inputs) => inputs.reservoir = reservoir,
            FrameInput::Create(inputs) => inputs.set_reservoir(reservoir),
            FrameInput::Empty => {}
        }
    }
}

/// `CALL` and `CALLCODE`, charging the caller for the records a value transfer writes and
/// committing the read of the block beneficiary's account when the callee, or the EIP-7702
/// delegate it runs, is the beneficiary.
///
/// `CALLCODE` runs the callee's code in the caller's own account, so the two records of a `CALL`
/// — the sender's and the recipient's — are one here.
fn call<const KIND: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    read_volatile(context, |context| charge_frame_start(context, contract::call::<KIND, _, _>))
}

/// `DELEGATECALL` and `STATICCALL`, committing the read of the block beneficiary's account when
/// the callee, or the EIP-7702 delegate it runs, is the beneficiary. They carry no value and write
/// nothing, so no record is charged.
fn volatile_call<const KIND: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    read_volatile(context, contract::call::<KIND, _, _>)
}

/// `CREATE` and `CREATE2`, charging the creator for the created account's record and for its own
/// nonce.
fn create<const IS_CREATE2: bool, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    charge_frame_start(context, contract::create::<IS_CREATE2, _, _>)
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
    host.detention.discard_stale_reads();
    let gas = host.detention.is_refusing().then_some(interpreter.gas);
    let result = inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
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
