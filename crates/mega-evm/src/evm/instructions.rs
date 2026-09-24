//! The instruction table of the Satin engine.
//!
//! Every opcode runs revm's own instruction, except the ones that write state the resource
//! limits count. Three of them write it themselves — `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT`
//! — and four write it by starting a frame: `CALL`, `CALLCODE`, `CREATE` and `CREATE2`.
//!
//! The first three run in a wrapper that commits what the Host staged for them after the opcode
//! completed:
//!
//! 1. discard any record staged before the opcode (nothing may commit it for this one);
//! 2. run revm's instruction, whose Host call stages the record and which charges the state gas of
//!    a fresh slot or a new beneficiary;
//! 3. discard the record if the opcode failed; if it completed, hold the state gas it charged to
//!    the state-gas limit, then commit the record;
//! 4. charge the frame the history gas of what the record appends — and give it back when the
//!    record was taken away, as a slot written back to its original value takes its own back.
//!
//! The history a record costs is its data size, so the charge and the count cannot drift apart:
//! a log pays for its address, its topics and its data, a storage write or a destructed account's
//! beneficiary for the forty bytes of one write record.
//!
//! A state charge or a commit that crosses a limit stops the opcode's frame with a revert whose
//! output is [`MegaLimitExceeded`](crate::MegaLimitExceeded) (see
//! [`AdditionalLimit`](crate::AdditionalLimit) for the abort protocol). A history charge the frame
//! cannot pay is an ordinary out-of-gas, which burns the frame's gas the way any other does.
//!
//! The other four run in a wrapper that charges their frame for the write records the frame it
//! starts makes — a value transfer's sender and recipient, a creation's creator nonce and created
//! account — before the frame runs, so the gas it forwards is not reduced by them and its
//! allowance is free for what the recipient does. What the frame does not keep goes back to the
//! caller when it returns. The frame the opcode is suspending on carries the caller's reservoir,
//! which the charge has just moved, so the wrapper writes the reservoir it left into the frame's
//! input; and a caller that cannot pay the charge drops that frame before it fails, because an
//! interpreter halts on an instruction's error only when no frame is pending. The state gas revm's
//! instruction charged for the account the frame would add is held to the state-gas limit later,
//! once revm has decided the frame: revm refuses some frames after the charge — a value call its
//! caller cannot fund, one past the call-stack limit — and gives the charge back.
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
//! `SSTORE` also holds its frame to the compute limit again once it completed: a slot restored to
//! its original value refills regular gas.
//!
//! An opcode whose charge fails with an out-of-gas halts its frame, and the halt zeroes the gas
//! the frame had left before the frame returns. Every wrapper above notes that gas for gas
//! detention first, which does not count what a halt burns as compute; so does a wrapper around
//! the opcodes whose own charge has no bound, `KECCAK256` and the four copies into memory.
//!
//! A wrapper keeps the static gas revm's table charges for its opcode, so the gas schedule is
//! unchanged.
//!
//! The table also carries the Amsterdam opcodes (`DUPN`, `SWAPN`, `EXCHANGE`, `SLOTNUM`), which
//! the base spec gates off: Satin takes the Amsterdam schedule, so it takes the opcodes with it.
//! They are installed before the wrappers go in, because installing them after would replace a
//! wrapped entry with an unwrapped one. `CLZ` needs no installation — its own gate is Osaka, so
//! the base spec already has it.

use revm::{
    bytecode::opcode::{
        BALANCE, BASEFEE, BLOBBASEFEE, BLOCKHASH, CALL, CALLCODE, CALLDATACOPY, CODECOPY, COINBASE,
        CREATE, CREATE2, DELEGATECALL, DIFFICULTY, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, GASLIMIT,
        KECCAK256, LOG0, LOG1, LOG2, LOG3, LOG4, MCOPY, NUMBER, RETURNDATACOPY, SELFBALANCE,
        SELFDESTRUCT, SLOAD, SLOTNUM, SSTORE, STATICCALL, TIMESTAMP,
    },
    context_interface::Host,
    handler::instructions::EthInstructions,
    interpreter::{
        enable_amsterdam_opcodes, instruction_table,
        instructions::{block_info, contract, gas_table_spec, host, memory, system},
        interpreter::EthInterpreter,
        interpreter_types::LoopControl,
        FrameInput, Gas, Instruction, InstructionContext, InstructionExecResult, InstructionResult,
        Interpreter, InterpreterAction,
    },
    primitives::hardfork::SpecId,
    Database,
};

