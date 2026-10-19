//! Same-user project-owner routing. A client never opens a second native primary.

#[cfg(unix)]
mod unix;

use cartograph_mcp::{
    BoxToolFuture, ProtocolServer, ServerConfig, ToolCall, ToolCallContext, ToolDefinition,
    ToolError, ToolErrorCode, ToolHandler, ToolResult,
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

pub(crate) struct OwnerClient {
    #[cfg(unix)]
    inner: unix::OwnerClient,
}

impl OwnerClient {
    #[cfg(unix)]
    pub(crate) async fn connect(root: &Path) -> Result<Option<Self>, String> {
        Ok(unix::OwnerClient::connect(root)
            .await?
            .map(|inner| Self { inner }))
    }

    #[cfg(not(unix))]
    pub(crate) fn connect(root: &Path) -> impl Future<Output = Result<Option<Self>, String>> {
        let _ = root;
        std::future::ready(Ok(None))
    }

    #[cfg(unix)]
    pub(crate) async fn call_owned(
        self,
        call: ToolCall,
        context: ToolCallContext,
        expected_owner: Option<String>,
    ) -> Result<(String, ToolResult), ToolError> {
        self.inner.call_owned(call, context, expected_owner).await
    }

    #[cfg(not(unix))]
    pub(crate) fn call_owned(
        self,
        call: ToolCall,
        context: ToolCallContext,
        expected_owner: Option<String>,
    ) -> impl Future<Output = Result<(String, ToolResult), ToolError>> {
        let _ = (self, call, context, expected_owner);
        std::future::ready(Err(transport_error(
            "native project owner transport is unavailable",
        )))
    }
}

pub(crate) struct OwnerProxy {
    root: PathBuf,
    definitions: Vec<ToolDefinition>,
    read_only: bool,
    owner_instance: OnceLock<String>,
    initializing: tokio::sync::Mutex<()>,
}

impl OwnerProxy {
    pub(crate) fn new(root: &Path, definitions: Vec<ToolDefinition>, read_only: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            definitions,
            read_only,
            owner_instance: OnceLock::new(),
            initializing: tokio::sync::Mutex::new(()),
        }
    }
}

impl ToolHandler for OwnerProxy {
    fn drain_cancelled_calls(&self) -> bool {
        true
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    fn call(&self, call: ToolCall, context: ToolCallContext) -> BoxToolFuture<'_> {
        Box::pin(async move {
            if !self.definitions.iter().any(|tool| tool.name() == call.name) {
                return Err(transport_error(
                    "tool is outside this host's authorization ceiling",
                ));
            }
            if self.read_only
                && !crate::mcp_handler::read_only_call_allowed(&self.definitions, &call)
            {
                return Err(transport_error(
                    "This mutating tool branch is disabled by the MCP server read-only write gate",
                ));
            }
            let context = context
                .with_tool_ceiling(self.definitions.iter().map(ToolDefinition::name))
                .map_err(|_| ToolError::internal())?;
            // Serialize only the first reply. Once pinned, independent calls
            // remain concurrent and cannot address a replacement owner's jobs.
            let _initialization = if self.owner_instance.get().is_none() {
                Some(tokio::select! {
                    biased;
                    () = context.cancellation().cancelled() => return Err(transport_error("native owner request was cancelled")),
                    () = tokio::time::sleep_until(context.deadline()) => return Err(transport_error("native owner request deadline expired")),
                    guard = self.initializing.lock() => guard,
                })
            } else {
                None
            };
            let expected_owner = self.owner_instance.get().cloned();
            let client = OwnerClient::connect(&self.root)
                .await
                .map_err(transport_error)?
                .ok_or_else(|| transport_error("native project owner is unavailable"))?;
            let (instance, result) = client
                .call_owned(call, context, expected_owner.clone())
                .await?;
            if expected_owner.is_none() {
                self.owner_instance.set(instance).map_err(|_| {
                    transport_error("native project owner identity could not be pinned")
                })?;
            }
            Ok(result)
        })
    }
}

pub(crate) async fn serve(
    root: &Path,
    handler: Arc<crate::mcp_handler::RocksHandler>,
    config: ServerConfig,
) -> Result<(), String> {
    let protocol =
        ProtocolServer::from_shared(config, handler.clone()).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        let mut listener = unix::OwnerListener::bind(root)?;
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let stdio = async {
            let result = protocol
                .serve_stdio()
                .await
                .map_err(|error| error.to_string());
            let _ = stop.send(true);
            result
        };
        let (stdio, owner) = tokio::join!(stdio, listener.run(handler, stopped));
        stdio?;
        owner
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        protocol
            .serve_stdio()
            .await
            .map_err(|error| error.to_string())
    }
}

pub(super) fn transport_error(message: impl Into<String>) -> ToolError {
    ToolError::safe(ToolErrorCode::Unavailable, message).unwrap_or_else(|_| ToolError::internal())
}

pub(super) struct Shutdown {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Shutdown {
    pub(super) fn new() -> Result<Self, String> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let interrupt = signal(SignalKind::interrupt())
                .map_err(|_| "interrupt observer unavailable".to_owned())?;
            let terminate = signal(SignalKind::terminate())
                .map_err(|_| "termination observer unavailable".to_owned())?;
            Ok(Self {
                interrupt,
                terminate,
            })
        }
        #[cfg(not(unix))]
        {
            Err(
                "headless owner transport requires Unix; use serve --mcp on this platform"
                    .to_owned(),
            )
        }
    }

    #[cfg(unix)]
    async fn requested(&mut self) {
        tokio::select! { _ = self.interrupt.recv() => {}, _ = self.terminate.recv() => {} }
    }
}

#[cfg(unix)]
pub(super) async fn watch(
    root: &Path,
    handler: Arc<crate::mcp_handler::RocksHandler>,
    mut shutdown: Shutdown,
) -> Result<(), String> {
    let mut listener = unix::OwnerListener::bind(root)?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let owner = listener.run(handler, stopped);
    std::io::Write::write_all(
        &mut std::io::stdout().lock(),
        b"{\"owner\":\"ready\",\"autoSync\":true}\n",
    )
    .map_err(|_| "headless owner readiness output failed".to_owned())?;
    tokio::pin!(owner);
    tokio::select! {
        result = &mut owner => result,
        () = shutdown.requested() => {
            let _ = stop.send(true);
            owner.await
        }
    }
}

#[cfg(not(unix))]
pub(super) fn watch(
    root: &Path,
    handler: Arc<crate::mcp_handler::RocksHandler>,
    shutdown: Shutdown,
) -> impl Future<Output = Result<(), String>> {
    let _ = (root, handler, shutdown);
    std::future::ready(Err("headless owner transport requires Unix".to_owned()))
}
