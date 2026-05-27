/// Custom error type for t8n operations
#[derive(Debug, thiserror::Error)]
pub(crate) enum T8nError {
    /// Failed to load an input file
    #[error("Failed to load input file '{file}': {source}")]
    InputLoad {
        /// The file path that failed to load
        file: String,
        /// The underlying I/O error
        source: std::io::Error,
    },

    /// Failed to parse JSON content
    #[error("Failed to parse JSON from '{file}': {source}")]
    JsonParse {
        /// The file path where JSON parsing failed
        file: String,
        /// The underlying JSON parsing error
        source: serde_json::Error,
    },

    /// Failed to write an output file
    #[error("Failed to write output file '{file}': {source}")]
    OutputWrite {
        /// The file path that failed to write
        file: String,
        /// The underlying I/O error
        source: std::io::Error,
    },

    /// Invalid transaction data provided
    #[error("Invalid transaction data: {0}")]
    InvalidTransaction(String),
}

/// Result type alias for T8N operations
pub(crate) type Result<T> = std::result::Result<T, T8nError>;
