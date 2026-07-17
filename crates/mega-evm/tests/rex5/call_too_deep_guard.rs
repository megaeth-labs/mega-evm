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

fn make_call_frame_init(
    target: Address,
    selector: [u8; 4],
    depth: usize,
    scheme: CallScheme,
) -> FrameInit {
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
            scheme,
            is_static: false,
        })),
    }
}

fn assert_call_too_deep(outcome_result: &FrameResult) {
    let FrameResult::Call(outcome) = outcome_result else {
        panic!("expected a Call frame result, got {:?}", outcome_result);
    };
    assert_eq!(
        outcome.result.result,
        InstructionResult::CallTooDeep,
        "depth guard should produce CallTooDeep"
    );
    assert_eq!(outcome.result.gas.spent(), 0, "no gas should be spent on CallTooDeep");
    assert_eq!(
        outcome.result.gas.remaining(),
        GAS_LIMIT,
        "all forwarded gas should be available for caller refund"
    );
}

#[test]
fn test_rex5_depth_guard_returns_call_too_deep_for_system_contract() {
    // REX5: a Call to a system contract at depth > CALL_STACK_LIMIT short-circuits with
    // CallTooDeep before any interceptor runs. The selector targets a real system
    // contract handler (AccessControlInterceptor::disableVolatileDataAccess) so the
    // test would also detect a regression that lets the interceptor fire.
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context);

    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::Call,
    );

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(frame_result) = result else {
        panic!("expected Result variant, got Item");
    };
    assert_call_too_deep(&frame_result);

    // Side-effect check: AccessControlInterceptor would have called disable_access(depth)
    // on the volatile tracker if it had fired. Confirm it did not.
    let ctx = evm.ctx_ref();
    assert!(
        !ctx.volatile_data_tracker.borrow().volatile_access_disabled(CALL_STACK_LIMIT as usize),
        "interceptor must not have fired",
    );
}

#[test]
fn test_rex5_depth_boundary_allows_call_at_limit() {
    // REX5: depth == CALL_STACK_LIMIT is the last permitted depth. The depth guard does
    // not fire and the interceptor should run normally (matches revm's `depth > limit`
    // boundary in `make_call_frame`).
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context);

    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize,
        CallScheme::Call,
    );

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result from interceptor");
    };
    // At depth == CALL_STACK_LIMIT the interceptor still runs and returns its synthetic
    // Return result; the depth guard's CallTooDeep would have aborted execution otherwise.
    assert_eq!(outcome.result.result, InstructionResult::Return);
}

#[test]
fn test_pre_rex5_depth_guard_disabled() {
    // REX4: the depth guard is intentionally absent for backward compatibility — the
    // interceptor still fires at any depth, matching pre-fix behavior. This pins the
    // replay invariant for stable specs.
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX4);
    let mut evm = MegaEvm::new(context);

    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::Call,
    );

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected interceptor Call result");
    };
    // The interceptor still fires under REX4, producing its synthetic Return result.
    assert_eq!(
        outcome.result.result,
        InstructionResult::Return,
        "pre-REX5 must preserve existing behavior (interceptor fires at any depth)",
    );
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
    additional.has_exceeded_limit = LimitCheck::ExceedsLimit {
        kind: LimitKind::KVUpdate,
        limit: 0,
        used: 1,
        frame_local: false,
    };
    context.additional_limit = Rc::new(RefCell::new(additional));

    let mut evm = MegaEvm::new(context);
    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::Call,
    );

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

/// Inspector that unconditionally intercepts every CALL by returning a synthetic
/// success outcome from `call(...)`. Tracks how many times `call` and `call_end`
/// fire so the test can verify the inspector callback pairing is preserved.
#[derive(Default)]
struct AlwaysInterceptInspector {
    call_count: usize,
    call_end_count: usize,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for AlwaysInterceptInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.call_count += 1;
        Some(CallOutcome {
            result: InterpreterResult {
                result: InstructionResult::Stop,
                output: Bytes::new(),
                gas: Gas::new(inputs.gas_limit),
            },
            memory_offset: inputs.return_memory_offset.clone(),
        })
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_end_count += 1;
    }
}

#[test]
fn test_rex5_inspect_frame_init_depth_guard_overrides_inspector() {
    // REX5 inspect mirror: when an inspector returns Some(output) for a call past
    // CALL_STACK_LIMIT, the depth guard must:
    //   1. Override the inspector's success with CallTooDeep
    //   2. Still call `call_end` so the inspector's start/end pairs balance
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let evm = MegaEvm::new(context);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());

    // Target a regular address (not a system contract). The inspector will intercept;
    // without the depth guard, the inspector's synthetic Stop result would survive.
    let target = address!("0000000000000000000000000000000000300001");
    let frame_init =
        make_call_frame_init(target, [0u8; 4], CALL_STACK_LIMIT as usize + 1, CallScheme::Call);

    let result = InspectorEvmTr::inspect_frame_init(&mut evm, frame_init)
        .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };
    // Depth guard wins over inspector synthetic Stop.
    assert_eq!(
        outcome.result.result,
        InstructionResult::CallTooDeep,
        "depth guard must override inspector's synthetic output",
    );
    // call_end must have fired to keep the inspector start/end pair balanced. If a
    // future refactor drops the `frame_end(...)` from the depth-guard early return,
    // this asserts catches it.
    let insp = evm.inspector();
    assert_eq!(insp.call_count, 1, "inspector should see exactly one call_start");
    assert_eq!(insp.call_end_count, 1, "inspector's call_end must be paired");
}