use crate::{
    history_gas, limit::HistoryBytes, volatile_data_access_disabled_revert_data,
    write_record_history_gas, ExternalEnvTypes, LimitCheck, MegaContext, VolatileDataAccess,
};

use super::MegaInstructions;

/// The context an instruction of the Satin engine runs with.
type Ctx<'a, DB, ExtEnvs> = InstructionContext<'a, MegaContext<DB, ExtEnvs>, EthInterpreter>;

/// An instruction of the Satin engine.
type InstructionFn<DB, ExtEnvs> = fn(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult;

/// The Satin instruction table: revm's for the base spec, with the Amsterdam opcodes activated,
/// the opcodes that write state the limits count wrapped, and the opcodes that can read volatile
/// data wrapped.
pub(crate) fn mega_instructions<DB: Database, ExtEnvs: ExternalEnvTypes>(
    spec: SpecId,
) -> MegaInstructions<DB, ExtEnvs> {
    let mut table = instruction_table();
    enable_amsterdam_opcodes(&mut table);
    let mut instructions = EthInstructions::new(table, gas_table_spec(spec), spec);
    let wrappers: [(u8, InstructionFn<DB, ExtEnvs>); 33] = [
        (SSTORE, sstore::<DB, ExtEnvs>),
        (LOG0, log::<0, DB, ExtEnvs>),
        (LOG1, log::<1, DB, ExtEnvs>),
        (LOG2, log::<2, DB, ExtEnvs>),
        (LOG3, log::<3, DB, ExtEnvs>),
        (LOG4, log::<4, DB, ExtEnvs>),
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
        (KECCAK256, keccak256::<DB, ExtEnvs>),
        (CALLDATACOPY, calldatacopy::<DB, ExtEnvs>),
        (CODECOPY, codecopy::<DB, ExtEnvs>),
        (RETURNDATACOPY, returndatacopy::<DB, ExtEnvs>),
        (MCOPY, mcopy::<DB, ExtEnvs>),
    ];
    for (opcode, wrapper) in wrappers {
        let static_gas = instructions.gas_table()[opcode as usize];
        instructions.insert_instruction(opcode, Instruction::new(wrapper), static_gas);
    }
    instructions
}

/// Runs `inner`, commits the record its Host call staged once it completed, and settles the
/// history gas the record costs.
///
/// An opcode completes when it returns `Ok` or stops the frame successfully (`SELFDESTRUCT`
/// returns its own `SelfDestruct` result). Any other result fails the opcode, which takes the
/// staged write back with it.
///
/// The state gas the opcode charged is held to the state-gas limit first — `SSTORE` charges a
/// fresh slot's and `SELFDESTRUCT` a new beneficiary's inside revm's instruction — and a crossing
/// stops the frame and discards the record with the write. Then the record is held to the
/// data-size and KV limits, and only then is its history charged. A record a limit rejects is not
/// kept, so its history is not a charge and the stop is what the frame reports. A record the
/// limits accept is charged, and a charge the frame cannot pay is an out-of-gas.
#[inline(always)]
fn commit_after<const FROM_ALLOWANCE: bool, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
    inner: InstructionFn<DB, ExtEnvs>,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    host.additional_limit.discard_stale_record();
    let result = inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
    let completed = match result {
        Ok(()) => true,
        Err(result) => result.is_ok(),
    };
    if !completed {
        host.additional_limit.discard_staged_record();
        return note_halt(interpreter, host, result);
    }
    let check = host.additional_limit.check_state_gas(interpreter.gas.state_gas_spent());
    if check.exceeded_limit() {
        host.additional_limit.discard_staged_record();
        return Err(stop_frame(interpreter, &check));
    }
    let (check, history) = host.additional_limit.commit_staged_record();
    if check.exceeded_limit() {
        return Err(stop_frame(interpreter, &check));
    }
    if host.prices_history() {
        let settled = settle_history::<FROM_ALLOWANCE, _, _>(interpreter, host, history);
        if settled.is_err() {
            return note_halt(interpreter, host, settled);
        }
    }
    result
}

