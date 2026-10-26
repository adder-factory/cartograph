use std::{error::Error, fmt, time::Duration};

/// Standard and server-defined JSON-RPC numeric error codes.
pub struct ErrorCode;

impl ErrorCode {
    /// Invalid JSON.
    pub const PARSE_ERROR: i64 = -32_700;
    /// Invalid JSON-RPC envelope.
    pub const INVALID_REQUEST: i64 = -32_600;
    /// Unknown JSON-RPC method.
    pub const METHOD_NOT_FOUND: i64 = -32_601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i64 = -32_602;
    /// Unexpected server failure.
    pub const INTERNAL_ERROR: i64 = -32_603;
    /// Request arrived before MCP initialization completed.
    pub const SERVER_NOT_INITIALIZED: i64 = -32_002;
    /// Request exceeded the server deadline.
    pub const REQUEST_TIMEOUT: i64 = -32_001;
    /// Bounded input line was exceeded before parsing.
    pub const INPUT_TOO_LARGE: i64 = -32_010;
    /// Serialized response exceeded the configured output bound.
    pub const OUTPUT_TOO_LARGE: i64 = -32_011;
    /// Request ID is already active on this connection.
    pub const DUPLICATE_REQUEST_ID: i64 = -32_012;
    /// Bounded in-flight request capacity is exhausted.
    pub const SERVER_BUSY: i64 = -32_013;
    /// Request was cancelled by its peer.
    pub const REQUEST_CANCELLED: i64 = -32_800;
    /// Per-request metadata names a protocol revision this server does not implement.
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32_022;
}

/// Stable machine-readable error values carried in JSON-RPC `error.data.code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StableErrorCode {
    /// Input was not valid JSON.
    ParseError,
    /// The JSON-RPC request envelope was invalid.
    InvalidRequest,
    /// The requested JSON-RPC method is unknown.
    MethodNotFound,
    /// The method parameters failed validation.
    InvalidParams,
    /// The server encountered an internal failure.
    InternalError,
    /// A request arrived before initialization completed.
    ServerNotInitialized,
    /// A request exceeded its deadline.
    RequestTimeout,
    /// An input line exceeded the configured byte bound.
    InputTooLarge,
    /// A serialized response exceeded the configured byte bound.
    OutputTooLarge,
    /// A request identifier is already active.
    DuplicateRequestId,
    /// The bounded in-flight request capacity is exhausted.
    ServerBusy,
    /// The peer cancelled the request.
    RequestCancelled,
    /// The requested tool is not advertised.
    UnknownTool,
    /// The supplied pagination cursor is unsupported.
    UnsupportedCursor,
    /// The connection was initialized more than once.
    AlreadyInitialized,
}

const STABLE_ERROR_CODE_NAMES: [&str; 15] = [
    "parse_error",
    "invalid_request",
    "method_not_found",
    "invalid_params",
    "internal_error",
    "server_not_initialized",
    "request_timeout",
    "input_too_large",
    "output_too_large",
    "duplicate_request_id",
    "server_busy",
    "request_cancelled",
    "unknown_tool",
    "unsupported_cursor",
    "already_initialized",
];

impl StableErrorCode {
    /// Stable snake-case wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        STABLE_ERROR_CODE_NAMES[self as usize]
    }
}

/// Stable tool-level failure categories exposed to coding agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolErrorCode {
    /// Tool arguments failed validation.
    InvalidArguments,
    /// The requested entity does not exist.
    NotFound,
    /// Current state conflicts with the requested operation.
    Conflict,
    /// A required resource is not ready.
    NotReady,
    /// A required service is unavailable.
    Unavailable,
    /// A bounded operation exceeded its published deadline.
    Timeout,
    /// An internal failure whose details must remain redacted.
    Internal,
}

impl ToolErrorCode {
    /// Stable snake-case wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::NotReady => "not_ready",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Internal => "tool_failed",
        }
    }
}

/// Tool execution failure with an explicitly safe or fully redacted message.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolError {
    code: ToolErrorCode,
    safe_message: Option<String>,
    diagnostic: Option<ToolErrorDiagnostic>,
}

/// Source-free failure evidence shared by text-only and structured MCP clients.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolErrorDiagnostic {
    /// Stable operation-specific failure identifier.
    pub code: &'static str,
    /// Stage which could not complete.
    pub stage: &'static str,
    /// Redacted failure category, never provider or driver text.
    pub cause: &'static str,
    /// Whether the same read can reasonably be retried.
    pub retryable: bool,
    /// Bounded corrective action independent of user input.
    pub next_action: &'static str,
}

