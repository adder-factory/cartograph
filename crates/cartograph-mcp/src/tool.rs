use std::{collections::BTreeMap, future::Future, pin::Pin};

use serde::Serialize;
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::{CancellationToken, ToolContractError, ToolError, ToolProfile, ToolProfiles};

const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 4_096;

/// Boxed async tool call returned by [`ToolHandler`].
pub type BoxToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>>;
/// Boxed connection-shutdown reconciliation returned by a tool adapter.
pub type BoxShutdownFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Generic product adapter boundary consumed by the MCP server.
pub trait ToolHandler: Send + Sync + 'static {
    /// Return the immutable tool contracts to validate and advertise.
    fn tools(&self) -> Vec<ToolDefinition>;

    /// Execute one already envelope-validated call.
    fn call(&self, call: ToolCall, context: ToolCallContext) -> BoxToolFuture<'_>;

    /// Cancel and reconcile adapter-owned background work before connection exit.
    fn shutdown(&self) -> BoxShutdownFuture<'_> {
        Box::pin(std::future::ready(()))
    }
}

/// One validated MCP tool contract plus internal authorization-profile membership.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    description: String,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<ToolAnnotations>,
    #[serde(skip)]
    profiles: ToolProfiles,
    #[serde(skip)]
    read_only_carve_out: bool,
}

/// Raw tool contract collected before validation at the transport boundary.
pub struct ToolDefinitionInput {
    name: String,
    description: String,
    input_schema: Value,
    profiles: ToolProfiles,
}

impl ToolDefinitionInput {
    /// Collect the human/machine contract with the broad default profile.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            profiles: ToolProfiles::ALL,
        }
    }

    /// Restrict the tool to an explicit non-empty profile set before validation.
    #[must_use]
    pub const fn with_profiles(mut self, profiles: ToolProfiles) -> Self {
        self.profiles = profiles;
        self
    }
}

impl ToolDefinition {
    /// Validate and construct a tool contract.
    ///
    /// # Errors
    ///
    /// Returns [`ToolContractError`] when the name, description, input schema,
    /// or profile membership violates the public tool contract.
    pub fn new(input: ToolDefinitionInput) -> Result<Self, ToolContractError> {
        let ToolDefinitionInput {
            name,
            description,
            input_schema,
            profiles,
        } = input;
        validate_name(&name)?;
        if description.is_empty() || description.len() > MAX_TOOL_DESCRIPTION_BYTES {
            return Err(ToolContractError::InvalidDescription);
        }
        validate_input_schema(&input_schema)?;
        if profiles.is_empty() {
            return Err(ToolContractError::EmptyProfiles);
        }
        Ok(Self {
            name,
            title: None,
            description,
            input_schema,
            annotations: None,
            profiles,
            read_only_carve_out: false,
        })
    }

    /// Attach a compact human-facing title.
    ///
    /// # Errors
    ///
    /// Returns [`ToolContractError::InvalidDescription`] when the title is empty
    /// or exceeds 256 bytes.
    pub fn with_title(mut self, title: impl Into<String>) -> Result<Self, ToolContractError> {
        let title = title.into();
        if title.is_empty() || title.len() > 256 {
            return Err(ToolContractError::InvalidDescription);
        }
        self.title = Some(title);
        Ok(self)
    }

    /// Attach standard MCP behavior hints.
    #[must_use]
    pub fn with_annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Keep a mixed read/write family visible in read-only mode. The product
    /// adapter remains responsible for rejecting every mutating call branch.
    #[must_use]
    pub const fn with_read_only_carve_out(mut self) -> Self {
        self.read_only_carve_out = true;
        self
    }

    /// Canonical programmatic tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bounded human-facing description used by MCP and generated CLI help.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Exact JSON input schema shared by MCP and generated CLI commands.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Standard MCP behavior hints used by profile/budget adapters.
    #[must_use]
    pub const fn annotations(&self) -> Option<ToolAnnotations> {
        self.annotations
    }

