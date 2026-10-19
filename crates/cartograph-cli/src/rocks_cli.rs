//! `RocksDB`-only routing and verified, project-local source rebuilds.

use std::{path::Path, process::ExitCode, sync::Arc, time::Duration};

use cartograph_agent::{RocksIndexOptions, RocksProjectRuntime};
use cartograph_config::{record_rocksdb_migration, storage_migration_guard};
use cartograph_db::rocks::StorageRequest;
use cartograph_mcp::ProtocolServer;
use clap::{Args, ValueEnum};
use serde_json::json;

use crate::{McpServeArguments, OutputFormat, print_serialized};

pub(super) mod owner;
pub(crate) mod remote_job;
pub(crate) mod service;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum MigrationTarget {
    Rocksdb,
}

#[derive(Debug, Args)]
pub(super) struct MigrationArguments {
    /// Verified destination. Existing PostgreSQL files and volumes are preserved.
    #[arg(long, value_enum)]
    to: MigrationTarget,
    /// Existing project root to rebuild with `RocksDB`.
    #[arg(long, default_value = ".")]
    project_path: std::path::PathBuf,
    /// Maximum native workers.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=16))]
    workers: Option<u16>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
}

// Validate migration state before dispatching the sole native storage path.
pub(super) async fn validate_project_storage(root: &Path) -> Result<(), String> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || storage_migration_guard(&root))
        .await
        .map_err(|_| "project storage configuration worker failed".to_owned())?
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn request() -> Result<StorageRequest, String> {
    StorageRequest::new(Duration::from_secs(300)).map_err(|error| error.to_string())
}

async fn index(
    runtime: &Arc<RocksProjectRuntime>,
    options: RocksIndexOptions,
    control: &StorageRequest,
) -> Result<cartograph_agent::RocksIndexReport, String> {
    let operation = runtime.index(options, control);
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result.map_err(|error| error.to_string()),
        signal = tokio::signal::ctrl_c() => {
            control.cancel();
            let result = operation.await.map_err(|error| error.to_string());
            match (signal, result) {
                (_, Err(error)) => Err(error),
                (Ok(()), Ok(report)) => Ok(report),
                (Err(_), Ok(_)) => Err("interrupt observer unavailable; indexing was drained".to_owned()),
            }
        }
    }
}

pub(super) async fn migrate(arguments: MigrationArguments) -> Result<ExitCode, String> {
    let control = request()?;
    let root = arguments.project_path.clone();
    let guard = tokio::task::spawn_blocking(move || storage_migration_guard(&root))
        .await
        .map_err(|_| "project storage configuration worker failed".to_owned())?
        .map_err(|error| error.to_string())?;
    let runtime = RocksProjectRuntime::open(&arguments.project_path, &control)
        .await
        .map_err(|error| error.to_string())?;
    let result = async {
        let report = index(&runtime, RocksIndexOptions { workers: arguments.workers, ..RocksIndexOptions::default() }, &control).await?;
        let status = runtime.status(&control).await.map_err(|error| error.to_string())?;
        if !report.fresh || !status.fresh || status.current.as_ref().is_none_or(|revision| revision.generation_id != report.revision.generation_id) {
            return Err("RocksDB destination source verification failed; migration marker was preserved".to_owned());
        }
        // Exercise a real typed query before recording migration completion.
        runtime.query(cartograph_agent::RocksQuery::Files { directory: None, limit: 1 }, false, &control).await.map_err(|error| error.to_string())?;
        let root = arguments.project_path.clone();
        tokio::task::spawn_blocking(move || record_rocksdb_migration(&root, guard)).await.map_err(|_| "project migration marker worker failed".to_owned())?.map_err(|error| error.to_string())?;
        print_serialized(&json!({"completed": true, "storage": "rocksdb", "method": "native_source_rebuild", "postgresqlDataPreserved": true, "migrationRecorded": true, "report": report}), arguments.format)?;
        Ok(ExitCode::SUCCESS)
    }.await;
    let shutdown = runtime.shutdown().await.map_err(|error| error.to_string());
    match result {
        Err(error) => Err(error),
        Ok(code) => {
            shutdown?;
            Ok(code)
        }
    }
}

