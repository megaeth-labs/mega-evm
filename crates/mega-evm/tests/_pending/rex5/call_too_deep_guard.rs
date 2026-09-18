//! Tests for the REX5+ `CALL_STACK_LIMIT` depth guard before system contract
//! interceptor dispatch.
//!
//! Pre-REX5: an attacker who could already build the call stack to revm's `CALL_STACK_LIMIT`
//! could still invoke a system contract — the interceptor dispatch in `frame_init` ran
//! *before* revm's own depth check (which only fires inside `make_call_frame`), so the
//! synthetic interceptor result bypassed the depth boundary entirely.
//!
//! Under REX5: a `CALL_STACK_LIMIT` check is inserted between the TX-level
//! exceeded-limit guard and interceptor dispatch. Calls past the limit short-circuit
//! with `CallTooDeep` and never reach the interceptors.
//!
//! These tests drive `EvmTr::frame_init` directly with a synthetic [`FrameInit`] at the
//! depth boundary, since recursing 1025 deep through real bytecode is prohibitively
//! expensive.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::MemoryDatabase, IMegaAccessControl, MegaContext, MegaEvm, MegaSpecId,
    ACCESS_CONTROL_ADDRESS,
};
use revm::{
    context::ContextTr,
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

const CALLER: Address = address!("0000000000000000000000000000000000300010");
const GAS_LIMIT: u64 = 100_000;

fn make_call_frame_init(target: Address, selector: [u8; 4], depth: usize) -> FrameInit {
    FrameInit {
        depth,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(Bytes::copy_from_slice(&selector)),
            return_memory_offset: 0..0,
            gas_limit: GAS_LIMIT,
            bytecode_address: target,
            target_address: target,
            caller: CALLER,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
        })),
    }
}

#[test]
fn test_rex5_exceeded_tx_limit_wins_over_call_too_deep() {
    // Priority-ordering regression: when both a TX-level additional-limit exceed AND
    // a CALL_STACK_LIMIT overflow apply, the exceeded-limit path must win — it carries
    // gas rescue semantics that CallTooDeep does not. `frame_init` and
    // `inspect_frame_init` must run their existing exceeded-limit check BEFORE the
    // new depth guard so the rescue path is preserved.
    use mega_evm::{AdditionalLimit, EvmTxRuntimeLimits, LimitCheck, LimitKind};
    use std::{cell::RefCell, rc::Rc};

    let mut db = MemoryDatabase::default();
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
    // Construct an AdditionalLimit whose `has_exceeded_limit` is pre-set to a TX-level
    // exceed. This mirrors the state produced by intrinsic-overflow detection in
    // `before_tx_start` without depending on its internal API.
    let mut additional =
        AdditionalLimit::new(MegaSpecId::REX5, EvmTxRuntimeLimits::from_spec(MegaSpecId::REX5));
    additional.set_has_exceeded_limit_for_test(LimitCheck::ExceedsLimit {
        kind: LimitKind::KVUpdate,
        limit: 0,
        used: 1,
        frame_local: false,
    });
    context.additional_limit = Rc::new(RefCell::new(additional));

    let mut evm = MegaEvm::new(context);
    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init =
        make_call_frame_init(ACCESS_CONTROL_ADDRESS, selector, CALL_STACK_LIMIT as usize + 1);

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };
    // TX-level exceed produces OutOfGas (halt path), not CallTooDeep. If the depth
    // guard fires first, this assertion fails — CallTooDeep would skip gas rescue.
    assert_eq!(
        outcome.result.result,
        InstructionResult::OutOfGas,
        "exceeded-limit halt must take priority over CallTooDeep depth guard",
    );
}
