//! Destroyed-remainder classification for a frame result's [`InstructionResult`].
//!
//! The conservation law defines a transaction's destroyed total from the envelope. The per-site
//! bookings that cross-check it still have to decide, per result, whether the remaining gas was
//! swallowed (book it) or handed back (book nothing).
//!
//! [`destroyed_disposition`] is the closed table for that: every variant has an explicit arm and
//! there is no `_`, so a revm bump that adds one fails to compile until a human assigns it. It
//! used to be `is_ok_or_revert()` — a catch-all on the halt side, which would have swallowed a new
//! variant without anyone classifying it.
//!
//! # Where the table is read
//!
//! Four sites, all inside [`finalize_frame`](super::AdditionalLimit::finalize_frame):
//! `settle_exceptional_halt_burn`, `settle_frame_init_reject_burn`,
//! `settle_precompile_envelope`, and `settle_inspector_result_gas`. The first three book a
//! destroyed remainder; the fourth books none but answers the same question for an inspector's
//! edit — an edit to a returned result moves what the transaction spends, an edit to a swallowed
//! one does not.
//!
//! A site that has to stay byte-identical with an upstream branch keyed on `is_ok_or_revert()`
//! keeps that predicate instead: the precompile dispatch mirrors an upstream decision rather than
//! stating one on `MegaETH`'s books, so it follows upstream's predicate wherever it goes.
//!
//! # Producers × accounting sites
//!
//! Every producer that can destroy an envelope books at exactly one site below. Completeness of
//! the *reported* total is still the conservation law; this is what the per-site bookings — and a
//! revm-upgrade diff — are checked against.
//!
//! | Producer | Accounting site | Notes |
//! | --- | --- | --- |
//! | Frame-run exceptional halt, including create-return rejects | `finalize_frame` on `FrameExit::Ran` → `settle_exceptional_halt_burn` | [`DestroyedDisposition::Swallow`] |
//! | Frame-init refusal from revm (`make_call_frame` / `make_create_frame` early-fail arms) | `finalize_frame` on `FrameExit::Refused` → `settle_frame_init_reject_burn` | per variant: collision / overflow-payment swallow; depth / funds / empty-code / nonce-overflow return |
//! | Synthetic frame-init refusal (interceptor, inspector intercept, REX5 depth guard) | the same burn, on `FrameExit::RefusedSynthetically` | same classification |
//! | Precompile halt | → `settle_precompile_envelope`, against the staged forwarded envelope and executed work | the staged slot, not `CallOutcome::was_precompile_called`, routes a result here |
//! | `KeylessDeploy` synthetic halt that keeps the call's gas | `sandbox/execution.rs::destroying_oog_frame_result`, which books remaining *before* the result spends the envelope | swallow; `finalize_frame` then sees remaining 0 |
//! | Failed-deposit receipt rewrite | [`AdditionalLimit::settle_rewritten_envelope`](super::AdditionalLimit::settle_rewritten_envelope) | the gap between the rebuilt envelope and the per-site bookings |
//! | Intrinsic pre-frame out-of-gas | `MegaHandler::before_execution` | unreachable on REX7 (REX5+ rejects that transaction in validation) |
//!
//! A new producer belongs on this table with its own site, not as a silent extra call to
//! `record_burned_gas`. A new [`InstructionResult`] variant belongs in [`destroyed_disposition`].
//!
//! # The other closed tables
//!
//! `tests/rex7/gas_surface.rs` closes a perpendicular axis: it enumerates the *carriers* — which
//! field of which object carries gas and which lane books it — while this one enumerates the
//! *endings*. `finalize_frame` composes the two answers.
//!
//! `make_call_frame`, `make_create_frame` and `classify_create_return` each return a result without
//! running a child body on a fixed list of arms. Those arms are not an enum, so a revm bump that
//! adds one does not fail this match; the upgrade checklist diffs them by hand against the list in
//! `tests/rex7/result_space_tripwire.rs`.
//!
//! One live mismatch is load-bearing: a CREATE whose nonce cannot be bumped returns
//! [`InstructionResult::Return`], not [`InstructionResult::NonceOverflow`]. The variant is still
//! classified, so that if an arm ever starts producing it the booking is defined.

use revm::interpreter::InstructionResult;

/// What the destroyed-remainder protocol does with a frame result's remaining gas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestroyedDisposition {
    /// Remaining gas is erased back into the caller. Book nothing.
    Return,
    /// Remaining gas is never handed back. Book it as destroyed.
    Swallow,
    /// Cannot appear as a frame result. Settlement must not observe it.
    Unreachable,
}

impl DestroyedDisposition {
    /// Whether the protocol books this result's remaining gas as destroyed.
    pub const fn swallows(self) -> bool {
        matches!(self, Self::Swallow)
    }
}

/// Classifies `result` for the destroyed-remainder protocol.
///
/// Every [`InstructionResult`] variant has an arm. A new variant is a compile error until it is
/// assigned [`Return`](DestroyedDisposition::Return), [`Swallow`](DestroyedDisposition::Swallow),
/// or [`Unreachable`](DestroyedDisposition::Unreachable).
pub const fn destroyed_disposition(result: InstructionResult) -> DestroyedDisposition {
    match result {
        // Success / revert: the caller gets the remaining gas back.
        InstructionResult::Stop |
        InstructionResult::Return |
        InstructionResult::SelfDestruct |
        InstructionResult::Revert |
        InstructionResult::CallTooDeep |
        InstructionResult::OutOfFunds |
        InstructionResult::CreateInitCodeStartingEF00 |
        InstructionResult::InvalidEOFInitCode |
        InstructionResult::InvalidExtDelegateCallTarget => DestroyedDisposition::Return,

        // Internal interpreter state. Never a `FrameResult`.
        InstructionResult::Suspend => DestroyedDisposition::Unreachable,

        // Exceptional halt: the remaining gas is gone.
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
        InstructionResult::InvalidImmediateEncoding => DestroyedDisposition::Swallow,
    }
}

/// Whether a frame result with this instruction result has remaining gas the protocol books as
/// destroyed.
///
/// Debug builds panic if [`DestroyedDisposition::Unreachable`] appears: that variant is not a
/// frame result, and reaching settlement with it means the classification table is stale.
pub(crate) fn remaining_is_destroyed(result: InstructionResult) -> bool {
    let class = destroyed_disposition(result);
    debug_assert!(
        !matches!(class, DestroyedDisposition::Unreachable),
        "unreachable InstructionResult at a destroyed-remainder settlement: {result:?}"
    );
    class.swallows()
}