/// Charges the running frame the history gas of the bytes a record appends, or gives back the
/// history of a record that was taken away.
///
/// Both sides are priced the same way, so a record taken back cancels its own charge exactly,
/// whichever frame made it: a refill below zero is reconciled when the frame merges into its
/// caller. A byte count with no price and a charge the frame cannot pay are both an out-of-gas.
///
/// With `FROM_ALLOWANCE` the frame's history allowance pays what it can of the charge before the
/// frame's gas pays the rest ([`storage_call_stipend`](crate::storage_call_stipend)). Only the
/// log site sets it: a write record is the frame's own to pay for, and a record taken back gives
/// back what the frame's gas paid, never what the allowance did — the allowance is spent, not
/// lent.
#[inline]
fn settle_history<const FROM_ALLOWANCE: bool, DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &mut MegaContext<DB, ExtEnvs>,
    history: HistoryBytes,
) -> Result<(), InstructionResult> {
    match history {
        HistoryBytes::None => Ok(()),
        HistoryBytes::Appended(bytes) => {
            let Some(cost) = history_gas(bytes) else { return Err(InstructionResult::OutOfGas) };
            let drawn =
                if FROM_ALLOWANCE { host.additional_limit.try_consume_stipend(cost) } else { 0 };
            if interpreter.gas.record_history_cost(cost - drawn) {
                Ok(())
            } else {
                Err(InstructionResult::OutOfGas)
            }
        }
        HistoryBytes::Taken(bytes) => {
            interpreter.gas.refill_history(history_gas(bytes).unwrap_or(0));
            Ok(())
        }
    }
}

/// Stops the running frame with the revert a crossed limit asks for: its output is
/// [`MegaLimitExceeded`](crate::MegaLimitExceeded) and its gas is the frame's, so the caller gets
/// the unspent part back.
#[cold]
#[inline(never)]
fn stop_frame(
    interpreter: &mut Interpreter<EthInterpreter>,
    check: &LimitCheck,
) -> InstructionResult {
    if interpreter.bytecode.action().is_some() {
        // A `SELFDESTRUCT` or a return the opcode already set gives way to the stop.
        let _ = interpreter.take_next_action();
    }
    let result = InstructionResult::Revert;
    interpreter.bytecode.set_action(InterpreterAction::new_return(
        result,
        check.revert_data(),
        interpreter.gas,
    ));
    result
}

/// `SSTORE`, committing the slot's write record, then holding the frame to the compute limit
/// again: a slot restored to its original value refills the state and history gas that spilled
/// onto regular gas, and the spill may predate the limit.
fn sstore<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    let result = commit_after::<false, _, _>(
        InstructionContext { interpreter: &mut *interpreter, host: &mut *host },
        host::sstore,
    );
    // Only a restore refills, and a restore takes a record back rather than adding one, so it
    // never crosses a limit: no stop's result is pending when the frame is held again.
    host.detention.hold(&mut interpreter.gas);
    result
}

/// `LOG0`..`LOG4`, committing the log's bytes.
fn log<const N: usize, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after::<true, _, _>(context, host::log::<N, MegaContext<DB, ExtEnvs>>)
}

