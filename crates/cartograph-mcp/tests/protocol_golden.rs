//! Golden protocol coverage for bounded MCP transport and tool contracts.

use std::{error::Error, io, sync::Arc, time::Duration};

use cartograph_mcp::{
    BoxToolFuture, ConfigError, ErrorCode, MODERN_PROTOCOL_VERSION, ProtocolServer, ServerConfig,
    ServerLimits, ServerLimitsInput, ServerMetadata, ToolAnnotations, ToolCall, ToolCallContext,
    ToolContractError, ToolDefinition, ToolDefinitionInput, ToolError, ToolErrorCode, ToolHandler,
    ToolProfile, ToolProfiles, ToolResult,
};
use serde as _;
use serde_json::{Value, json};
use tokio::{
    io::{
        AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf,
        duplex, split,
    },
    sync::Notify,
    task::JoinHandle,
    time::timeout,
};

type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

#[derive(Clone)]
struct FixtureHandler {
    slow_started: Arc<Notify>,
    slow_dropped: Arc<Notify>,
}

impl FixtureHandler {
    fn new() -> Self {
        Self {
            slow_started: Arc::new(Notify::new()),
            slow_dropped: Arc::new(Notify::new()),
        }
    }

    async fn wait_for_slow_call(&self) -> TestResult {
        timeout(Duration::from_secs(2), self.slow_started.notified())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "slow fixture did not start"))?;
        Ok(())
    }

    async fn wait_for_slow_drop(&self) -> TestResult {
        timeout(Duration::from_secs(2), self.slow_dropped.notified())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "slow fixture was not reaped"))?;
        Ok(())
    }
}

struct SlowDrop(Arc<Notify>);

impl Drop for SlowDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl ToolHandler for FixtureHandler {
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            definition(
                "z_review",
                "Review-only fixture",
                empty_schema(),
                ToolProfiles::REVIEW,
            )
            .with_annotations(ToolAnnotations {
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
            }),
            definition(
                "cartograph_slow",
                "Never completes without transport supervision",
                empty_schema(),
                ToolProfiles::ALL,
            ),
            definition(
                "cartograph_echo",
                "Echo one message",
                json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "Text to return"
                        }
                    },
                    "required": ["message"]
                }),
                ToolProfiles::ALL,
            ),
            definition(
                "cartograph_read",
                "Read-only fixture",
                empty_schema(),
                ToolProfiles::READ_ONLY,
            ),
            definition(
                "cartograph_fail",
                "Redacted internal failure fixture",
                empty_schema(),
                ToolProfiles::ALL,
            ),
            definition(
                "cartograph_big",
                "Oversized output fixture",
                empty_schema(),
                ToolProfiles::ALL,
            ),
            definition(
                "cartograph_core",
                "Core-only fixture",
                empty_schema(),
                ToolProfiles::CORE,
            ),
            definition(
                "cartograph_context_fixture",
                "Structured reference-payload fixture",
                empty_schema(),
                ToolProfiles::CORE,
            ),
        ]
    }

    fn call(&self, call: ToolCall, context: ToolCallContext) -> BoxToolFuture<'_> {
        Box::pin(async move {
            match call.name.as_str() {
                "cartograph_echo" => match call.arguments.get("message").and_then(Value::as_str) {
                    Some(message) => Ok(ToolResult::text(message)),
                    None => Err(safe_tool_error(
                        ToolErrorCode::InvalidArguments,
                        "message must be a string",
                    )),
                },
                "cartograph_big" => Ok(ToolResult::text("x".repeat(8_192))),
                "cartograph_fail" => Err(ToolError::internal()),
                "cartograph_context_fixture" => serde_json::to_string(&reference_context_payload())
                    .map(ToolResult::text)
                    .map_err(|_| ToolError::internal()),
                "cartograph_slow" => {
                    let _drop = SlowDrop(self.slow_dropped.clone());
                    self.slow_started.notify_one();
                    context.cancellation().cancelled().await;
                    std::future::pending::<Result<ToolResult, ToolError>>().await
                }
                "cartograph_core" | "cartograph_read" | "z_review" => {
                    Ok(ToolResult::text(call.name))
                }
                _ => Err(safe_tool_error(ToolErrorCode::NotFound, "tool not found")),
            }
        })
    }
}

