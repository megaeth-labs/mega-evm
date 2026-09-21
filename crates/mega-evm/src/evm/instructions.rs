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
//! 2. run revm's instruction, whose Host call stages the record;
//! 3. commit the record if the opcode completed, discard it if the opcode failed;
//! 4. charge the frame the history gas of what the record appends — and give it back when the
//!    record was taken away, as a slot written back to its original value takes its own back.
//!
//! The history a record costs is its data size, so the charge and the count cannot drift apart:
//! a log pays for its address, its topics and its data, a storage write or a destructed account's
//! beneficiary for the forty bytes of one write record.
//!
//! A commit that crosses a limit stops the opcode's frame with a revert whose output is
//! [`MegaLimitExceeded`](crate::MegaLimitExceeded) (see
//! [`AdditionalLimit`](crate::AdditionalLimit) for the abort protocol). A history charge the frame
//! cannot pay is an ordinary out-of-gas, which burns the frame's gas the way any other does.
//!
//! The other four run in a wrapper that charges their frame for the write records the frame it
//! starts makes — a value transfer's sender and recipient, a creation's creator nonce and created
//! account — before the frame runs, so the gas it forwards is not reduced by them and its
//! allowance is free for what the recipient does. What the frame does not keep goes back to the
//! caller when it returns.
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
        CALL, CALLCODE, CREATE, CREATE2, LOG0, LOG1, LOG2, LOG3, LOG4, SELFDESTRUCT, SSTORE,
    },
    handler::instructions::EthInstructions,
    interpreter::{
        enable_amsterdam_opcodes, instruction_table,
        instructions::{contract, gas_table_spec, host},
        interpreter::EthInterpreter,
        interpreter_types::LoopControl,
        Instruction, InstructionContext, InstructionExecResult, InstructionResult, Interpreter,
        InterpreterAction,
    },
    primitives::hardfork::SpecId,
    Database,
};

use crate::{
    history_gas, limit::HistoryBytes, write_record_history_gas, ExternalEnvTypes, LimitCheck,
    MegaContext,
};

use super::MegaInstructions;

/// The context an instruction of the Satin engine runs with.
type Ctx<'a, DB, ExtEnvs> = InstructionContext<'a, MegaContext<DB, ExtEnvs>, EthInterpreter>;

/// An instruction of the Satin engine.
type InstructionFn<DB, ExtEnvs> = fn(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult;

/// The Satin instruction table: revm's for the base spec, with the Amsterdam opcodes activated
/// and `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT` wrapped.
pub(crate) fn mega_instructions<DB: Database, ExtEnvs: ExternalEnvTypes>(
    spec: SpecId,
) -> MegaInstructions<DB, ExtEnvs> {
    let mut table = instruction_table();
    enable_amsterdam_opcodes(&mut table);
    let mut instructions = EthInstructions::new(table, gas_table_spec(spec), spec);
    let wrappers: [(u8, InstructionFn<DB, ExtEnvs>); 11] = [
        (SSTORE, sstore::<DB, ExtEnvs>),
        (LOG0, log::<0, DB, ExtEnvs>),
        (LOG1, log::<1, DB, ExtEnvs>),
        (LOG2, log::<2, DB, ExtEnvs>),
        (LOG3, log::<3, DB, ExtEnvs>),
        (LOG4, log::<4, DB, ExtEnvs>),
        (SELFDESTRUCT, selfdestruct::<DB, ExtEnvs>),
        (CALL, call::<CALL, DB, ExtEnvs>),
        (CALLCODE, call::<CALLCODE, DB, ExtEnvs>),
        (CREATE, create::<false, DB, ExtEnvs>),
        (CREATE2, create::<true, DB, ExtEnvs>),
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
        return result;
    }
    let (check, history) = host.additional_limit.commit_staged_record();
    if host.prices_history() {
        settle_history::<FROM_ALLOWANCE, _, _>(interpreter, host, history)?;
    }
    if check.exceeded_limit() {
        return Err(stop_frame(interpreter, &check));
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

/// `SSTORE`, committing the slot's write record.
fn sstore<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after::<false, _, _>(context, host::sstore)
}

/// `LOG0`..`LOG4`, committing the log's bytes.
fn log<const N: usize, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after::<true, _, _>(context, host::log::<N, MegaContext<DB, ExtEnvs>>)
}

/// `SELFDESTRUCT`, committing the beneficiary's write record.
fn selfdestruct<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after::<false, _, _>(context, host::selfdestruct)
}

/// Runs `inner` and charges the frame the history of the write records the frame `inner` starts
/// will make.
///
/// The charge is made after revm's instruction has computed the gas it forwards, so it comes out
/// of what the caller kept rather than out of what the callee gets. An opcode that starts no
/// frame — a call the balance cannot fund, a creation the depth refuses — makes no records and is
/// charged nothing. A charge the frame cannot pay is an ordinary out-of-gas.
#[inline(always)]
fn charge_frame_start<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
    inner: InstructionFn<DB, ExtEnvs>,
) -> InstructionExecResult {
    let InstructionContext { interpreter, host } = context;
    let result = inner(InstructionContext { interpreter: &mut *interpreter, host: &mut *host });
    if !host.prices_history() {
        return result;
    }
    // An opcode that starts a frame suspends with the frame's input as its action; one that ends
    // otherwise — a call the balance cannot fund, an out-of-gas — leaves no such action and makes
    // no records.
    let Some(InterpreterAction::NewFrame(input)) = interpreter.bytecode.action() else {
        return result;
    };
    let records = host.additional_limit.frame_start_records(input);
    if records.total() == 0 {
        return result;
    }
    let (Some(on_lane), Some(caller)) = (
        write_record_history_gas(records.on_lane),
        write_record_history_gas(u64::from(records.caller)),
    ) else {
        return Err(InstructionResult::OutOfGas);
    };
    let Some(cost) = on_lane.checked_add(caller) else { return Err(InstructionResult::OutOfGas) };
    if !interpreter.gas.record_history_cost(cost) {
        return Err(InstructionResult::OutOfGas);
    }
    host.additional_limit.stage_frame_charge(on_lane, caller);
    result
}

/// `CALL` and `CALLCODE`, charging the caller for the records a value transfer writes.
///
/// `CALLCODE` runs the callee's code in the caller's own account, so the two records of a `CALL`
/// — the sender's and the recipient's — are one here. `DELEGATECALL` and `STATICCALL` carry no
/// value and write nothing, so they run revm's instruction unwrapped.
fn call<const KIND: u8, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    charge_frame_start(context, contract::call::<KIND, _, _>)
}

/// `CREATE` and `CREATE2`, charging the creator for the created account's record and for its own
/// nonce.
fn create<const IS_CREATE2: bool, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    charge_frame_start(context, contract::create::<IS_CREATE2, _, _>)
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
