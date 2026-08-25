//! Execution evidence captured from the real `KeylessDeploy` sandbox run.
//!
//! The canonical sandbox state remains the authority for state transition
//! semantics. This module only preserves execution facts which cannot be
//! reconstructed from the final state diff: ordered SSTORE targets and
//! successful KECCAK256 inputs/outputs. Capture is opt-in so ordinary `MegaEVM`
//! users keep the existing no-inspector sandbox path.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::ops::Range;
use std::vec::Vec;

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use revm::{
    bytecode::opcode,
    context::ContextTr,
    interpreter::{
        interpreter_types::{InputsTr, Jumps, LoopControl, MemoryTr},
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter, InterpreterTypes,
    },
    Inspector,
};

use crate::{JournalInspectTr, MegaSpecId, StackInspectTr};

/// One ordered execution fact from a `KeylessDeploy` sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeylessSandboxEvidenceOp {
    /// An SSTORE attempted by a frame whose state survived sandbox execution.
    Sstore {
        /// Storage context of the executing frame.
        address: Address,
        /// Storage slot supplied to SSTORE.
        slot: U256,
    },
    /// A successful KECCAK256 operation from a frame whose state survived.
    Keccak {
        /// Storage context of the executing frame.
        address: Address,
        /// Exact bytes hashed by the opcode.
        preimage: Bytes,
        /// Opcode result.
        hash: B256,
    },
}

/// Surviving execution evidence from one accepted `KeylessDeploy` sandbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeylessSandboxEvidence {
    operations: Vec<KeylessSandboxEvidenceOp>,
    observed_pre_rex5_split_create: bool,
}

impl KeylessSandboxEvidence {
    /// Ordered surviving sandbox operations.
    pub fn operations(&self) -> &[KeylessSandboxEvidenceOp] {
        &self.operations
    }

    /// Consume the artifact and return its ordered operations.
    pub fn into_operations(self) -> Vec<KeylessSandboxEvidenceOp> {
        self.operations
    }

    /// Whether the nested journal proved that a failed MiniRex-through-REX4
    /// CREATE checkpoint nevertheless survived.
    pub fn observed_pre_rex5_split_create(&self) -> bool {
        self.observed_pre_rex5_split_create
    }

    /// Whether this artifact carries no retained operation or split marker.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty() && !self.observed_pre_rex5_split_create
    }
}

#[derive(Clone, Debug)]
struct FrameSnapshot {
    operations_len: usize,
    observed_pre_rex5_split_create: bool,
}

#[derive(Clone, Debug)]
struct PendingKeccak {
    address: Address,
    offset: U256,
    size: U256,
}

/// Internal observer attached only to an opted-in `KeylessDeploy` sandbox.
#[derive(Clone, Debug)]
pub(crate) struct KeylessSandboxEvidenceRecorder {
    spec: MegaSpecId,
    evidence: KeylessSandboxEvidence,
    pending_keccak: Option<PendingKeccak>,
    frame_snapshots: Vec<FrameSnapshot>,
}

impl KeylessSandboxEvidenceRecorder {
    pub(crate) fn new(spec: MegaSpecId) -> Self {
        Self {
            spec,
            evidence: KeylessSandboxEvidence::default(),
            pending_keccak: None,
            frame_snapshots: Vec::new(),
        }
    }

    pub(crate) fn take_evidence(&mut self) -> KeylessSandboxEvidence {
        core::mem::take(&mut self.evidence)
    }

    fn push_frame_snapshot(&mut self) {
        self.frame_snapshots.push(FrameSnapshot {
            operations_len: self.evidence.operations.len(),
            observed_pre_rex5_split_create: self.evidence.observed_pre_rex5_split_create,
        });
    }

    fn finish_frame(&mut self, reverted: bool) {
        let Some(snapshot) = self.frame_snapshots.pop() else {
            return;
        };
        if reverted {
            self.evidence.operations.truncate(snapshot.operations_len);
            self.evidence.observed_pre_rex5_split_create = snapshot.observed_pre_rex5_split_create;
        }
    }

    fn failed_create_survives<CTX: JournalInspectTr>(
        &self,
        context: &mut CTX,
        outcome: &CreateOutcome,
    ) -> bool {
        // This journal read is needed only for MegaETH's frozen historical
        // split outcome. Gating before the read avoids making ordinary and
        // REX5+ failed CREATEs depend on journal/code hydration behavior.
        if !self.spec.is_enabled(MegaSpecId::MINI_REX) ||
            self.spec.is_enabled(MegaSpecId::REX5) ||
            outcome.result.result.is_ok()
        {
            return false;
        }

        outcome.address.is_some_and(|address| {
            context.inspect_account(address, false).is_ok_and(|account| account.is_created())
        })
    }
}

impl<CTX, INTR> Inspector<CTX, INTR> for KeylessSandboxEvidenceRecorder
where
    CTX: ContextTr + JournalInspectTr,
    INTR: InterpreterTypes,
    INTR::Stack: StackInspectTr,
{
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        let opcode = interp.bytecode.opcode();
        let address = interp.input.target_address();

        if opcode == opcode::SSTORE {
            if let Some(slot) = interp.stack.inspect::<0>() {
                self.evidence.operations.push(KeylessSandboxEvidenceOp::Sstore { address, slot });
            }
        } else if opcode == opcode::KECCAK256 {
            let Some(offset) = interp.stack.inspect::<0>() else {
                return;
            };
            let Some(size) = interp.stack.inspect::<1>() else {
                return;
            };
            self.pending_keccak = Some(PendingKeccak { address, offset, size });
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        let Some(pending) = self.pending_keccak.take() else {
            return;
        };

        let instruction_failed = interp
            .bytecode
            .action()
            .as_ref()
            .and_then(|action| action.instruction_result())
            .is_some_and(|result| !result.is_ok());
        if instruction_failed {
            return;
        }

        let preimage =
            executed_keccak_preimage(pending.offset, pending.size, interp.memory.size(), |range| {
                Bytes::copy_from_slice(interp.memory.slice(range).as_ref())
            })
            .expect("successful KECCAK256 must expose its executed memory range");
        let hash = keccak256(preimage.as_ref());
        self.evidence.operations.push(KeylessSandboxEvidenceOp::Keccak {
            address: pending.address,
            preimage,
            hash,
        });
    }

    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.push_frame_snapshot();
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.finish_frame(!outcome.result.result.is_ok());
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.push_frame_snapshot();
        None
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let split_create = self.failed_create_survives(context, outcome);
        self.evidence.observed_pre_rex5_split_create |= split_create;
        self.finish_frame(!outcome.result.result.is_ok() && !split_create);
    }
}

fn u256_to_usize(value: U256) -> Option<usize> {
    let limbs = value.as_limbs();
    if (limbs[0] > usize::MAX as u64) || (limbs[1] != 0) || (limbs[2] != 0) || (limbs[3] != 0) {
        return None;
    }
    Some(limbs[0] as usize)
}

fn executed_keccak_preimage(
    offset: U256,
    size: U256,
    memory_size: usize,
    mut copy_memory: impl FnMut(Range<usize>) -> Bytes,
) -> Option<Bytes> {
    let size = u256_to_usize(size)?;
    if size == 0 {
        return Some(Bytes::new());
    }

    let start = u256_to_usize(offset)?;
    let end = start.checked_add(size)?;
    if end > memory_size {
        return None;
    }
    Some(copy_memory(start..end))
}