struct Connection {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    server: JoinHandle<Result<(), cartograph_mcp::ServeError>>,
}

impl Connection {
    fn open(server: ProtocolServer) -> Self {
        let (client, server_stream) = duplex(128 * 1_024);
        let (client_reader, client_writer) = split(client);
        let (server_reader, server_writer) = split(server_stream);
        let task = tokio::spawn(async move { server.serve(server_reader, server_writer).await });
        Self {
            reader: BufReader::new(client_reader),
            writer: client_writer,
            server: task,
        }
    }

    async fn send(&mut self, value: &Value) -> TestResult {
        self.send_raw(&serde_json::to_string(value)?).await
    }

    async fn send_raw(&mut self, raw: &str) -> TestResult {
        self.writer.write_all(raw.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<String, Box<dyn Error + Send + Sync>> {
        let mut line = String::new();
        let bytes = timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP response timed out"))??;
        if bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "MCP output closed").into());
        }
        Ok(line)
    }

    async fn read_value(&mut self) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let line = self.read_line().await?;
        Ok(serde_json::from_str(&line)?)
    }

    async fn initialize(&mut self) -> TestResult {
        self.send(&initialize_request(1)).await?;
        let response = self.read_value().await?;
        if response["result"]["protocolVersion"] != DEFAULT_TEST_PROTOCOL {
            return Err(io::Error::other("unexpected negotiated protocol").into());
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await
    }

    async fn finish(mut self) -> TestResult {
        self.writer.shutdown().await?;
        let mut remainder = String::new();
        self.reader.read_to_string(&mut remainder).await?;
        if !remainder.is_empty() {
            return Err(io::Error::other(format!("unexpected MCP output: {remainder}")).into());
        }
        let result = self
            .server
            .await
            .map_err(|_| io::Error::other("MCP server task failed"))?;
        result?;
        Ok(())
    }
}

const DEFAULT_TEST_PROTOCOL: &str = "2024-11-05";
const FIXTURE_REFERENCE_ID: u64 = 101;
const FIXTURE_START_BYTE: u64 = 12;
const FIXTURE_END_BYTE: u64 = 19;
const FIXTURE_CONFIDENCE: f64 = 0.95;
const FIXTURE_SITE_COUNT: u64 = 7;
const FIXTURE_OWNER_SYMBOL_ID: &str = "11111111-1111-4111-8111-111111111111";
const FIXTURE_TARGET_SYMBOL_ID: &str = "22222222-2222-4222-8222-222222222222";

fn reference_context_payload() -> Value {
    json!({
        "confidence": "high",
        "graph_direction": "callers",
        "evidence": [{
            "reasons": ["exact_reference"],
            "language": "rust",
            "document_kind": "symbol",
            "reference": {
                "reference_id": FIXTURE_REFERENCE_ID,
                "owner_symbol_id": FIXTURE_OWNER_SYMBOL_ID,
                "target_symbol_id": FIXTURE_TARGET_SYMBOL_ID,
                "start_byte": FIXTURE_START_BYTE,
                "end_byte": FIXTURE_END_BYTE,
                "span_precision": "exact",
                "confidence": FIXTURE_CONFIDENCE,
                "provenance": "resolved_fixture",
                "represented_site_count": FIXTURE_SITE_COUNT,
            }
        }]
    })
}

