//! Bounded Model Context Protocol transport and protocol contracts.
//!
//! The crate deliberately stops at an async [`ToolHandler`] boundary. Product
//! services can be adapted later without allowing MCP transport code to reach
//! through crate boundaries into database or indexing internals.

mod cancellation;
mod error;
mod profile;
mod protocol;
mod tool;
mod transport;

pub use cancellation::CancellationToken;
pub use error::{
    ConfigError, ErrorCode, ServeError, StableErrorCode, ToolContractError, ToolError,
    ToolErrorCode, ToolErrorDiagnostic,
};
pub use profile::{ToolProfile, ToolProfiles};
pub use protocol::{
    DEFAULT_PROTOCOL_VERSION, MODERN_PROTOCOL_VERSION, ProtocolServer, RequestId, ServerConfig,
    ServerLimits, ServerLimitsInput, ServerMetadata,
};
pub use tool::{
    BoxShutdownFuture, BoxToolFuture, TextContent, ToolAnnotations, ToolCall, ToolCallContext,
    ToolDefinition, ToolDefinitionInput, ToolHandler, ToolResult,
};