impl ToolError {
    /// Construct an agent-visible failure. The caller must pass only text that
    /// is safe to cross the MCP boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ToolContractError::InvalidSafeError`] for internal categories,
    /// empty messages, or messages exceeding the public byte bound.
    pub fn safe(
        code: ToolErrorCode,
        safe_message: impl Into<String>,
    ) -> Result<Self, ToolContractError> {
        let safe_message = safe_message.into();
        if code == ToolErrorCode::Internal || safe_message.is_empty() || safe_message.len() > 1_024
        {
            return Err(ToolContractError::InvalidSafeError);
        }
        Ok(Self {
            code,
            safe_message: Some(safe_message),
            diagnostic: None,
        })
    }

    /// Construct a failure whose details must not cross the MCP boundary.
    #[must_use]
    pub const fn internal() -> Self {
        Self {
            code: ToolErrorCode::Internal,
            safe_message: None,
            diagnostic: None,
        }
    }

    /// Attach an explicitly safe, source-free operation diagnostic.
    #[must_use]
    pub const fn with_diagnostic(mut self, diagnostic: ToolErrorDiagnostic) -> Self {
        self.diagnostic = Some(diagnostic);
        self
    }

    /// Source-free operation diagnostic when the adapter supplied one.
    #[must_use]
    pub const fn diagnostic(&self) -> Option<&ToolErrorDiagnostic> {
        self.diagnostic.as_ref()
    }

    /// Stable safe category for adapters composing or auditing tool calls.
    #[must_use]
    pub const fn code(&self) -> ToolErrorCode {
        self.code
    }

    /// Agent-safe message, with internal failures fully redacted.
    #[must_use]
    pub fn wire_message(&self) -> &str {
        self.safe_message
            .as_deref()
            .unwrap_or("Tool execution failed")
    }
}

impl fmt::Debug for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolError")
            .field("code", &self.code)
            .field("message", &self.safe_message.as_ref().map(|_| "<safe>"))
            .field("diagnostic", &self.diagnostic)
            .finish()
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Cartograph tool execution failed")
    }
}

impl Error for ToolError {}

/// Invalid public MCP/tool configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Server name or version metadata is invalid.
    InvalidServerMetadata,
    /// Advertised server instructions are invalid.
    InvalidInstructions,
    /// The configured protocol version is invalid.
    InvalidProtocolVersion,
    /// The configured input limit is invalid.
    InvalidInputLimit,
    /// The configured output limit is invalid.
    InvalidOutputLimit,
    /// The configured in-flight request limit is invalid.
    InvalidInflightLimit,
    /// The configured request deadline is invalid.
    InvalidRequestDeadline,
    /// A tool definition violates the public contract.
    InvalidToolDefinition,
    /// Two registered tools use the same canonical name.
    DuplicateToolName,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidServerMetadata => "MCP server metadata is invalid",
            Self::InvalidInstructions => "MCP server instructions are invalid",
            Self::InvalidProtocolVersion => "MCP protocol version is invalid",
            Self::InvalidInputLimit => "MCP input byte limit is invalid",
            Self::InvalidOutputLimit => "MCP output byte limit is invalid",
            Self::InvalidInflightLimit => "MCP in-flight request limit is invalid",
            Self::InvalidRequestDeadline => "MCP request deadline is invalid",
            Self::InvalidToolDefinition => "MCP tool definition is invalid",
            Self::DuplicateToolName => "MCP tool names must be unique",
        };
        formatter.write_str(message)
    }
}

impl Error for ConfigError {}

/// Invalid tool/profile contract supplied by an adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolContractError {
    /// The canonical tool name is invalid.
    InvalidName,
    /// A forwarded authorization ceiling exceeds its bounded name count.
    InvalidToolCeiling,
    /// The tool description or title is invalid.
    InvalidDescription,
    /// The tool input schema is not a JSON object schema.
    InvalidInputSchema,
    /// The tool is not assigned to any profile.
    EmptyProfiles,
    /// The requested profile name is invalid.
    InvalidProfile,
    /// An agent-visible error would violate the safe-error contract.
    InvalidSafeError,
}

impl fmt::Display for ToolContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MCP tool contract is invalid")
    }
}

impl Error for ToolContractError {}

/// Redacted transport lifecycle failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServeError {
    /// Reading a bounded input line failed.
    InputReadFailed,
    /// Writing a response failed.
    OutputWriteFailed,
    /// The bounded response channel closed unexpectedly.
    OutputChannelClosed,
    /// The response writer task failed.
    OutputTaskFailed,
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InputReadFailed => "MCP input stream failed",
            Self::OutputWriteFailed => "MCP output stream failed",
            Self::OutputChannelClosed => "MCP output channel closed",
            Self::OutputTaskFailed => "MCP output task failed",
        };
        formatter.write_str(message)
    }
}

impl Error for ServeError {}

pub(crate) const fn valid_deadline(deadline: Duration) -> bool {
    !deadline.is_zero()
        && (deadline.as_secs() < 600 || (deadline.as_secs() == 600 && deadline.subsec_nanos() == 0))
}