/// `SELFDESTRUCT`, committing the beneficiary's write record and the read of the block
/// beneficiary's account when it is either end.
fn selfdestruct<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    read_volatile(context, |context| commit_after::<false, _, _>(context, host::selfdestruct))
}

/// Runs `inner` and charges the frame the history of the write records the frame `inner` starts
/// will make.
///
/// The charge is made after revm's instruction has computed the gas it forwards, so it comes out
/// of what the caller kept rather than out of what the callee gets. An opcode that starts no frame
/// — a creation the balance, the nonce or the depth refuses, an out-of-gas — makes no records and
/// is charged nothing. A frame revm refuses once it has it — a value call the caller cannot fund,
/// one past the call-stack limit — makes no records either, and its failure gives the charge
/// back. A charge the caller cannot pay fails the opcode with an out-of-gas, which takes the frame
/// the opcode was suspending on with it ([`abandon_frame`]).
///
/// revm's instruction has also charged the caller the state gas of the account the frame would
/// add — a value transfer's new recipient, a created account. That charge is held to the state-gas
/// limit when revm has decided the frame, not here: a frame revm refuses gives it back, and a
/// limit that held it would stop the transaction for an account nobody adds.
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
    // otherwise — a call the balance cannot fund, an out-of-gas — leaves no such action and makes
    // no records.
    let records = match interpreter.bytecode.action() {
        Some(InterpreterAction::NewFrame(input)) => {
            host.additional_limit.frame_start_records(input)
        }
        _ => return note_halt(interpreter, host, result),
    };
    if !host.prices_history() {
        return result;
    }
    let (Some(on_lane), Some(caller)) = (
        write_record_history_gas(records.on_lane),
        write_record_history_gas(u64::from(records.caller)),
    ) else {
        return abandon_frame(interpreter, host);
    };
    let Some(cost) = on_lane.checked_add(caller) else { return abandon_frame(interpreter, host) };
    if !interpreter.gas.record_history_cost(cost) {
        return abandon_frame(interpreter, host);
    }
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
#[cold]
#[inline(never)]
fn abandon_frame<DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &mut MegaContext<DB, ExtEnvs>,
) -> InstructionExecResult {
    if let InterpreterAction::NewFrame(input) = interpreter.take_next_action() {
        interpreter.gas.erase_cost(forwarded_gas(&input, host.gas_params().call_stipend()));
    }
    note_halt(interpreter, host, Err(InstructionResult::OutOfGas))
}

/// Passes `result` on, first noting for gas detention what the running frame has left when
/// `result` is an out-of-gas the interpreter will halt the frame on: the halt zeroes it before the
/// frame returns, and it is what the halt burns rather than what the frame ran.
///
/// The interpreter halts on an instruction's error only when no action is pending; an opcode that
/// set one ends its frame with that action instead, and burns nothing.
#[inline(always)]
fn note_halt<DB: Database, ExtEnvs: ExternalEnvTypes>(
    interpreter: &mut Interpreter<EthInterpreter>,
    host: &mut MegaContext<DB, ExtEnvs>,
    result: InstructionExecResult,
) -> InstructionExecResult {
    if result == Err(InstructionResult::OutOfGas) && interpreter.bytecode.action().is_none() {
        host.detention.note_halt(interpreter.gas.remaining());
    }
    result
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
        return note_halt(interpreter, host, result);
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
        return note_halt(interpreter, host, result);
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
    match input {
        FrameInput::Call(inputs) if inputs.transfers_value() => {
            inputs.gas_limit.saturating_sub(call_stipend)
        }
        FrameInput::Call(inputs) => inputs.gas_limit,
        FrameInput::Create(inputs) => inputs.gas_limit(),
        FrameInput::Empty => 0,
    }
}

/// Defines an opcode that reads a block-environment field, as revm's instruction in
/// [`read_volatile`].
macro_rules! block_env_read {
    ($($name:ident => $inner:path, $opcode:literal;)*) => {$(
        #[doc = concat!("`", $opcode, "`, committing the read of the field.")]
        fn $name<DB: Database, ExtEnvs: ExternalEnvTypes>(
            context: Ctx<'_, DB, ExtEnvs>,
        ) -> InstructionExecResult {
            read_volatile(context, $inner)
        }
    )*};
}