#[tokio::test]
async fn initialize_initialized_and_ping_match_the_golden_wire_contract() -> TestResult {
    let server = test_server(
        ToolProfile::Core,
        ServerLimits::default(),
        FixtureHandler::new(),
    )?;
    let mut connection = Connection::open(server);
    connection.send(&initialize_request(1)).await?;

    let line = connection.read_line().await?;
    let expected = [
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{",
        "\"capabilities\":{\"tools\":{}},",
        "\"protocolVersion\":\"2024-11-05\",",
        "\"serverInfo\":{\"name\":\"cartograph\",\"version\":\"",
        env!("CARGO_PKG_VERSION"),
        "\"}}}\n",
    ]
    .concat();
    assert_eq!(line, expected);

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {"_meta": {"client": "fixture"}}
        }))
        .await?;
    connection
        .send(&json!({"jsonrpc": "2.0", "id": "ping-1", "method": "ping"}))
        .await?;
    assert_eq!(
        connection.read_value().await?,
        json!({"jsonrpc": "2.0", "id": "ping-1", "result": {}})
    );
    connection
        .send(&json!({"jsonrpc": "2.0", "id": "", "method": "ping"}))
        .await?;
    assert_eq!(
        connection.read_value().await?,
        json!({"jsonrpc": "2.0", "id": "", "result": {}})
    );
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {"_meta": {"progressToken": "legacy-progress"}}
        }))
        .await?;
    let legacy_list = connection.read_value().await?;
    assert!(legacy_list["result"].get("resultType").is_none());
    assert_eq!(tool_names(&legacy_list).len(), 6);
    connection.finish().await
}

#[tokio::test]
async fn initialize_advertises_only_explicit_validated_server_instructions() -> TestResult {
    const INSTRUCTIONS: &str =
        "Check freshness first, inspect exact evidence, and run the repository gates.";
    let config = ServerConfig::new(
        ServerMetadata::cartograph(),
        ToolProfile::Coding,
        ServerLimits::default(),
    )
    .with_instructions(INSTRUCTIONS)?;
    let server = ProtocolServer::new(config, FixtureHandler::new())?;
    let mut connection = Connection::open(server);
    connection.send(&initialize_request(1)).await?;
    let response = connection.read_value().await?;
    assert_eq!(response["result"]["instructions"], INSTRUCTIONS);
    assert_eq!(response["result"]["protocolVersion"], DEFAULT_TEST_PROTOCOL);
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await?;
    connection.finish().await
}

#[tokio::test]
async fn modern_discovery_and_ping_are_stateless_cacheable_and_self_describing() -> TestResult {
    const INSTRUCTIONS: &str =
        "Check freshness first, inspect exact evidence, and run the repository gates.";
    let config = ServerConfig::new(
        ServerMetadata::cartograph(),
        ToolProfile::Core,
        ServerLimits::default(),
    )
    .with_instructions(INSTRUCTIONS)?;
    let server = ProtocolServer::new(config, FixtureHandler::new())?;
    let mut connection = Connection::open(server);

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "discover-1",
            "method": "server/discover",
            "params": modern_params()
        }))
        .await?;
    let discovery = connection.read_value().await?;
    assert_eq!(discovery["result"]["resultType"], "complete");
    assert_eq!(
        discovery["result"]["supportedVersions"],
        json!([MODERN_PROTOCOL_VERSION])
    );
    assert_eq!(discovery["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(discovery["result"]["instructions"], INSTRUCTIONS);
    assert_eq!(discovery["result"]["ttlMs"], 3_600_000);
    assert_eq!(discovery["result"]["cacheScope"], "private");
    assert_eq!(
        discovery["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "cartograph"
    );

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "ping-modern",
            "method": "ping",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .await?;
    let ping = connection.read_value().await?;
    assert_eq!(ping["result"]["resultType"], "complete");
    assert_eq!(
        ping["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "cartograph"
    );

    connection.send(&initialize_request(2)).await?;
    let legacy = connection.read_value().await?;
    assert_eq!(legacy["result"]["protocolVersion"], DEFAULT_TEST_PROTOCOL);
    assert!(legacy["result"].get("resultType").is_none());
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .await?;
    connection
        .send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}))
        .await?;
    assert_eq!(tool_names(&connection.read_value().await?).len(), 6);
    connection.finish().await
}

