//! Init codes that exercise the corners of nested keyless-deploy sandbox execution.
//!
//! Each builder returns raw init code for a keyless deployment whose constructor does
//! something an observer or tracer has to see through: a CREATE that is later rolled back,
//! three levels of nested frames mixed with reverting calls and logs, or a call back into
//! the `KeylessDeploy` system contract from inside the sandbox. The same bytes are used by
//! the in-process tests, the offline state-test corpus, and the end-to-end suite.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::{Address, Bytes};
use revm::bytecode::opcode::{
    CALL, CODECOPY, CREATE, LOG1, MSTORE, MSTORE8, POP, RETURN, REVERT, SSTORE,
};

use super::BytecodeBuilder;
use crate::KEYLESS_DEPLOY_ADDRESS;

/// Init code for the smallest deployable contract: returns a one-byte `STOP` runtime.
///
/// `PUSH1 0, PUSH1 0, MSTORE8, PUSH1 1, PUSH1 0, RETURN`.
pub const STOP_RUNTIME_INIT: [u8; 10] =
    [0x60, 0x00, 0x60, 0x00, 0x53, 0x60, 0x01, 0x60, 0x00, 0xf3];

/// Runtime that always reverts with empty data: `PUSH1 0, PUSH1 0, REVERT`.
pub const REVERTING_RUNTIME: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xfd];

/// The topic the deep-mixed constructor logs under.
pub const DEEP_MIXED_LOG_TOPIC: [u8; 32] = [0xdd; 32];

/// Gas forwarded on every internal CALL these constructors make.
const INTERNAL_CALL_GAS: u16 = 50_000;

/// Emits `CREATE(value = 0, offset = 22, size = 10)` of [`STOP_RUNTIME_INIT`] staged in word 0.
fn create_stop_runtime_child(b: BytecodeBuilder) -> BytecodeBuilder {
    b.push_bytes(STOP_RUNTIME_INIT)
        .push_number(0_u8)
        .append(MSTORE)
        .push_number(STOP_RUNTIME_INIT.len() as u8)
        .push_number(22_u8)
        .push_number(0_u8)
        .append(CREATE)
}

/// Emits `CALL(gas, target, 0, 0, 0, 0, 0)` followed by `POP`.
fn call_and_pop(b: BytecodeBuilder, target: Address) -> BytecodeBuilder {
    b.push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_number(0_u8)
        .push_address(target)
        .push_number(INTERNAL_CALL_GAS)
        .append(CALL)
        .append(POP)
}

/// Emits `MSTORE8(offset, 0); RETURN(offset, 1)`: a one-byte `STOP` runtime.
fn return_stop_runtime(b: BytecodeBuilder, offset: u8) -> BytecodeBuilder {
    b.push_number(0_u8)
        .push_number(offset)
        .append(MSTORE8)
        .push_number(1_u8)
        .push_number(offset)
        .append(RETURN)
}

/// Constructor that CREATEs a child and then REVERTs.
///
/// The child creation succeeds inside the sandbox and is rolled back with the constructor,
/// so the sandbox ends as `ExecutionFailed` while its event stream still carried a nested
/// CREATE.
pub fn revert_after_create_init() -> Bytes {
    let b = create_stop_runtime_child(BytecodeBuilder::default()).append(POP);
    b.push_number(0_u8).push_number(0_u8).append(REVERT).build()
}

/// Constructor with three nesting levels, reverting calls, a log, and a storage write.
///
/// The parent CREATEs a child (stored in slot 0) whose own constructor CREATEs a grandchild
/// and CALLs `reverter`; the parent then CALLs `reverter` itself, emits a `LOG1` under
/// [`DEEP_MIXED_LOG_TOPIC`] with data `deep`, and returns a one-byte `STOP` runtime. The
/// child init code is appended after the parent logic and copied with `CODECOPY`.
pub fn deep_mixed_init(reverter: Address) -> Bytes {
    let child =
        call_and_pop(create_stop_runtime_child(BytecodeBuilder::default()).append(POP), reverter);
    let child = return_stop_runtime(child, 0x40).build_vec();

    let parent_logic = |child_offset: u16| -> Vec<u8> {
        let b = BytecodeBuilder::default()
            // CODECOPY(dest = 0, code_offset = child_offset, size = child.len())
            .push_number(child.len() as u8)
            .push_number(child_offset)
            .push_number(0_u8)
            .append(CODECOPY)
            // CREATE(value = 0, offset = 0, size = child.len()) ; SSTORE(0, child)
            .push_number(child.len() as u8)
            .push_number(0_u8)
            .push_number(0_u8)
            .append(CREATE)
            .push_number(0_u8)
            .append(SSTORE);
        let b = call_and_pop(b, reverter);
        // LOG1(offset = 0x3c, size = 4, topic) with "deep" staged in word 0x20.
        let b = b
            .push_bytes(*b"deep")
            .push_number(0x20_u8)
            .append(MSTORE)
            .push_bytes(DEEP_MIXED_LOG_TOPIC)
            .push_number(4_u8)
            .push_number(0x3c_u8)
            .append(LOG1);
        return_stop_runtime(b, 0x60).build_vec()
    };

    // The child code starts right after the parent logic; the PUSH2 keeps the logic length
    // independent of the offset value.
    let logic_len = parent_logic(0).len();
    let mut code = parent_logic(logic_len as u16);
    debug_assert_eq!(code.len(), logic_len);
    code.extend_from_slice(&child);
    Bytes::from(code)
}

/// Constructor that CALLs the `KeylessDeploy` system contract from inside the sandbox.
///
/// Interception only applies at depth 0, so the call reaches the contract's bytecode and
/// reverts; the constructor ignores the result and deploys a one-byte `STOP` runtime.
pub fn nested_keyless_call_init() -> Bytes {
    let b = call_and_pop(BytecodeBuilder::default(), KEYLESS_DEPLOY_ADDRESS);
    return_stop_runtime(b, 0).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_revert_after_create_init_bytes() {
        let code = revert_after_create_init();
        assert_eq!(&code[..1], &[0x69], "PUSH10 of the child init");
        assert_eq!(&code[1..11], &STOP_RUNTIME_INIT);
        assert_eq!(code.last(), Some(&REVERT));
    }

    #[test]
    fn test_deep_mixed_init_embeds_child_after_logic() {
        let reverter = Address::repeat_byte(0xaa);
        let code = deep_mixed_init(reverter);
        // The child init ends with RETURN and starts with the PUSH10 of the grandchild init.
        assert_eq!(code.last(), Some(&RETURN));
        let child_start = code.len() - code.iter().rev().position(|&b| b == 0x69).unwrap() - 1;
        assert_eq!(&code[child_start + 1..child_start + 11], &STOP_RUNTIME_INIT);
        // CODECOPY reads the child from exactly that offset.
        let offset = u16::from_be_bytes([code[3], code[4]]) as usize;
        assert_eq!(offset, child_start);
        assert_eq!(code[1] as usize, code.len() - child_start, "CODECOPY size is the child length");
    }

    #[test]
    fn test_nested_keyless_call_init_targets_system_contract() {
        let code = nested_keyless_call_init();
        let at = code.windows(20).position(|w| w == KEYLESS_DEPLOY_ADDRESS.as_slice()).unwrap();
        assert_eq!(code[at - 1], 0x73, "PUSH20 of the KeylessDeploy address");
        assert_eq!(code.last(), Some(&RETURN));
    }
}
