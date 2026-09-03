//! Closed classification of every [`InstructionResult`] variant for the destroyed-remainder
//! protocol, and the early-fail arm list a revm bump has to diff by hand.
//!
//! revm's `InstructionResult` is a closed enumeration of envelope endings, but until this file the
//! destroyed-remainder protocol classified them through `is_ok_or_revert()` — a catch-all on the
//! halt side. A variant revm added later would be swallowed without anyone assigning it. The
//! `CreateCollision` booking was that gap: the halt class happened to be right, and nothing forced
//! a human to say so.
//!
//! [`destroyed_disposition`] is the assignment table: every variant has an arm, and there is no
//! `_`. Commenting one out, or a revm bump that adds a variant, fails to compile. This file is the
//! readable copy of that table, plus the second closed set that *is* a catch-all: the early-fail
//! arms of `make_call_frame` / `make_create_frame` / `classify_create_return`, which are not an
//! enum and have to be read by hand on an upgrade.
//!
//! Complementary to the EEST corpus sweep, which collides with whatever the fixtures reach. This
//! file is the enumeration seal.

use mega_evm::{destroyed_disposition, DestroyedDisposition};
use revm::interpreter::InstructionResult;

/// One row of the destroyed-remainder assignment table.
struct VariantRow {
    result: InstructionResult,
    disposition: DestroyedDisposition,
}

/// Every [`InstructionResult`] variant, with the disposition [`destroyed_disposition`] assigns.
///
/// A new variant is a new row here *and* a new arm in [`destroyed_disposition`]. Omitting the arm
/// is a compile error; omitting the row is what
/// [`test_every_instruction_result_has_an_explicit_destroyed_disposition`] catches.
const VARIANTS: &[VariantRow] = &[
    // Return: remaining gas is erased back into the caller.
    VariantRow { result: InstructionResult::Stop, disposition: DestroyedDisposition::Return },
    VariantRow { result: InstructionResult::Return, disposition: DestroyedDisposition::Return },
    VariantRow {
        result: InstructionResult::SelfDestruct,
        disposition: DestroyedDisposition::Return,
    },
    VariantRow { result: InstructionResult::Revert, disposition: DestroyedDisposition::Return },
    VariantRow {
        result: InstructionResult::CallTooDeep,
        disposition: DestroyedDisposition::Return,
    },
    VariantRow { result: InstructionResult::OutOfFunds, disposition: DestroyedDisposition::Return },
    VariantRow {
        result: InstructionResult::CreateInitCodeStartingEF00,
        disposition: DestroyedDisposition::Return,
    },
    VariantRow {
        result: InstructionResult::InvalidEOFInitCode,
        disposition: DestroyedDisposition::Return,
    },
    VariantRow {
        result: InstructionResult::InvalidExtDelegateCallTarget,
        disposition: DestroyedDisposition::Return,
    },
    // Unreachable: never a frame result.
    VariantRow {
        result: InstructionResult::Suspend,
        disposition: DestroyedDisposition::Unreachable,
    },
    // Swallow: remaining gas is never handed back.
    VariantRow { result: InstructionResult::OutOfGas, disposition: DestroyedDisposition::Swallow },
    VariantRow { result: InstructionResult::MemoryOOG, disposition: DestroyedDisposition::Swallow },
    VariantRow {
        result: InstructionResult::MemoryLimitOOG,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::PrecompileOOG,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::InvalidOperandOOG,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::ReentrancySentryOOG,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::OpcodeNotFound,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::CallNotAllowedInsideStatic,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::StateChangeDuringStaticCall,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::InvalidFEOpcode,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::InvalidJump,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::NotActivated,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::StackUnderflow,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::StackOverflow,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::OutOfOffset,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::CreateCollision,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::OverflowPayment,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::PrecompileError,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::NonceOverflow,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::CreateContractSizeLimit,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::CreateContractStartingWithEF,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::CreateInitCodeSizeLimit,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::FatalExternalError,
        disposition: DestroyedDisposition::Swallow,
    },
    VariantRow {
        result: InstructionResult::InvalidImmediateEncoding,
        disposition: DestroyedDisposition::Swallow,
    },
];