#[tokio::test]
async fn modern_tools_are_stable_cacheable_and_callable_without_initialize() -> TestResult {
    let server = test_server(
        ToolProfile::Core,
        ServerLimits::default(),
        FixtureHandler::new(),
    )?;
    let mut connection = Connection::open(server);

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": modern_params()
        }))
        .await?;
    let first = connection.read_value().await?;
    assert_eq!(first["result"]["resultType"], "complete");
    assert_eq!(first["result"]["ttlMs"], 3_600_000);
    assert_eq!(first["result"]["cacheScope"], "private");
    assert_eq!(
        tool_names(&first),
        vec![
            "cartograph_big",
            "cartograph_context_fixture",
            "cartograph_core",
            "cartograph_echo",
            "cartograph_fail",
            "cartograph_slow",
        ]
    );

    let mut call_params = modern_params();
    let call_object = call_params
        .as_object_mut()
        .ok_or_else(|| io::Error::other("modern params were not an object"))?;
    call_object.insert("name".to_owned(), json!("cartograph_echo"));
    call_object.insert("arguments".to_owned(), json!({"message": "modern"}));
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": call_params
        }))
        .await?;
    let called = connection.read_value().await?;
    assert_eq!(called["result"]["resultType"], "complete");
    assert_eq!(called["result"]["content"][0]["text"], "modern");
    assert_eq!(
        called["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "cartograph"
    );

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
            "params": modern_params()
        }))
        .await?;
    let second = connection.read_value().await?;
    assert_eq!(first["result"]["tools"], second["result"]["tools"]);
    assert_eq!(first["result"]["ttlMs"], second["result"]["ttlMs"]);
    assert_eq!(
        first["result"]["cacheScope"],
        second["result"]["cacheScope"]
    );
    connection.finish().await
}

#[tokio::test]
async fn modern_request_metadata_and_versions_fail_closed_without_legacy_fallback() -> TestResult {
    let server = test_server(
        ToolProfile::Core,
        ServerLimits::default(),
        FixtureHandler::new(),
    )?;
    let mut connection = Connection::open(server);

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION
                }
            }
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::INVALID_PARAMS,
        "invalid_params",
    );

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": modern_params_for("2099-01-01")
        }))
        .await?;
    let unsupported = connection.read_value().await?;
    assert_eq!(unsupported["error"]["code"], -32_022);
    assert_eq!(
        unsupported["error"]["message"],
        "Unsupported protocol version"
    );
    assert_eq!(unsupported["error"]["data"]["requested"], "2099-01-01");
    assert_eq!(
        unsupported["error"]["data"]["supported"],
        json!([MODERN_PROTOCOL_VERSION])
    );

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "server/discover",
            "params": {}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::INVALID_PARAMS,
        "invalid_params",
    );

    connection
        .send(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::SERVER_NOT_INITIALIZED,
        "server_not_initialized",
    );
    connection.finish().await
}