    /// Whether this definition belongs to one advertised tool profile.
    #[must_use]
    pub const fn included_in(&self, profile: ToolProfile) -> bool {
        self.profiles.includes(profile)
    }

    /// Whether this mixed family has a product-enforced read-only branch.
    #[must_use]
    pub const fn has_read_only_carve_out(&self) -> bool {
        self.read_only_carve_out
    }
}

/// Standard MCP tool behavior hints.
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// Whether the tool promises not to mutate its environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Whether the tool may perform a destructive operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Whether repeated calls with the same arguments have the same effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Whether the tool can interact with entities outside the local system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Envelope-validated call delivered to the product adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// Advertised canonical tool name.
    pub name: String,
    /// JSON object supplied as MCP `arguments`.
    pub arguments: Map<String, Value>,
}

/// Deadline and cooperative cancellation visible to a tool adapter.
#[derive(Clone, Debug)]
pub struct ToolCallContext {
    cancellation: CancellationToken,
    deadline: Instant,
}

impl ToolCallContext {
    pub(crate) const fn new(cancellation: CancellationToken, deadline: Instant) -> Self {
        Self {
            cancellation,
            deadline,
        }
    }

    /// Build a bounded context for an in-process CLI adapter that reuses the
    /// exact MCP implementation without opening a transport connection.
    #[must_use]
    pub fn local(timeout: std::time::Duration) -> Self {
        let now = Instant::now();
        Self::new(
            CancellationToken::new(),
            now.checked_add(timeout).unwrap_or(now),
        )
    }

    /// Cooperative cancellation probe.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Absolute hard request deadline.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Return whether cancellation or the hard deadline has elapsed.
    #[must_use]
    pub fn should_stop(&self) -> bool {
        self.cancellation.is_cancelled() || Instant::now() >= self.deadline
    }
}

/// MCP text content block used by Cartograph v2's initial tool surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

impl TextContent {
    /// Construct one text block.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// MCP call result with optional structured output and stable error metadata.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    content: Vec<TextContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    metadata: Option<BTreeMap<String, String>>,
}

impl ToolResult {
    /// Construct a successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![TextContent::new(text)],
            structured_content: None,
            is_error: false,
            metadata: None,
        }
    }

    /// Attach structured JSON output matching a tool's future output schema.
    #[must_use]
    pub fn with_structured_content(mut self, content: Map<String, Value>) -> Self {
        self.structured_content = Some(content);
        self
    }

    /// First text block for bounded macro composition and privacy-aware trace summaries.
    #[must_use]
    pub fn primary_text(&self) -> Option<&str> {
        self.content.first().map(|content| content.text.as_str())
    }

    /// Structured result payload for native CLI rendering and bounded macro
    /// composition without reparsing a Markdown text projection.
    #[must_use]
    pub fn structured_content(&self) -> Option<&Map<String, Value>> {
        self.structured_content.as_ref()
    }

    /// Whether this result is an MCP tool-error envelope.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.is_error
    }

    pub(crate) fn from_error(error: &ToolError) -> Self {
        let mut metadata = BTreeMap::new();
        metadata.insert("code".to_owned(), error.code().as_str().to_owned());
        Self {
            content: vec![TextContent::new(error.wire_message())],
            structured_content: None,
            is_error: true,
            metadata: Some(metadata),
        }
    }
}

fn validate_name(name: &str) -> Result<(), ToolContractError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    valid.ok_or(ToolContractError::InvalidName)
}

fn validate_input_schema(schema: &Value) -> Result<(), ToolContractError> {
    let Some(object) = schema.as_object() else {
        return Err(ToolContractError::InvalidInputSchema);
    };
    let valid_type = object.get("type").and_then(Value::as_str) == Some("object");
    let valid_properties = object.get("properties").is_none_or(Value::is_object);
    let valid_required = object.get("required").is_none_or(|required| {
        required
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
    });
    let valid = valid_type && valid_properties && valid_required;
    valid.ok_or(ToolContractError::InvalidInputSchema)
}