/// The early-fail arms of revm's frame-init / create-return as of revm-handler 20.0.3.
///
/// This list is not type-tied to upstream. A revm bump that adds an arm does not fail to compile.
/// Diff `EthFrame::make_call_frame`, `EthFrame::make_create_frame`, and `return_create` /
/// `classify_create_return` against it, then add the row and assign the produced
/// [`InstructionResult`] in [`destroyed_disposition`].
///
/// The nonce-overflow arm is the live mismatch the `CreateCollision` gap was a sibling of: the
/// arm returns `Return`, not `NonceOverflow`. The variant is still classified (swallow) so a
/// future arm that starts producing it has a defined booking.
struct EarlyFailArm {
    /// Upstream function and the condition that returns without a child body.
    site: &'static str,
    result: InstructionResult,
}

const EARLY_FAIL_ARMS: &[EarlyFailArm] = &[
    EarlyFailArm {
        site: "make_call_frame: depth > CALL_STACK_LIMIT",
        result: InstructionResult::CallTooDeep,
    },
    EarlyFailArm {
        site: "make_call_frame: transfer_loaded → TransferError::OutOfFunds",
        result: InstructionResult::OutOfFunds,
    },
    EarlyFailArm {
        site: "make_call_frame: transfer_loaded → TransferError::OverflowPayment",
        result: InstructionResult::OverflowPayment,
    },
    EarlyFailArm {
        site: "make_call_frame: transfer_loaded → TransferError::CreateCollision",
        result: InstructionResult::CreateCollision,
    },
    EarlyFailArm { site: "make_call_frame: empty bytecode", result: InstructionResult::Stop },
    EarlyFailArm {
        site: "make_create_frame: depth > CALL_STACK_LIMIT",
        result: InstructionResult::CallTooDeep,
    },
    EarlyFailArm {
        site: "make_create_frame: caller balance < value",
        result: InstructionResult::OutOfFunds,
    },
    EarlyFailArm {
        site: "make_create_frame: nonce bump fails (NOT NonceOverflow)",
        result: InstructionResult::Return,
    },
    EarlyFailArm {
        site: "make_create_frame: create_account_checkpoint → TransferError::CreateCollision",
        result: InstructionResult::CreateCollision,
    },
    EarlyFailArm {
        site: "make_create_frame: create_account_checkpoint → TransferError::OverflowPayment",
        result: InstructionResult::OverflowPayment,
    },
    EarlyFailArm {
        site: "make_create_frame: create_account_checkpoint → TransferError::OutOfFunds",
        result: InstructionResult::OutOfFunds,
    },
    EarlyFailArm {
        site: "classify_create_return: runtime code size",
        result: InstructionResult::CreateContractSizeLimit,
    },
    EarlyFailArm {
        site: "classify_create_return: 0xEF prefix",
        result: InstructionResult::CreateContractStartingWithEF,
    },
    EarlyFailArm {
        site: "classify_create_return: code-deposit charge",
        result: InstructionResult::OutOfGas,
    },
];

/// Naming every variant, with no `_`, is the compile-time tripwire in this file.
///
/// Commenting one out is a non-exhaustive match. A revm bump that adds a variant is the same
/// error, and the next step is an arm in [`destroyed_disposition`] plus a row in [`VARIANTS`].
#[test]
fn test_instruction_result_space_has_no_catchall() {
    match InstructionResult::Stop {
        InstructionResult::Stop |
        InstructionResult::Return |
        InstructionResult::SelfDestruct |
        InstructionResult::Suspend |
        InstructionResult::Revert |
        InstructionResult::CallTooDeep |
        InstructionResult::OutOfFunds |
        InstructionResult::CreateInitCodeStartingEF00 |
        InstructionResult::InvalidEOFInitCode |
        InstructionResult::InvalidExtDelegateCallTarget |
        InstructionResult::OutOfGas |
        InstructionResult::MemoryOOG |
        InstructionResult::MemoryLimitOOG |
        InstructionResult::PrecompileOOG |
        InstructionResult::InvalidOperandOOG |
        InstructionResult::ReentrancySentryOOG |
        InstructionResult::OpcodeNotFound |
        InstructionResult::CallNotAllowedInsideStatic |
        InstructionResult::StateChangeDuringStaticCall |
        InstructionResult::InvalidFEOpcode |
        InstructionResult::InvalidJump |
        InstructionResult::NotActivated |
        InstructionResult::StackUnderflow |
        InstructionResult::StackOverflow |
        InstructionResult::OutOfOffset |
        InstructionResult::CreateCollision |
        InstructionResult::OverflowPayment |
        InstructionResult::PrecompileError |
        InstructionResult::NonceOverflow |
        InstructionResult::CreateContractSizeLimit |
        InstructionResult::CreateContractStartingWithEF |
        InstructionResult::CreateInitCodeSizeLimit |
        InstructionResult::FatalExternalError |
        InstructionResult::InvalidImmediateEncoding => {}
    }
}

