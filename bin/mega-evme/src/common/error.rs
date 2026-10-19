use alloy_primitives::{hex::FromHexError, BlockNumber, TxHash, B256};
use alloy_provider::transport::TransportError;
use mega_evm::{
    alloy_evm::block::BlockExecutionError,
    revm::{bytecode::BytecodeDecodeError, database_interface::bal::EvmDatabaseError},
};

/// Error types for the replay command
#[derive(Debug, thiserror::Error)]
pub enum EvmeError {
    /// RPC transport error
    #[error("RPC transport error: {0}")]
    RpcTransportError(TransportError),

    /// Transaction not found
    #[error("Transaction not found: {0}")]
    TransactionNotFound(TxHash),

    /// Block not found
    #[error("Block not found: {0}")]
    BlockNotFound(BlockNumber),

    /// Block execution error
    #[error("Block execution error: {0}")]
    BlockExecutionError(#[from] BlockExecutionError),

    /// Invalid bytecode
    #[error("Invalid bytecode: {0}")]
    InvalidBytecode(#[from] BytecodeDecodeError),

    /// Failed to read file
    #[error("Failed to read file: {0}")]
    FileRead(#[from] std::io::Error),

    /// Invalid hex string
    #[error("Invalid hex string: {0}")]
    InvalidHex(#[from] FromHexError),

    /// EVM execution error
    #[error("EVM execution error: {0}")]
    ExecutionError(String),

    /// Invalid input
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// RPC error
    #[error("RPC error: {0}")]
    RpcError(String),

    /// Fixture envelope error
    #[error("Fixture error: {0}")]
    FixtureError(String),

    /// Unsupported transaction type
    #[error("Unsupported transaction type: {0}")]
    UnsupportedTxType(u8),

    /// Code hash mismatch
    #[error("Code hash mismatch: expected {expected}, computed {computed}")]
    CodeHashMismatch {
        /// Expected code hash from prestate
        expected: B256,
        /// Computed code hash from bytecode
        computed: B256,
    },

    /// Other error
    #[error("Other error: {0}")]
    Other(String),

    /// A failure the released CLI returned, already rendered the way that CLI prints it on stderr.
    #[error("{0}")]
    ReleasedCli(String),
}

// Implement DBErrorMarker to allow EvmeError to be used as Database error type
impl mega_evm::revm::database::DBErrorMarker for EvmeError {}

impl From<EvmDatabaseError<Self>> for EvmeError {
    fn from(err: EvmDatabaseError<Self>) -> Self {
        match err {
            EvmDatabaseError::Database(e) => e,
            EvmDatabaseError::Bal(e) => Self::Other(format!("BAL error: {e}")),
        }
    }
}

/// Result type for the mega-evme command
pub type Result<T> = std::result::Result<T, EvmeError>;

/// Drops C0 controls and ANSI escape sequences from `text`.
///
/// A revert reason or an error is printed as text. Those bytes would move the cursor or restyle
/// the terminal.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_ansi(&mut chars);
            continue;
        }
        if c <= '\u{1f}' || c == '\u{7f}' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Consumes one ANSI sequence whose ESC was already read.
fn skip_ansi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(next) = chars.next() else { return };
    match next {
        // CSI: ESC [ parameters and intermediates, then one final byte.
        '[' => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        // OSC: ESC ] ... BEL, or ESC ] ... ST (ESC \).
        ']' => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_terminal_text;

    #[test]
    fn test_terminal_text_drops_controls_and_ansi() {
        assert_eq!(sanitize_terminal_text("café"), "café");
        assert_eq!(sanitize_terminal_text("owned\r\nit"), "ownedit");
        assert_eq!(sanitize_terminal_text("a\u{7f}b"), "ab");
        // Clear screen and cursor home.
        assert_eq!(sanitize_terminal_text("\u{1b}[2J\u{1b}[Hleft"), "left");
        // An OSC title sequence, ended by BEL.
        assert_eq!(sanitize_terminal_text("\u{1b}]0;title\u{7}left"), "left");
        // A lone ESC and a one-byte escape drop with their follower.
        assert_eq!(sanitize_terminal_text("\u{1b}"), "");
        assert_eq!(sanitize_terminal_text("\u{1b}Aleft"), "left");
    }
}
