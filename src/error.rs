use std::fmt;

/// Stable error codes for the CLI.
///
/// Each variant has a numeric code that is stable across versions.
/// This enables machine-parseable error handling for callers
/// (e.g., the MCP server, scripts, or daemon consumers).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Unspecified or unknown error.
    Unspecified = 10,
    /// Failed to resolve Chrome WebSocket endpoint.
    ChromeEndpointResolution = 1,
    /// Failed to connect to Chrome DevTools Protocol.
    ChromeConnection = 2,
    /// Chrome target (page) not found.
    TargetNotFound = 3,
    /// Command execution failed on the page.
    CommandExecution = 4,
    /// Failed to write to or read from a file.
    FileIo = 5,
    /// Failed to spawn or connect to the daemon.
    Daemon = 6,
    /// Invalid user input (bad arguments, missing required fields).
    InvalidInput = 7,
    /// JavaScript evaluation error on the page.
    JavaScriptError = 8,
    /// Unexpected WebSocket disconnection.
    WebSocketDisconnected = 9,
}

impl ErrorCode {
    /// Get the numeric code for this error.
    pub fn code(&self) -> u32 {
        *self as u32
    }
}

/// A typed error with a stable error code.
#[derive(Debug)]
pub struct CliError {
    code: ErrorCode,
    message: String,
    source: Option<anyhow::Error>,
}

impl CliError {
    /// Create a new `CliError` with the given code and message.
    #[allow(dead_code)]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source: None,
        }
    }

    /// Create a new `CliError` with a source error.
    #[allow(dead_code)]
    pub fn with_source(code: ErrorCode, message: impl Into<String>, source: anyhow::Error) -> Self {
        Self {
            code,
            message: message.into(),
            source: Some(source),
        }
    }

    /// Get the stable error code.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Get the numeric code as a string (for JSON output).
    #[allow(dead_code)]
    pub fn code_str(&self) -> String {
        self.code.code().to_string()
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[E{}] {}", self.code.code(), self.message)?;
        if let Some(source) = &self.source {
            write!(f, "\nCause: {source}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e.as_ref() as _)
    }
}

impl From<anyhow::Error> for CliError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            code: ErrorCode::Unspecified,
            message: err.to_string(),
            source: Some(err),
        }
    }
}

/// Result type alias using [`CliError`].
#[allow(dead_code)]
pub type CliResult<T> = Result<T, CliError>;