//! Central exit-code taxonomy for the `mega-evme` CLI.
//!
//! Verification pipelines branch on the process status, so every failure the
//! CLI can reach maps onto exactly one documented code, and every exit flows
//! through this module: the binary hands its top-level result to
//! [`report_command_result`] and returns the code it produces. No other code
//! path calls `std::process::exit`.
//!
//! | Code | Class                   | Meaning                                                                                                                                              |
//! | ---- | ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
//! | `0`  | success                 | The command completed; with `--verify-receipt`, every verification matched.                                                                          |
//! | `1`  | `execution-error`       | Execution or internal error: an EVM/setup failure, bad input, or a definitive negative answer such as an unknown transaction or block.                |
//! | `2`  | `verification-mismatch` | The run completed, but at least one replay did not reproduce its on-chain receipt.                                                                    |
//! | `3`  | `rpc-failure`           | An RPC/transport call failed (endpoint unreachable, transport error, offline replay cache miss): the question went unanswered rather than answered no. |
//!
//! A batch run reports every target individually and then fails once with the
//! counts by failure class ([`BatchFailureCounts`]), which
//! [`ExitCode::from_batch_failures`] resolves by precedence: any
//! execution/internal failure yields `1`, else any RPC failure yields `3`, else
//! any mismatch yields `2`.
//!
//! Extension rule: a new failure class gets a new discriminant. The meaning of
//! an existing code never changes and a retired code is never reused, because
//! callers pin these numbers in scripts. The mapping matches the error enums
//! exhaustively, so a new error variant does not compile until it has been
//! assigned a class.

use serde::Serialize;
use tracing::error;

use crate::{
    cmd::Error,
    common::{BatchFailureCounts, EvmeError},
};

/// Process exit status of a `mega-evme` run.
///
/// The discriminants are the wire contract with calling scripts; see the module
/// documentation for the taxonomy and the rule for extending it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    /// The command completed and every check it ran passed.
    Success = 0,
    /// Execution or internal error, bad input, or a definitive negative answer.
    ExecutionError = 1,
    /// The run completed but at least one receipt verification mismatched.
    VerificationMismatch = 2,
    /// An RPC or transport call failed, so the question went unanswered.
    RpcFailure = 3,
}

impl ExitCode {
    /// The numeric status this class exits with.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Kebab-case name of the class, used as the `kind` of the structured error
    /// object printed in `--json` mode.
    ///
    /// This namespace describes the run as a whole and is distinct from the
    /// per-target `kind` on a batch NDJSON error line (`not_found`, `pending`,
    /// `rpc`, `execution`), which reports why one target failed.
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ExecutionError => "execution-error",
            Self::VerificationMismatch => "verification-mismatch",
            Self::RpcFailure => "rpc-failure",
        }
    }

    /// Map a top-level command error onto its class.
    pub fn from_command_error(err: &Error) -> Self {
        match err {
            Error::Custom(_) => Self::ExecutionError,
            Error::Evme(err) => Self::from_evme_error(err),
        }
    }

    /// Map a command failure onto its class.
    ///
    /// The match is exhaustive on purpose: a new [`EvmeError`] variant must
    /// pick a class here rather than inherit one by default.
    pub fn from_evme_error(err: &EvmeError) -> Self {
        match err {
            // The endpoint never answered: unreachable, transport-level
            // failure, or an offline replay file without the response.
            EvmeError::RpcTransportError(_) | EvmeError::RpcError(_) => Self::RpcFailure,
            // Answered, definitively negative.
            EvmeError::TransactionNotFound(_) |
            EvmeError::BlockNotFound(_) |
            // Execution, setup, input, and internal failures.
            EvmeError::BlockExecutionError(_) |
            EvmeError::InvalidBytecode(_) |
            EvmeError::FileRead(_) |
            EvmeError::InvalidHex(_) |
            EvmeError::ExecutionError(_) |
            EvmeError::InvalidInput(_) |
            EvmeError::FixtureError(_) |
            EvmeError::UnsupportedTxType(_) |
            EvmeError::CodeHashMismatch { .. } |
            EvmeError::Other(_) => Self::ExecutionError,
            // The run completed; the replay diverged from the chain.
            EvmeError::VerificationMismatch { .. } => Self::VerificationMismatch,
            EvmeError::BatchFailed(counts) => Self::from_batch_failures(counts),
        }
    }

    /// Resolve the class of a batch run from its failure counts.
    ///
    /// Precedence: an execution/internal failure outranks an RPC failure, which
    /// outranks a mismatch. A target that never replayed was also never
    /// verified, so reporting such a run as a mismatch would overstate what it
    /// found.
    pub const fn from_batch_failures(counts: &BatchFailureCounts) -> Self {
        if counts.execution > 0 {
            Self::ExecutionError
        } else if counts.rpc > 0 {
            Self::RpcFailure
        } else if counts.mismatched > 0 {
            Self::VerificationMismatch
        } else {
            Self::Success
        }
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        Self::from(code.code())
    }
}

