//! Storage-neutral source synchronization contract for the existing watcher.

use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};

use cartograph_agent::{
    IndexFailure, IndexOptions, ProjectError, ProjectRuntime, RocksIndexOptions, RocksProjectError,
    RocksProjectRuntime,
};
use cartograph_db::rocks::{RocksError, StorageRequest};
use cartograph_domain::ContentDigest;

type SyncFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub(crate) struct SyncObservation {
    pub fresh: bool,
    pub live_source_revision: ContentDigest,
}

pub(crate) struct SyncIndexReport {
    pub published: bool,
    pub maintenance_failed: bool,
    pub incremental_files: u16,
    pub profile: Option<cartograph_agent::IndexProfile>,
}

pub(crate) trait SyncProject: Send + Sync {
    fn sync_root(&self) -> &Path;
    fn sync_status(self: Arc<Self>) -> SyncFuture<Result<SyncObservation, ProjectError>>;
    fn sync_index(self: Arc<Self>) -> SyncFuture<Result<SyncIndexReport, IndexFailure>>;
    fn sync_retention(self: Arc<Self>) -> SyncFuture<bool>;
}

impl SyncProject for ProjectRuntime {
    fn sync_root(&self) -> &Path {
        self.project_root_for_host_operations()
    }

    fn sync_status(self: Arc<Self>) -> SyncFuture<Result<SyncObservation, ProjectError>> {
        Box::pin(async move {
            let status = self.status().await?;
            Ok(SyncObservation {
                fresh: status.fresh,
                live_source_revision: status.live_source_revision,
            })
        })
    }

    fn sync_index(self: Arc<Self>) -> SyncFuture<Result<SyncIndexReport, IndexFailure>> {
        Box::pin(async move {
            let report = self
                .index_with_failure_detail(IndexOptions::automatic().with_profile(true))
                .await?;
            Ok(SyncIndexReport {
                published: report.published,
                incremental_files: report.incremental_files,
                profile: report.profile,
                maintenance_failed: report.retention.maintenance_failed(),
            })
        })
    }

    fn sync_retention(self: Arc<Self>) -> SyncFuture<bool> {
        Box::pin(async move {
            self.maintain_retention().await.as_ref().map_or(
                true,
                cartograph_agent::GenerationRetentionStatus::maintenance_failed,
            )
        })
    }
}

impl SyncProject for RocksProjectRuntime {
    fn sync_root(&self) -> &Path {
        self.project_root_for_host_operations()
    }

    fn sync_status(self: Arc<Self>) -> SyncFuture<Result<SyncObservation, ProjectError>> {
        Box::pin(async move {
            let request = request().map_err(project_error)?;
            let status = self.status(&request).await.map_err(project_error)?;
            Ok(SyncObservation {
                fresh: status.fresh,
                live_source_revision: status.live_source_revision,
            })
        })
    }

    fn sync_index(self: Arc<Self>) -> SyncFuture<Result<SyncIndexReport, IndexFailure>> {
        Box::pin(async move {
            let request = request().map_err(project_error)?;
            let report = self
                .index(RocksIndexOptions::default(), &request)
                .await
                .map_err(project_error)?;
            Ok(SyncIndexReport {
                published: !report.unchanged,
                incremental_files: report.incremental_files,
                profile: Some(report.profile),
                maintenance_failed: report.retention_error.is_some()
                    || report.parse_cache_retention_error.is_some()
                    || report.auxiliary_retention_error.is_some()
                    || report
                        .auxiliary_retention
                        .as_ref()
                        .is_some_and(cartograph_db::NativeAuxiliaryPruneReport::maintenance_failed),
            })
        })
    }

    fn sync_retention(self: Arc<Self>) -> SyncFuture<bool> {
        Box::pin(async move {
            let Ok(request) = StorageRequest::new(Duration::from_secs(30)) else {
                return true;
            };
            self.maintain_retention(&request).await.as_ref().map_or(
                true,
                cartograph_agent::RocksMaintenanceReport::maintenance_failed,
            )
        })
    }
}

fn request() -> Result<StorageRequest, RocksProjectError> {
    StorageRequest::new(Duration::from_secs(300)).map_err(Into::into)
}

fn project_error(error: RocksProjectError) -> ProjectError {
    match error {
        RocksProjectError::Source(error) => error,
        RocksProjectError::SourceChanged => ProjectError::SourceChangedDuringIndex,
        RocksProjectError::Storage(RocksError::WriterBusy) => ProjectError::IndexLeaseBusy,
        RocksProjectError::Storage(RocksError::Cancelled | RocksError::Closed) => {
            ProjectError::RequestCancelled
        }
        RocksProjectError::Storage(RocksError::Limit {
            resource: "operation_deadline",
        }) => ProjectError::IndexStageFailedWithReason {
            stage: cartograph_agent::PipelineStage::Publish,
            reason: cartograph_agent::PipelineFailureReason::DeadlineExceeded,
        },
        _ => ProjectError::IndexFailed,
    }
}