pub(super) async fn initialize_project(
    root: &Path,
    format: OutputFormat,
) -> Result<ExitCode, String> {
    let control = request()?;
    let runtime = RocksProjectRuntime::open(root, &control)
        .await
        .map_err(|error| error.to_string())?;
    let result = async {
        let report = index(&runtime, RocksIndexOptions::default(), &control).await?;
        if !report.fresh {
            return Err(
                "initial RocksDB graph is not current; run cartograph index again".to_owned(),
            );
        }
        print_serialized(&json!({"storage": "rocksdb", "report": report}), format)?;
        Ok(ExitCode::SUCCESS)
    }
    .await;
    let shutdown = runtime.shutdown().await.map_err(|error| error.to_string());
    match result {
        Err(error) => Err(error),
        Ok(code) => {
            shutdown?;
            Ok(code)
        }
    }
}

pub(super) async fn serve(arguments: McpServeArguments) -> Result<ExitCode, String> {
    if !arguments.mcp {
        return Err("serve requires --mcp".to_owned());
    }
    let read_only = arguments.no_write_tools || arguments.profile == crate::McpProfile::ReadOnly;
    let config =
        crate::mcp_server_config(arguments.profile, arguments.disable_tool.clone(), read_only)?;
    if owner::OwnerClient::connect(&arguments.project_path)
        .await?
        .is_some()
    {
        let mut definitions = crate::mcp_handler::RocksHandler::definitions(read_only)
            .map_err(|error| error.to_string())?;
        definitions.retain(|tool| {
            tool.included_in(arguments.profile.into())
                && !arguments
                    .disable_tool
                    .iter()
                    .any(|name| name == tool.name())
        });
        let proxy = owner::OwnerProxy::new(&arguments.project_path, definitions, read_only);
        ProtocolServer::new(config, proxy)
            .map_err(|error| error.to_string())?
            .serve_stdio()
            .await
            .map_err(|error| error.to_string())?;
        return Ok(ExitCode::SUCCESS);
    }
    let runtime = RocksProjectRuntime::open(&arguments.project_path, &request()?)
        .await
        .map_err(|error| error.to_string())?;
    let mut handler = crate::mcp_handler::RocksHandler::new(runtime, read_only)
        .map_err(|error| error.to_string())?
        .restrict(arguments.profile.into(), &arguments.disable_tool)
        .with_defaults(
            arguments.defaults.allow_stale,
            arguments.defaults.low_tokens,
        );
    if let crate::McpServeSync::Enabled {
        startup_reconciliation,
    } = arguments.sync
    {
        handler = handler.with_synchronization(startup_reconciliation).await?;
    }
    let handler = Arc::new(handler);
    let result = owner::serve(&arguments.project_path, handler.clone(), config).await;
    let shutdown = handler.close().await.map_err(|error| error.to_string());
    result?;
    shutdown?;
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn watch(root: &Path) -> Result<ExitCode, String> {
    // Register the signal observers before starting any writer or watcher.
    let shutdown = owner::Shutdown::new()?;
    if owner::OwnerClient::connect(root).await?.is_some() {
        return Err(
            "another native project owner is active; the login service will retry after it exits"
                .to_owned(),
        );
    }
    let runtime = RocksProjectRuntime::open(root, &request()?)
        .await
        .map_err(|error| error.to_string())?;
    let handler = Arc::new(
        crate::mcp_handler::RocksHandler::new(runtime, false)
            .map_err(|error| error.to_string())?
            .with_synchronization(true)
            .await?,
    );
    let result = owner::watch(root, handler.clone(), shutdown).await;
    let closed = handler.close().await.map_err(|error| error.to_string());
    result?;
    closed?;
    Ok(ExitCode::SUCCESS)
}