#[tokio::test]
async fn tools_list_is_sorted_schema_exact_profile_filtered_and_deterministic() -> TestResult {
    let first = tools_list_line(ToolProfile::Core).await?;
    let second = tools_list_line(ToolProfile::Core).await?;
    assert_eq!(first, second);

    let response: Value = serde_json::from_str(&first)?;
    assert_eq!(
        tool_names(&response),
        vec![
            "cartograph_big",
            "cartograph_context_fixture",
            "cartograph_core",
            "cartograph_echo",
            "cartograph_fail",
            "cartograph_slow",
        ]
    );
    let echo = response["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "cartograph_echo"))
        .ok_or_else(|| io::Error::other("echo tool missing"))?;
    assert_eq!(
        echo["inputSchema"],
        json!({
            "type": "object",
            "properties": {
                "message": {"type": "string", "description": "Text to return"}
            },
            "required": ["message"]
        })
    );
    assert!(echo.get("profiles").is_none());

    let broad_read_tools = vec![
        "cartograph_big",
        "cartograph_echo",
        "cartograph_fail",
        "cartograph_slow",
    ];
    assert_eq!(
        profile_tool_names(ToolProfile::Full).await?,
        broad_read_tools
    );
    assert_eq!(
        profile_tool_names(ToolProfile::Coding).await?,
        broad_read_tools
    );

    assert_eq!(
        profile_tool_names(ToolProfile::ReadOnly).await?,
        vec![
            "cartograph_big",
            "cartograph_echo",
            "cartograph_fail",
            "cartograph_read",
            "cartograph_slow",
        ]
    );
    assert_eq!(
        profile_tool_names(ToolProfile::Review).await?,
        vec![
            "cartograph_big",
            "cartograph_echo",
            "cartograph_fail",
            "cartograph_slow",
            "z_review",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn tool_transport_preserves_structured_reference_payload_and_multiplicity() -> TestResult {
    let server = test_server(
        ToolProfile::Core,
        ServerLimits::default(),
        FixtureHandler::new(),
    )?;
    let mut connection = Connection::open(server);
    connection.initialize().await?;
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "evidence-1",
            "method": "tools/call",
            "params": {
                "name": "cartograph_context_fixture",
                "arguments": {}
            }
        }))
        .await?;

    let response = connection.read_value().await?;
    let transported = response["result"]["content"][0]["text"]
        .as_str()
        .ok_or_else(|| io::Error::other("structured reference payload was not transported"))?;
    assert_eq!(
        transported,
        serde_json::to_string(&reference_context_payload())?
    );
    let transported: Value = serde_json::from_str(transported)?;
    let reference = &transported["evidence"][0]["reference"];
    assert_eq!(transported["confidence"], "high");
    assert_eq!(transported["graph_direction"], "callers");
    assert_eq!(transported["evidence"][0]["reasons"][0], "exact_reference");
    assert_eq!(transported["evidence"][0]["language"], "rust");
    assert_eq!(transported["evidence"][0]["document_kind"], "symbol");
    assert_eq!(reference["reference_id"], FIXTURE_REFERENCE_ID);
    assert_eq!(reference["owner_symbol_id"], FIXTURE_OWNER_SYMBOL_ID);
    assert_eq!(reference["target_symbol_id"], FIXTURE_TARGET_SYMBOL_ID);
    assert_eq!(reference["span_precision"], "exact");
    assert_eq!(reference["represented_site_count"], FIXTURE_SITE_COUNT);
    assert_eq!(reference["start_byte"], FIXTURE_START_BYTE);
    assert_eq!(reference["end_byte"], FIXTURE_END_BYTE);
    assert_eq!(reference["confidence"], FIXTURE_CONFIDENCE);
    assert_eq!(reference["provenance"], "resolved_fixture");
    connection.finish().await
}

#[tokio::test]
async fn envelope_params_unknown_methods_and_unknown_tools_are_stable_and_redacted() -> TestResult {
    let server = test_server(
        ToolProfile::Core,
        ServerLimits::default(),
        FixtureHandler::new(),
    )?;
    let mut connection = Connection::open(server);

    connection.send_raw("{not-json}").await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::PARSE_ERROR,
        "parse_error",
    );
    connection.send(&json!([1, 2, 3])).await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::INVALID_REQUEST,
        "invalid_request",
    );
    connection
        .send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::SERVER_NOT_INITIALIZED,
        "server_not_initialized",
    );

    connection.initialize().await?;
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "cartograph_echo", "arguments": "not-an-object"}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::INVALID_PARAMS,
        "invalid_params",
    );

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "secret_hidden_tool", "arguments": {}}
        }))
        .await?;
    let unknown = connection.read_value().await?;
    assert_error(&unknown, ErrorCode::INVALID_PARAMS, "unknown_tool");
    assert!(!unknown.to_string().contains("secret_hidden_tool"));

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 41,
            "method": "tools/call",
            "params": {"name": "cartograph_read", "arguments": {}}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::INVALID_PARAMS,
        "unknown_tool",
    );

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "secret/private/method"
        }))
        .await?;
    let method = connection.read_value().await?;
    assert_error(&method, ErrorCode::METHOD_NOT_FOUND, "method_not_found");
    assert!(!method.to_string().contains("secret/private/method"));

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {"name": "cartograph_fail", "arguments": {}}
        }))
        .await?;
    assert_eq!(
        connection.read_value().await?,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "result": {
                "content": [{"type": "text", "text": "Tool execution failed"}],
                "isError": true,
                "_meta": {"code": "tool_failed"}
            }
        })
    );
    connection.finish().await
}

