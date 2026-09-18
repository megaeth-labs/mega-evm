//! Frame results `MegaETH` builds without running a frame carry the reservoir the frame
//! inherited and settle like revm's own.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{test_utils::MemoryDatabase, MegaEvm};
use revm::{
    context::{ContextTr, JournalTr},
    handler::{EvmTr, FrameResult, ItemOrResult},
    inspector::InspectorEvmTr,
    interpreter::{
        interpreter::SharedMemory, interpreter_action::FrameInit,
        interpreter_types::InterpreterTypes, CallInput, CallInputs, CallOutcome, CallScheme,
        CallValue, FrameInput, Gas, InstructionResult, InterpreterResult,
    },
    primitives::CALL_STACK_LIMIT,
    Inspector,
};

use crate::common::context;

const CALLER: Address = address!("0000000000000000000000000000000000300010");
const TARGET: Address = address!("0000000000000000000000000000000000300001");
const GAS_LIMIT: u64 = 100_000;
const RESERVOIR: u64 = 7_000_000;

fn call_frame_init(depth: usize) -> FrameInit {
    FrameInit {
        depth,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: GAS_LIMIT,
            bytecode_address: TARGET,
            known_bytecode: Default::default(),
            target_address: TARGET,
            caller: CALLER,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            reservoir: RESERVOIR,
            charged_new_account_state_gas: false,
        })),
    }
}

fn assert_call_too_deep(result: &FrameResult) {
    let FrameResult::Call(outcome) = result else { panic!("expected a call result: {result:?}") };
    assert_eq!(outcome.result.result, InstructionResult::CallTooDeep);
    assert_eq!(outcome.result.gas.remaining(), GAS_LIMIT, "the forwarded gas is untouched");
    assert_eq!(outcome.result.gas.reservoir(), RESERVOIR, "the inherited reservoir is carried");
    assert_eq!(outcome.result.gas.state_gas_spent(), 0);
}

/// A call past the call-stack limit is answered with `CallTooDeep` before anything could
/// intercept it, with the forwarded gas untouched and the inherited reservoir carried.
#[test]
fn test_depth_guard_returns_call_too_deep_with_the_inherited_reservoir() {
    let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
    let result = EvmTr::frame_init(&mut evm, call_frame_init(CALL_STACK_LIMIT as usize + 1))
        .expect("frame_init does not fail");
    let ItemOrResult::Result(result) = result else { panic!("no frame is built past the limit") };
    assert_call_too_deep(&result);
}

/// `CALL_STACK_LIMIT` itself is the last permitted depth: the guard does not fire there.
#[test]
fn test_depth_boundary_allows_call_at_limit() {
    let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
    let journal = evm.ctx_mut().journal_mut();
    journal.load_account(CALLER).unwrap();
    journal.load_account(TARGET).unwrap();
    let result = EvmTr::frame_init(&mut evm, call_frame_init(CALL_STACK_LIMIT as usize))
        .expect("frame_init does not fail");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("a call to an account without code is answered at once");
    };
    assert_eq!(outcome.result.result, InstructionResult::Stop, "the call went through");
    assert_eq!(outcome.result.gas.reservoir(), RESERVOIR);
}

/// Answers every call itself, and counts the calls and their ends.
#[derive(Default)]
struct AlwaysAnswers {
    calls: usize,
    call_ends: usize,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for AlwaysAnswers {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls += 1;
        Some(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Stop,
                Bytes::new(),
                Gas::new(inputs.gas_limit),
            ),
            inputs.return_memory_offset.clone(),
        ))
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends += 1;
    }
}

/// An inspector's answer to a call past the limit gives way to `CallTooDeep`, and the inspector
/// still sees the call end.
#[test]
fn test_inspect_frame_init_depth_guard_overrides_inspector() {
    let mut evm =
        MegaEvm::new(context(MemoryDatabase::default())).with_inspector(AlwaysAnswers::default());
    let result = InspectorEvmTr::inspect_frame_init(
        &mut evm,
        call_frame_init(CALL_STACK_LIMIT as usize + 1),
    )
    .expect("inspect_frame_init does not fail");
    let ItemOrResult::Result(result) = result else { panic!("the frame was answered") };
    assert_call_too_deep(&result);
    assert_eq!(
        (evm.inspector().calls, evm.inspector().call_ends),
        (1, 1),
        "call and call_end pair"
    );
}
