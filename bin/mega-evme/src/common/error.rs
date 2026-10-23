use alloy_primitives::{hex::FromHexError, BlockNumber, TxHash, B256};
use mega_evm::revm::{bytecode::BytecodeDecodeError, database_interface::bal::EvmDatabaseError};

/// Error types for the replay command
#[derive(Debug, thiserror::Error)]
pub enum EvmeError {
    /// Transaction not found
    #[error("Transaction not found: {0}")]
    TransactionNotFound(TxHash),

    /// Block not found
    #[error("Block not found: {0}")]
    BlockNotFound(BlockNumber),

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

/// Drops what would move the cursor or restyle the terminal from `text`, and keeps the rest.
///
/// A revert reason or an error is printed as text, and its bytes are not the tool's. Removed: ANSI
/// escape sequences, the C0 controls but `\n` and `\t`, DEL, the C1 controls (U+0080 to U+009F,
/// which include the one-character CSI, OSC and DCS introducers) and the bidirectional overrides
/// and isolates (U+202A to U+202E, U+2066 to U+2069), which make a line read otherwise than its
/// bytes. Line breaks and tabs stay, so a multi-line message keeps its lines.
///
/// This is for text a human reads. A JSON value carries the reason's characters escaped and needs
/// no filtering.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_ansi(&mut chars);
            continue;
        }
        let bidi = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if (c.is_control() && c != '\n' && c != '\t') || bidi {
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
        assert_eq!(sanitize_terminal_text("a\u{7f}b"), "ab");
        // Clear screen and cursor home.
        assert_eq!(sanitize_terminal_text("\u{1b}[2J\u{1b}[Hleft"), "left");
        // An OSC title sequence, ended by BEL.
        assert_eq!(sanitize_terminal_text("\u{1b}]0;title\u{7}left"), "left");
        // A lone ESC and a one-byte escape drop with their follower.
        assert_eq!(sanitize_terminal_text("\u{1b}"), "");
        assert_eq!(sanitize_terminal_text("\u{1b}Aleft"), "left");
    }

    #[test]
    fn test_terminal_text_keeps_lines_and_tabs() {
        assert_eq!(sanitize_terminal_text("line one\nline two"), "line one\nline two");
        assert_eq!(sanitize_terminal_text("a\tb"), "a\tb");
        // A carriage return can overwrite what the line printed before it.
        assert_eq!(sanitize_terminal_text("owned\r\nit"), "owned\nit");
    }

    #[test]
    fn test_terminal_text_drops_c1_controls_and_bidi_overrides() {
        // The one-character CSI and OSC introducers, as UTF-8 text.
        assert_eq!(sanitize_terminal_text("\u{9b}2J\u{9b}Hcleared"), "2JHcleared");
        assert_eq!(sanitize_terminal_text("\u{9d}0;title\u{9c}x"), "0;titlex");
        assert!((0x80..=0x9f)
            .all(|c| sanitize_terminal_text(&char::from_u32(c).unwrap().to_string()).is_empty()));
        assert_eq!(sanitize_terminal_text("\u{202e}gnp.exe"), "gnp.exe");
        assert_eq!(sanitize_terminal_text("\u{2066}x\u{2069}"), "x");
        // Letters beside the ranges stay.
        assert_eq!(sanitize_terminal_text("\u{a0}\u{a1}\u{2028}"), "\u{a0}\u{a1}\u{2028}");
    }
}