#[tokio::test]
async fn cancellation_aborts_and_reaps_an_inflight_tool_call() -> TestResult {
    let handler = FixtureHandler::new();
    let observer = handler.clone();
    let server = test_server(ToolProfile::Core, ServerLimits::default(), handler)?;
    let mut connection = Connection::open(server);
    connection.initialize().await?;
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "slow-1",
            "method": "tools/call",
            "params": {"name": "cartograph_slow", "arguments": {}}
        }))
        .await?;
    observer.wait_for_slow_call().await?;
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "slow-1", "reason": "no longer needed"}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::REQUEST_CANCELLED,
        "request_cancelled",
    );
    connection
        .send(&json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}))
        .await?;
    assert_eq!(connection.read_value().await?["result"], json!({}));
    connection.finish().await
}

#[tokio::test]
async fn hard_request_deadline_aborts_and_reaps_noncompleting_work() -> TestResult {
    let limits = ServerLimits::new(
        ServerLimitsInput::new(1_024, 4_096, 4).with_request_deadline(Duration::from_millis(25)),
    )?;
    let server = test_server(ToolProfile::Core, limits, FixtureHandler::new())?;
    let mut connection = Connection::open(server);
    connection.initialize().await?;
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {"name": "cartograph_slow", "arguments": {}}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::REQUEST_TIMEOUT,
        "request_timeout",
    );
    connection.finish().await
}

#[tokio::test]
async fn inflight_capacity_and_duplicate_ids_are_bounded_without_locking_cancellation() -> TestResult
{
    let handler = FixtureHandler::new();
    let observer = handler.clone();
    let limits = ServerLimits::new(
        ServerLimitsInput::new(1_024, 4_096, 1).with_request_deadline(Duration::from_secs(1)),
    )?;
    let server = test_server(ToolProfile::Core, limits, handler)?;
    let mut connection = Connection::open(server);
    connection.initialize().await?;
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "active",
            "method": "tools/call",
            "params": {"name": "cartograph_slow", "arguments": {}}
        }))
        .await?;
    observer.wait_for_slow_call().await?;

    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "second",
            "method": "tools/call",
            "params": {"name": "cartograph_slow", "arguments": {}}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::SERVER_BUSY,
        "server_busy",
    );
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": "active",
            "method": "tools/call",
            "params": {"name": "cartograph_slow", "arguments": {}}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::DUPLICATE_REQUEST_ID,
        "duplicate_request_id",
    );
    connection
        .send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "active"}
        }))
        .await?;
    assert_error(
        &connection.read_value().await?,
        ErrorCode::REQUEST_CANCELLED,
        "request_cancelled",
    );
    connection.finish().await
}

#[tokio::test]
async fn input_and_output_byte_limits_fail_closed_without_killing_the_session() -> TestResult {
    let input_limits = ServerLimits::new(
        ServerLimitsInput::new(256, 4_096, 4).with_request_deadline(Duration::from_secs(1)),
    )?;
    let input_server = test_server(ToolProfile::Core, input_limits, FixtureHandler::new())?;
    let mut input_connection = Connection::open(input_server);
    input_connection.send_raw(&"x".repeat(300)).await?;
    let oversized_input = input_connection.read_value().await?;
    assert_eq!(oversized_input["id"], Value::Null);
    assert_error(
        &oversized_input,
        ErrorCode::INPUT_TOO_LARGE,
        "input_too_large",
    );
    input_connection
        .send(&json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}))
        .await?;
    assert_eq!(input_connection.read_value().await?["result"], json!({}));
    input_connection.finish().await?;

    let output_limits = ServerLimits::new(
        ServerLimitsInput::new(1_024, 512, 4).with_request_deadline(Duration::from_secs(1)),
    )?;
    let output_server = test_server(ToolProfile::Core, output_limits, FixtureHandler::new())?;
    let mut output_connection = Connection::open(output_server);
    output_connection.initialize().await?;
    output_connection
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {"name": "cartograph_big", "arguments": {}}
        }))
        .await?;
    let output_line = output_connection.read_line().await?;
    assert!(output_line.trim_end().len() <= 512);
    let oversized_output: Value = serde_json::from_str(&output_line)?;
    assert_error(
        &oversized_output,
        ErrorCode::OUTPUT_TOO_LARGE,
        "output_too_large",
    );
    output_connection.finish().await
}