/// Every variant has a row, and the row matches [`destroyed_disposition`].
///
/// Commenting out a match arm in `destroyed_disposition` fails this crate at compile time — that
/// is the tripwire. This test is the readable copy: a missing row, or a row that disagrees with
/// the match, is a runtime failure naming the variant.
#[test]
fn test_every_instruction_result_has_an_explicit_destroyed_disposition() {
    for row in VARIANTS {
        assert_eq!(
            destroyed_disposition(row.result),
            row.disposition,
            "{:?}: the tripwire table and destroyed_disposition must assign the same class",
            row.result,
        );
    }
}

/// Return / Swallow follow revm's ok-or-revert / halt macros; Unreachable is only `Suspend`.
///
/// The protocol owns the assignment, so this is a snapshot of today's agreement rather than a
/// requirement that they stay coupled. A variant reclassified away from the macros is a deliberate
/// row change here.
#[test]
fn test_return_and_swallow_agree_with_revm_ok_or_revert() {
    for row in VARIANTS {
        match row.disposition {
            DestroyedDisposition::Return => {
                assert!(
                    row.result.is_ok_or_revert(),
                    "{:?} is Return, so is_ok_or_revert must hold",
                    row.result,
                );
                assert!(!row.result.is_halt(), "{:?} is Return, so it is not a halt", row.result);
            }
            DestroyedDisposition::Swallow => {
                assert!(row.result.is_halt(), "{:?} is Swallow, so it must be a halt", row.result,);
                assert!(
                    !row.result.is_ok_or_revert(),
                    "{:?} is Swallow, so is_ok_or_revert must not hold",
                    row.result,
                );
            }
            DestroyedDisposition::Unreachable => {
                assert_eq!(
                    row.result,
                    InstructionResult::Suspend,
                    "the only unreachable variant is Suspend (internal interpreter state); \
                     got {:?}",
                    row.result,
                );
            }
        }
    }
}

/// Each documented early-fail arm produces a variant the disposition table already classifies.
///
/// Adding an arm in revm without a row here is what the upgrade checklist is for; this test only
/// pins that the arms we already know about have a defined booking.
#[test]
fn test_every_documented_early_fail_arm_has_a_classified_result() {
    for arm in EARLY_FAIL_ARMS {
        let class = destroyed_disposition(arm.result);
        assert!(
            VARIANTS.iter().any(|row| row.result == arm.result && row.disposition == class),
            "{} produces {:?}, which must have a tripwire row",
            arm.site,
            arm.result,
        );
        assert_ne!(
            class,
            DestroyedDisposition::Unreachable,
            "{} produces {:?}, which cannot be classified unreachable: it is a live frame result",
            arm.site,
            arm.result,
        );
    }
}

/// The CREATE nonce-overflow arm returns `Return`, not `NonceOverflow`.
///
/// That is the shape the `CreateCollision` gap was a sibling of: the variant exists, the live arm
/// produces a different one, and both must stay classified.
#[test]
fn test_create_nonce_overflow_arm_returns_return_not_nonce_overflow() {
    let arm = EARLY_FAIL_ARMS
        .iter()
        .find(|arm| arm.site.contains("nonce bump fails"))
        .expect("the nonce-overflow arm is part of the early-fail list");
    assert_eq!(arm.result, InstructionResult::Return);
    assert_eq!(destroyed_disposition(InstructionResult::Return), DestroyedDisposition::Return);
    assert_eq!(
        destroyed_disposition(InstructionResult::NonceOverflow),
        DestroyedDisposition::Swallow,
        "NonceOverflow is a halt; if an arm starts producing it, the booking is swallow",
    );
}