/// The structured failure object `--json` runs print as their last stdout line.
#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

/// Payload of an [`ErrorEnvelope`].
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    /// The process exit code this failure produces.
    code: u8,
    /// Kebab-case failure class, see [`ExitCode::kind`].
    kind: &'static str,
    /// The error's `Display` text, never its `Debug` form.
    message: &'a str,
}

/// Report a finished command and return the code the process exits with.
///
/// A successful run prints nothing. A failure is reported exactly once on
/// stderr as `error: <message>`, always `Display`-formatted (the message itself
/// may carry continuation lines, such as an RPC error's re-capture hint), and
/// in `--json` mode additionally as the structured object on stdout — the last
/// line, so a machine-readable run never ends with empty stdout, and in batch
/// mode the object follows the per-target lines. The `tracing` event carries
/// the same text and stays silent unless `-v` was given.
pub fn report_command_result(result: Result<(), Error>, json: bool) -> ExitCode {
    let Err(err) = result else {
        return ExitCode::Success;
    };

    let code = ExitCode::from_command_error(&err);
    error!(err = %err, exit_code = code.code(), "Command failed");

    let message = err.to_string();
    eprintln!("error: {message}");
    if json {
        let envelope = ErrorEnvelope {
            error: ErrorBody { code: code.code(), kind: code.kind(), message: &message },
        };
        println!("{}", serde_json::to_string(&envelope).expect("failed to serialize the error"));
    }

    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    /// Every failure class the taxonomy defines keeps its documented code.
    #[test]
    fn test_exit_codes_are_stable() {
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::ExecutionError.code(), 1);
        assert_eq!(ExitCode::VerificationMismatch.code(), 2);
        assert_eq!(ExitCode::RpcFailure.code(), 3);
    }

    /// The `kind` namespace is kebab-case and one name per class.
    #[test]
    fn test_exit_code_kinds_are_kebab_case() {
        for code in [
            ExitCode::Success,
            ExitCode::ExecutionError,
            ExitCode::VerificationMismatch,
            ExitCode::RpcFailure,
        ] {
            let kind = code.kind();
            assert!(
                kind.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "kind must be kebab-case, got: {kind}"
            );
        }
    }

    /// An unanswered question is an RPC failure, whatever shape it arrived in.
    #[test]
    fn test_rpc_class_errors_map_to_three() {
        for err in [
            EvmeError::RpcError("cache miss in offline replay file".to_string()),
            EvmeError::RpcTransportError(
                alloy_provider::transport::TransportErrorKind::custom_str("connection refused"),
            ),
        ] {
            assert_eq!(
                ExitCode::from_evme_error(&err),
                ExitCode::RpcFailure,
                "unexpected class for {err}"
            );
        }
    }

    /// Definitive answers, bad input, and internal failures all exit 1.
    #[test]
    fn test_execution_class_errors_map_to_one() {
        for err in [
            EvmeError::TransactionNotFound(B256::ZERO),
            EvmeError::BlockNotFound(7),
            EvmeError::ExecutionError("halted".to_string()),
            EvmeError::InvalidInput("no transaction hashes".to_string()),
            EvmeError::FixtureError("unsupported transaction".to_string()),
            EvmeError::UnsupportedTxType(0x7e),
            EvmeError::CodeHashMismatch { expected: B256::ZERO, computed: B256::ZERO },
            EvmeError::FileRead(std::io::Error::other("boom")),
            EvmeError::InvalidHex(alloy_primitives::hex::FromHexError::OddLength),
            EvmeError::Other("something else".to_string()),
        ] {
            assert_eq!(
                ExitCode::from_evme_error(&err),
                ExitCode::ExecutionError,
                "unexpected class for {err}"
            );
        }
    }

    /// A completed run whose replay diverged from the chain exits 2.
    #[test]
    fn test_verification_mismatch_maps_to_two() {
        let err = EvmeError::VerificationMismatch { mismatched: 1, total: 3 };
        assert_eq!(ExitCode::from_evme_error(&err), ExitCode::VerificationMismatch);
    }

    /// The top-level wrapper adds no class of its own beyond the internal one.
    #[test]
    fn test_command_error_classes() {
        assert_eq!(
            ExitCode::from_command_error(&Error::Custom("bad state")),
            ExitCode::ExecutionError
        );
        assert_eq!(
            ExitCode::from_command_error(&Error::Evme(EvmeError::RpcError("down".to_string()))),
            ExitCode::RpcFailure
        );
    }

    /// Batch precedence: execution beats rpc beats mismatch.
    #[test]
    fn test_batch_failure_precedence() {
        let mixed = BatchFailureCounts { execution: 1, rpc: 2, mismatched: 3, total: 6 };
        assert_eq!(ExitCode::from_batch_failures(&mixed), ExitCode::ExecutionError);

        let rpc_only = BatchFailureCounts { execution: 0, rpc: 2, mismatched: 3, total: 6 };
        assert_eq!(ExitCode::from_batch_failures(&rpc_only), ExitCode::RpcFailure);

        let mismatch_only = BatchFailureCounts { execution: 0, rpc: 0, mismatched: 3, total: 6 };
        assert_eq!(ExitCode::from_batch_failures(&mismatch_only), ExitCode::VerificationMismatch);

        let clean = BatchFailureCounts::default();
        assert_eq!(ExitCode::from_batch_failures(&clean), ExitCode::Success);
    }

    /// The aggregate error carries its counts through the top-level mapping.
    #[test]
    fn test_batch_failed_error_maps_by_counts() {
        let rpc_only = EvmeError::BatchFailed(BatchFailureCounts {
            execution: 0,
            rpc: 1,
            ..Default::default()
        });
        assert_eq!(ExitCode::from_evme_error(&rpc_only), ExitCode::RpcFailure);

        let with_execution = EvmeError::BatchFailed(BatchFailureCounts {
            execution: 1,
            rpc: 1,
            mismatched: 1,
            total: 3,
        });
        assert_eq!(ExitCode::from_evme_error(&with_execution), ExitCode::ExecutionError);
    }

    /// The serialized object is a single compact line with the documented shape.
    #[test]
    fn test_error_envelope_shape() {
        let code = ExitCode::RpcFailure;
        let envelope = ErrorEnvelope {
            error: ErrorBody { code: code.code(), kind: code.kind(), message: "endpoint down" },
        };
        let line = serde_json::to_string(&envelope).expect("serialize");

        assert!(!line.contains('\n'), "the object must be a single line: {line}");
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(
            value,
            serde_json::json!({
                "error": { "code": 3, "kind": "rpc-failure", "message": "endpoint down" }
            })
        );
    }
}