#[tokio::test]
async fn broken_output_terminates_an_open_input_session_and_reaps_calls() -> TestResult {
    let handler = FixtureHandler::new();
    let server = test_server(ToolProfile::Core, ServerLimits::default(), handler.clone())?;
    let (mut client_input, server_input) = duplex(8_192);
    let (server_output, abandoned_output) = duplex(8_192);
    drop(abandoned_output);
    let server_task = tokio::spawn(async move { server.serve(server_input, server_output).await });

    client_input
        .write_all(
            concat!(
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",",
                "\"params\":{\"protocolVersion\":\"2024-11-05\",",
                "\"capabilities\":{},\"clientInfo\":{\"name\":\"fixture\",",
                "\"version\":\"1.0\"}}}\n"
            )
            .as_bytes(),
        )
        .await?;
    client_input.flush().await?;

    let result = timeout(Duration::from_secs(2), server_task)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "broken writer was not observed"))?
        .map_err(|_| io::Error::other("MCP server task failed"))?;
    assert_eq!(result, Err(cartograph_mcp::ServeError::OutputWriteFailed));
    Ok(())
}

#[tokio::test]
async fn writer_failure_cancels_and_reaps_active_calls_before_returning() -> TestResult {
    let handler = FixtureHandler::new();
    let server = test_server(ToolProfile::Core, ServerLimits::default(), handler.clone())?;
    let (mut input, server_input) = duplex(16 * 1_024);
    let (server_output, client_output) = duplex(16 * 1_024);
    let mut output = BufReader::new(client_output);
    let server_task = tokio::spawn(async move { server.serve(server_input, server_output).await });

    input
        .write_all(format!("{}\n", initialize_request(1)).as_bytes())
        .await?;
    input.flush().await?;
    let mut initialized = String::new();
    output.read_line(&mut initialized).await?;
    assert_ne!(initialized, "");
    input
        .write_all(
            concat!(
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",",
                "\"params\":{\"name\":\"cartograph_slow\",\"arguments\":{}}}\n"
            )
            .as_bytes(),
        )
        .await?;
    input.flush().await?;
    handler.wait_for_slow_call().await?;

    drop(output);
    input
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n")
        .await?;
    input.flush().await?;
    let result = timeout(Duration::from_secs(2), server_task)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "writer failure did not terminate"))?
        .map_err(|_| io::Error::other("MCP server task failed"))?;
    assert_eq!(result, Err(cartograph_mcp::ServeError::OutputWriteFailed));
    handler.wait_for_slow_drop().await?;
    Ok(())
}

