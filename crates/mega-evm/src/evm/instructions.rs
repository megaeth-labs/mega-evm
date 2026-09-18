//! The instruction table of the Satin engine.
//!
//! Every opcode runs revm's own instruction, except the three that write state the resource
//! limits count: `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT`. Each of those runs in a wrapper
//! that commits what the Host staged for it after the opcode completed:
//!
//! 1. discard any record staged before the opcode (nothing may commit it for this one);
//! 2. run revm's instruction, whose Host call stages the record;
//! 3. commit the record if the opcode completed, discard it if the opcode failed.
//!
//! A wrapper keeps the static gas revm's table charges for its opcode, so the gas schedule is
//! unchanged.

use revm::{
    bytecode::opcode::{LOG0, LOG1, LOG2, LOG3, LOG4, SELFDESTRUCT, SSTORE},
    handler::instructions::EthInstructions,
    interpreter::{
        instructions::host, interpreter::EthInterpreter, Instruction, InstructionContext,
        InstructionExecResult,
    },
    primitives::hardfork::SpecId,
    Database,
};

use crate::{ExternalEnvTypes, MegaContext};

use super::MegaInstructions;

/// The context an instruction of the Satin engine runs with.
type Ctx<'a, DB, ExtEnvs> = InstructionContext<'a, MegaContext<DB, ExtEnvs>, EthInterpreter>;

/// An instruction of the Satin engine.
type InstructionFn<DB, ExtEnvs> = fn(Ctx<'_, DB, ExtEnvs>) -> InstructionExecResult;

/// The Satin instruction table: revm's for the base spec, with `SSTORE`, `LOG0`..`LOG4` and
/// `SELFDESTRUCT` wrapped.
pub(crate) fn mega_instructions<DB: Database, ExtEnvs: ExternalEnvTypes>(
    spec: SpecId,
) -> MegaInstructions<DB, ExtEnvs> {
    let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
    let wrappers: [(u8, InstructionFn<DB, ExtEnvs>); 7] = [
        (SSTORE, sstore::<DB, ExtEnvs>),
        (LOG0, log::<0, DB, ExtEnvs>),
        (LOG1, log::<1, DB, ExtEnvs>),
        (LOG2, log::<2, DB, ExtEnvs>),
        (LOG3, log::<3, DB, ExtEnvs>),
        (LOG4, log::<4, DB, ExtEnvs>),
        (SELFDESTRUCT, selfdestruct::<DB, ExtEnvs>),
    ];
    for (opcode, wrapper) in wrappers {
        let static_gas = instructions.gas_table()[opcode as usize];
        instructions.insert_instruction(opcode, Instruction::new(wrapper), static_gas);
    }
    instructions
}

/// Runs `inner` and commits the record its Host call staged once it completed.
///
/// An opcode completes when it returns `Ok` or stops the frame successfully (`SELFDESTRUCT`
/// returns its own `SelfDestruct` result). Any other result fails the opcode, which takes the
/// staged write back with it.
#[inline(always)]
fn commit_after<DB: Database, ExtEnvs: ExternalEnvTypes>(
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
    if completed {
        host.additional_limit.commit_staged_record();
    } else {
        host.additional_limit.discard_staged_record();
    }
    result
}

/// `SSTORE`, committing the slot's write record.
fn sstore<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after(context, host::sstore)
}

/// `LOG0`..`LOG4`, committing the log's bytes.
fn log<const N: usize, DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after(context, host::log::<N, MegaContext<DB, ExtEnvs>>)
}

/// `SELFDESTRUCT`, committing the beneficiary's write record.
fn selfdestruct<DB: Database, ExtEnvs: ExternalEnvTypes>(
    context: Ctx<'_, DB, ExtEnvs>,
) -> InstructionExecResult {
    commit_after(context, host::selfdestruct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
    use revm::interpreter::instructions::gas_table_spec;

    /// The wrappers keep the static gas revm charges for their opcodes: the whole table is revm's.
    #[test]
    fn test_wrappers_keep_the_static_gas_table() {
        let mega = mega_instructions::<MemoryDatabase, EmptyExternalEnv>(SpecId::OSAKA);
        assert_eq!(mega.gas_table(), &gas_table_spec(SpecId::OSAKA));
        assert_eq!(mega.spec, SpecId::OSAKA);
    }
}
