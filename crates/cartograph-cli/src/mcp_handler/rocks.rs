//! Native ownership adapter around the complete typed CLI/MCP service.
use super::{
    Arc, BoxShutdownFuture, BoxToolFuture, CartographMcpHandler, HandlerDefaults,
    HandlerExecutionDefaults, ProjectRuntime, ToolCall, ToolCallContext, ToolContractError,
    ToolDefinition, ToolHandler,
};
use cartograph_agent::{ProjectError, RocksProjectRuntime};

pub(crate) struct RocksHandler {
    handler: CartographMcpHandler,
}
impl RocksHandler {
    pub(crate) fn new(
        runtime: Arc<RocksProjectRuntime>,
        read_only: bool,
    ) -> Result<Self, ToolContractError> {
        let handler = CartographMcpHandler::new(Arc::new(ProjectRuntime::from_native(runtime)))?
            .configured(HandlerDefaults {
                execution: HandlerExecutionDefaults {
                    read_only,
                    trace_calls: !read_only,
                },
                ..HandlerDefaults::default()
            });
        Ok(Self { handler })
    }
    pub(crate) fn definitions(_read_only: bool) -> Result<Vec<ToolDefinition>, ToolContractError> {
        super::tool_definitions()
    }
    pub(crate) fn restrict(
        mut self,
        profile: cartograph_mcp::ToolProfile,
        disabled: &[String],
    ) -> Self {
        self.handler.definitions.retain(|tool| {
            tool.included_in(profile) && !disabled.iter().any(|name| name == tool.name())
        });
        self
    }
    pub(crate) fn with_defaults(mut self, allow_stale: bool, low_tokens: bool) -> Self {
        self.handler.defaults.evidence = super::HandlerEvidenceDefaults {
            allow_stale,
            low_tokens,
        };
        self
    }
    pub(crate) async fn with_synchronization(self, startup: bool) -> Result<Self, String> {
        super::enable_handler_auto_sync(&self.handler, startup)
            .await
            .map_err(|error| error.to_string())?;
        Ok(self)
    }
    pub(crate) async fn close(&self) -> Result<(), ProjectError> {
        self.handler.admin_jobs.shutdown().await;
        self.handler.project_cache.shutdown().await
    }
}
impl ToolHandler for RocksHandler {
    fn drain_cancelled_calls(&self) -> bool {
        self.handler.drain_cancelled_calls()
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        self.handler.tools()
    }
    fn call(&self, call: ToolCall, context: ToolCallContext) -> BoxToolFuture<'_> {
        Box::pin(async move {
            if !self
                .handler
                .definitions
                .iter()
                .any(|tool| tool.name() == call.name)
            {
                return Err(super::safe_error(
                    super::ToolErrorCode::Unavailable,
                    "tool is outside this host's authorization ceiling",
                ));
            }
            self.handler.call(call, context).await
        })
    }
    fn shutdown(&self) -> BoxShutdownFuture<'_> {
        // Transport drains jobs and watchers; the hosting owner checks storage shutdown.
        Box::pin(async move {
            self.handler.admin_jobs.shutdown().await;
            self.handler.project_cache.stop_watchers().await;
        })
    }
}

#[cfg(test)]
#[path = "rocks_tests.rs"]
mod tests;