block_env_read! {
    coinbase => block_info::coinbase, "COINBASE";
    timestamp => block_info::timestamp, "TIMESTAMP";
    number => block_info::block_number, "NUMBER";
    difficulty => block_info::difficulty, "PREVRANDAO";
    gaslimit => block_info::gaslimit, "GASLIMIT";
    basefee => block_info::basefee, "BASEFEE";
    blobbasefee => block_info::blob_basefee, "BLOBBASEFEE";
    slotnum => block_info::slot_num_enabled, "SLOTNUM";
    blockhash => host::blockhash, "BLOCKHASH";
}

/// Defines an opcode that loads an account or a slot, as revm's instruction in
/// [`read_volatile`].
macro_rules! state_read {
    ($($name:ident => $inner:path, $opcode:literal, $what:literal;)*) => {$(
        #[doc = concat!("`", $opcode, "`, committing the read of ", $what, ".")]
        fn $name<DB: Database, ExtEnvs: ExternalEnvTypes>(
            context: Ctx<'_, DB, ExtEnvs>,
        ) -> InstructionExecResult {
            read_volatile(context, $inner)
        }
    )*};
}

state_read! {
    balance => host::balance, "BALANCE", "the block beneficiary's account";
    selfbalance => host::selfbalance, "SELFBALANCE", "the block beneficiary's account";
    extcodesize => host::extcodesize, "EXTCODESIZE", "the block beneficiary's account";
    extcodecopy => host::extcodecopy, "EXTCODECOPY", "the block beneficiary's account";
    extcodehash => host::extcodehash, "EXTCODEHASH", "the block beneficiary's account";
    sload => host::sload, "SLOAD", "the Oracle's storage";
}

/// Defines an opcode whose own charge has no bound, as revm's instruction with what the frame had
/// left noted when the charge fails ([`note_halt`]).
macro_rules! unbounded_charge {
    ($($name:ident => $inner:path, $opcode:literal;)*) => {$(
        #[doc = concat!("`", $opcode, "`, noting what the frame had when its charge fails.")]
        fn $name<DB: Database, ExtEnvs: ExternalEnvTypes>(
            context: Ctx<'_, DB, ExtEnvs>,
        ) -> InstructionExecResult {
            let InstructionContext { interpreter, host } = context;
            let result = $inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
            note_halt(interpreter, host, result)
        }
    )*};
}

unbounded_charge! {
    keccak256 => system::keccak256, "KECCAK256";
    calldatacopy => system::calldatacopy, "CALLDATACOPY";
    codecopy => system::codecopy, "CODECOPY";
    returndatacopy => system::returndatacopy, "RETURNDATACOPY";
    mcopy => memory::mcopy, "MCOPY";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
    use revm::bytecode::opcode::{CLZ, DUPN, EXCHANGE, SLOTNUM, SWAPN};

    /// The wrappers keep the static gas revm charges for their opcodes: the whole table is revm's.
    ///
    /// Activating the Amsterdam opcodes does not touch it either — their static gas is the same
    /// on every spec, which is why the activation is an instruction-table change alone.
    #[test]
    fn test_wrappers_keep_the_static_gas_table() {
        let mega = mega_instructions::<MemoryDatabase, EmptyExternalEnv>(SpecId::OSAKA);
        assert_eq!(mega.gas_table(), &gas_table_spec(SpecId::OSAKA));
        assert_eq!(mega.spec, SpecId::OSAKA);
        for (opcode, gas) in [(DUPN, 3), (SWAPN, 3), (EXCHANGE, 3), (SLOTNUM, 2), (CLZ, 5)] {
            assert_eq!(mega.gas_table()[opcode as usize], gas, "opcode {opcode:#04x}");
        }
    }
}