#[test]
fn public_configuration_and_schema_boundaries_reject_invalid_contracts() {
    for instructions in [String::new(), "   \n".to_owned(), "bad\0value".to_owned()] {
        assert_eq!(
            ServerConfig::default()
                .with_instructions(instructions)
                .err(),
            Some(ConfigError::InvalidInstructions)
        );
    }
    assert_eq!(
        ServerConfig::default()
            .with_instructions("x".repeat(16 * 1_024 + 1))
            .err(),
        Some(ConfigError::InvalidInstructions)
    );
    assert_eq!(
        ServerConfig::default()
            .with_protocol_version(MODERN_PROTOCOL_VERSION)
            .err(),
        Some(ConfigError::InvalidProtocolVersion)
    );
    assert_eq!(
        ServerConfig::default()
            .with_protocol_version("2025-11-25")
            .err(),
        Some(ConfigError::InvalidProtocolVersion)
    );
    assert_eq!(
        ServerLimits::new(
            ServerLimitsInput::new(127, 4_096, 1).with_request_deadline(Duration::from_secs(1)),
        ),
        Err(ConfigError::InvalidInputLimit)
    );
    assert_eq!(
        ServerLimits::new(
            ServerLimitsInput::new(1_024, 511, 1).with_request_deadline(Duration::from_secs(1)),
        ),
        Err(ConfigError::InvalidOutputLimit)
    );
    assert_eq!(
        ServerLimits::new(
            ServerLimitsInput::new(1_024, 4_096, 0).with_request_deadline(Duration::from_secs(1)),
        ),
        Err(ConfigError::InvalidInflightLimit)
    );
    assert_eq!(
        ServerLimits::new(
            ServerLimitsInput::new(1_024, 4_096, 1)
                .with_request_deadline(Duration::from_mins(10) + Duration::from_nanos(1),),
        ),
        Err(ConfigError::InvalidRequestDeadline)
    );
    assert_eq!(
        ToolDefinition::new(
            ToolDefinitionInput::new(
                "bad_schema",
                "Invalid properties shape",
                json!({"type": "object", "properties": []}),
            )
            .with_profiles(ToolProfiles::CORE),
        )
        .err(),
        Some(ToolContractError::InvalidInputSchema)
    );
    assert_eq!(
        ToolDefinition::new(
            ToolDefinitionInput::new("no_profile", "No advertised profile", empty_schema())
                .with_profiles(ToolProfiles::NONE),
        )
        .err(),
        Some(ToolContractError::EmptyProfiles)
    );
}

fn definition(
    name: &str,
    description: &str,
    schema: Value,
    profiles: ToolProfiles,
) -> ToolDefinition {
    match ToolDefinition::new(
        ToolDefinitionInput::new(name, description, schema).with_profiles(profiles),
    ) {
        Ok(definition) => definition,
        Err(error) => panic!("fixture tool definition failed: {error}"),
    }
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

fn safe_tool_error(code: ToolErrorCode, message: &str) -> ToolError {
    match ToolError::safe(code, message) {
        Ok(error) => error,
        Err(error) => panic!("fixture safe tool error failed: {error}"),
    }
}

fn test_server(
    profile: ToolProfile,
    limits: ServerLimits,
    handler: FixtureHandler,
) -> Result<ProtocolServer, cartograph_mcp::ConfigError> {
    ProtocolServer::new(
        ServerConfig::new(ServerMetadata::cartograph(), profile, limits),
        handler,
    )
}

fn initialize_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": DEFAULT_TEST_PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "fixture", "version": "1.0"}
        }
    })
}

fn modern_params() -> Value {
    modern_params_for(MODERN_PROTOCOL_VERSION)
}

fn modern_params_for(protocol_version: &str) -> Value {
    json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": protocol_version,
            "io.modelcontextprotocol/clientInfo": {
                "name": "fixture",
                "version": "1.0"
            },
            "io.modelcontextprotocol/clientCapabilities": {}
        }
    })
}

async fn tools_list_line(profile: ToolProfile) -> Result<String, Box<dyn Error + Send + Sync>> {
    let server = test_server(profile, ServerLimits::default(), FixtureHandler::new())?;
    let mut connection = Connection::open(server);
    connection.initialize().await?;
    connection
        .send(&json!({"jsonrpc": "2.0", "id": 11, "method": "tools/list"}))
        .await?;
    let line = connection.read_line().await?;
    connection.finish().await?;
    Ok(line)
}

async fn profile_tool_names(
    profile: ToolProfile,
) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    let line = tools_list_line(profile).await?;
    let response: Value = serde_json::from_str(&line)?;
    Ok(tool_names(&response)
        .into_iter()
        .map(str::to_owned)
        .collect())
}

fn tool_names(response: &Value) -> Vec<&str> {
    response["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

fn assert_error(response: &Value, numeric_code: i64, stable_code: &str) {
    assert_eq!(response["error"]["code"], numeric_code);
    assert_eq!(response["error"]["data"]["code"], stable_code);
}