#[test]
fn test_rex5_depth_guard_returns_call_too_deep_for_callcode() {
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context);

    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::CallCode,
    );

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(frame_result) = result else {
        panic!("expected Result variant, got Item");
    };
    assert_call_too_deep(&frame_result);
}

#[test]
fn test_rex5_depth_guard_returns_call_too_deep_for_delegatecall() {
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context);

    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::DelegateCall,
    );

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(frame_result) = result else {
        panic!("expected Result variant, got Item");
    };
    assert_call_too_deep(&frame_result);
}

#[test]
fn test_rex5_depth_guard_returns_call_too_deep_for_staticcall() {
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let mut evm = MegaEvm::new(context);

    let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
    let frame_init = make_call_frame_init(
        ACCESS_CONTROL_ADDRESS,
        selector,
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::StaticCall,
    );

    let result = EvmTr::frame_init(&mut evm, frame_init).expect("frame_init should not error");
    let ItemOrResult::Result(frame_result) = result else {
        panic!("expected Result variant, got Item");
    };
    assert_call_too_deep(&frame_result);
}

#[test]
fn test_rex5_inspect_frame_init_depth_guard_overrides_callcode_inspector() {
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let evm = MegaEvm::new(context);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());

    let target = address!("0000000000000000000000000000000000300002");
    let frame_init =
        make_call_frame_init(target, [0u8; 4], CALL_STACK_LIMIT as usize + 1, CallScheme::CallCode);

    let result = InspectorEvmTr::inspect_frame_init(&mut evm, frame_init)
        .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };
    assert_eq!(
        outcome.result.result,
        InstructionResult::CallTooDeep,
        "depth guard must override inspector output for CALLCODE too",
    );

    let insp = evm.inspector();
    assert_eq!(insp.call_count, 1, "inspector should see exactly one call_start");
    assert_eq!(insp.call_end_count, 1, "inspector's call_end must be paired");
}

#[test]
fn test_rex5_inspect_frame_init_depth_guard_overrides_delegatecall_inspector() {
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let evm = MegaEvm::new(context);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());

    let target = address!("0000000000000000000000000000000000300003");
    let frame_init = make_call_frame_init(
        target,
        [0u8; 4],
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::DelegateCall,
    );

    let result = InspectorEvmTr::inspect_frame_init(&mut evm, frame_init)
        .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };
    assert_eq!(
        outcome.result.result,
        InstructionResult::CallTooDeep,
        "depth guard must override inspector output for DELEGATECALL too",
    );

    let insp = evm.inspector();
    assert_eq!(insp.call_count, 1, "inspector should see exactly one call_start");
    assert_eq!(insp.call_end_count, 1, "inspector's call_end must be paired");
}

#[test]
fn test_rex5_inspect_frame_init_depth_guard_overrides_staticcall_inspector() {
    let mut db = MemoryDatabase::default();
    let context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let evm = MegaEvm::new(context);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());

    let target = address!("0000000000000000000000000000000000300005");
    let frame_init = make_call_frame_init(
        target,
        [0u8; 4],
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::StaticCall,
    );

    let result = InspectorEvmTr::inspect_frame_init(&mut evm, frame_init)
        .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };
    assert_eq!(
        outcome.result.result,
        InstructionResult::CallTooDeep,
        "depth guard must override inspector output for STATICCALL too",
    );

    let insp = evm.inspector();
    assert_eq!(insp.call_count, 1, "inspector should see exactly one call_start");
    assert_eq!(insp.call_end_count, 1, "inspector's call_end must be paired");
}

#[test]
fn test_rex5_inspect_frame_init_exceeded_tx_limit_wins_over_call_too_deep() {
    use mega_evm::{AdditionalLimit, EvmTxRuntimeLimits, LimitCheck, LimitKind};
    use std::{cell::RefCell, rc::Rc};

    let mut db = MemoryDatabase::default();
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
    let mut additional =
        AdditionalLimit::new(MegaSpecId::REX5, EvmTxRuntimeLimits::from_spec(MegaSpecId::REX5));
    additional.has_exceeded_limit = LimitCheck::ExceedsLimit {
        kind: LimitKind::KVUpdate,
        limit: 0,
        used: 1,
        frame_local: false,
    };
    context.additional_limit = Rc::new(RefCell::new(additional));

    let evm = MegaEvm::new(context);
    let mut evm = evm.with_inspector(AlwaysInterceptInspector::default());
    let frame_init = make_call_frame_init(
        address!("0000000000000000000000000000000000300004"),
        [0u8; 4],
        CALL_STACK_LIMIT as usize + 1,
        CallScheme::Call,
    );

    let result = InspectorEvmTr::inspect_frame_init(&mut evm, frame_init)
        .expect("inspect_frame_init should not error");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("expected Call result");
    };
    assert_eq!(
        outcome.result.result,
        InstructionResult::OutOfGas,
        "inspect_frame_init must preserve exceeded-limit priority over CallTooDeep",
    );

    let insp = evm.inspector();
    assert_eq!(insp.call_count, 1, "inspector should see exactly one call_start");
    assert_eq!(insp.call_end_count, 1, "inspector's call_end must be paired");
}
