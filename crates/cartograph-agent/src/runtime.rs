//! The shared feature runtime borrows one native project owner.

use std::future::Future;

use super::{
    Arc, CartographDatabase, ContentDigest, Duration, GenerationDigestVersion,
    GenerationRetentionStatus, HistoryIndexError, HistoryIndexOptions, HistoryIndexStatus,
    IndexAdmissionReconciliation, IndexFailure, IndexLiveSource, IndexOptions, IndexPublication,
    IndexReport, Instant, IssueHistoryIndexError, IssueHistoryIndexOptions,
    IssueHistoryIndexRequest, IssueHistoryIndexStatus, MAX_CONCURRENT_SOURCE_SCANS,
    NativeBuildProgress, Path, PathBuf, PipelineFailureReason, PipelineStage, ProjectCancellation,
    ProjectError, ProjectId, ProjectSourceIdentity, ProjectStatus, RocksIndexOptions,
    RocksMaintenanceReport, RocksProjectError, RocksProjectRuntime, Semaphore, SourceDriftPolicy,
    SourceScanCounters, history_unavailable_reason, issue_history_unavailable_reason,
    load_project_source_settings, monotonic_millis, project_identity_digest, project_root_identity,
    source_revision,
};
use cartograph_db::rocks::{RocksError, StorageRequest};

/// Project-scoped graph, source and auxiliary services sharing one `RocksDB` owner.
pub struct ProjectRuntime {
    pub(super) root: PathBuf,
    pub(super) root_identity: String,
    pub(super) repository_fingerprint: ContentDigest,
    pub(super) database: CartographDatabase,
    pub(super) native: Arc<RocksProjectRuntime>,
    pub(super) source_scan_permits: Arc<Semaphore>,
    pub(super) source_scans: Arc<SourceScanCounters>,
}

impl ProjectRuntime {
    /// Open the project's embedded database and its bounded storage workers.
    /// # Errors
    /// Rejects unsafe ownership, an occupied primary, invalid policies or native startup failures.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let request = StorageRequest::new(Duration::from_secs(30)).map_err(native_storage_error)?;
        Self::open_with_request(root, &request).await
    }

    /// Open while retaining the caller's cancellation and remaining deadline.
    /// # Errors
    /// Rejects stopped requests, unsafe ownership or bounded native startup failure.
    pub async fn open_with_request(
        root: impl AsRef<Path>,
        request: &StorageRequest,
    ) -> Result<Self, ProjectError> {
        RocksProjectRuntime::open(root, request)
            .await
            .map(Self::from_native)
            .map_err(native_error)
    }

    /// Share the already-opened owner with the complete typed feature service.
    #[must_use]
    pub fn from_native(native: Arc<RocksProjectRuntime>) -> Self {
        let root = native.project_root_for_host_operations().to_path_buf();
        let repository_fingerprint = project_identity_digest(&root);
        let root_identity = project_root_identity(&repository_fingerprint);
        Self {
            root,
            root_identity,
            repository_fingerprint,
            database: native.database().clone(),
            native,
            source_scan_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SOURCE_SCANS)),
            source_scans: Arc::default(),
        }
    }

    /// Inspect a source checkout without opening a database.
    /// # Errors
    /// Rejects a missing root or incomplete bounded source manifest.
    pub fn inspect_source_identity(
        root: impl AsRef<Path>,
    ) -> Result<ProjectSourceIdentity, ProjectError> {
        let root = std::fs::canonicalize(root).map_err(|_| ProjectError::ProjectRootUnavailable)?;
        if !root.is_dir() {
            return Err(ProjectError::ProjectRootUnavailable);
        }
        let source = source_revision(&root)?;
        Ok(ProjectSourceIdentity {
            repository_fingerprint: project_identity_digest(&root),
            source_revision: source.digest,
            v1_source_manifest: source.v1_source_manifest,
            files: source.files,
            source_bytes: source.source_bytes,
        })
    }

    /// Stable project identity is available before the first index.
    /// # Errors
    /// A cancelled or closed owner cannot register new agent state.
    pub fn register_agent_state_project(
        &self,
    ) -> impl Future<Output = Result<ProjectId, ProjectError>> {
        std::future::ready(
            CartographDatabase::request_control(Duration::from_secs(30))
                .map_err(|_| ProjectError::RequestCancelled)
                .map(|_| self.database.project_id().clone()),
        )
    }

    /// Compare the complete current publication with live source bytes.
    /// # Errors
    /// Rejects failed native reads, source scans or cancellation.
    pub async fn status(&self) -> Result<ProjectStatus, ProjectError> {
        self.status_with_cancellation(ProjectCancellation::new())
            .await
    }

    /// Compare live source using the publication's exact source admission policy.
    /// # Errors
    /// Rejects failed bounded native reads, source scans or requested cancellation.
    pub async fn status_with_cancellation(
        &self,
        cancellation: ProjectCancellation,
    ) -> Result<ProjectStatus, ProjectError> {
        let request = CartographDatabase::request_control(Duration::from_secs(30))
            .map_err(|_| ProjectError::StatusFailed)?;
        let operation = self.native.status(&request);
        tokio::pin!(operation);
        let source = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                request.cancel();
                operation.await
            },
            result = &mut operation => result,
        }
        .map_err(native_error)?;
        let snapshot = self
            .database
            .project_snapshot_by_root(&self.root_identity)
            .await
            .map_err(|_| ProjectError::StatusFailed)?;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let fresh = snapshot
            .as_ref()
            .and_then(|project| project.current.as_ref())
            .is_some_and(|current| {
                current.source_revision == source.live_source_revision.as_str()
                    && current.digest_version == GenerationDigestVersion::CURRENT
            });
        Ok(ProjectStatus {
            snapshot,
            live_source_revision: source.live_source_revision,
            fresh,
        })
    }

    /// Publish a complete graph or reuse the exact current revision.
    /// # Errors
    /// Rejects invalid policies, exhausted bounds, changed source or publication failure.
    pub async fn index(&self, options: IndexOptions) -> Result<IndexReport, ProjectError> {
        self.index_with_cancellation(options, ProjectCancellation::new())
            .await
    }

    /// Cancel source, worker and storage work through the native owner's retained operation.
    /// # Errors
    /// Preserves the previous complete graph if this request cannot publish.
    pub async fn index_with_cancellation(
        &self,
        options: IndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<IndexReport, ProjectError> {
        self.index_with_cancellation_detail(options, cancellation)
            .await
            .map_err(IndexFailure::into_error)
    }

    /// Publish while preserving independent cleanup errors.
    /// # Errors
    /// Returns the primary failure and any cleanup failure retained by native publication.
    pub async fn index_with_failure_detail(
        &self,
        options: IndexOptions,
    ) -> Result<IndexReport, IndexFailure> {
        self.index_with_cancellation_detail(options, ProjectCancellation::new())
            .await
    }

    /// Publish one bounded native operation and retain auxiliary outcomes separately.
    /// # Errors
    /// Returns source, admission, worker, cancellation or publication failures.
    pub async fn index_with_cancellation_detail(
        &self,
        options: IndexOptions,
        cancellation: ProjectCancellation,
    ) -> Result<IndexReport, IndexFailure> {
        let started = Instant::now();
        let request = CartographDatabase::request_control(Duration::from_secs(300))
            .map_err(|_| ProjectError::RequestCancelled)?;
        for attempt in 0..3 {
            let result = loop {
                match self
                    .index_once(options.clone(), cancellation.clone(), &request)
                    .await
                {
                    Err(failure) if matches!(failure.error(), ProjectError::IndexLeaseBusy) => {
                        // A watcher can have published before completing its
                        // maintenance. Wait within the original request rather
                        // than failing an explicit no-op reconciliation.
                        tokio::select! {
                            biased;
                            () = cancellation.cancelled() => return Err(ProjectError::RequestCancelled.into()),
                            error = request.stopped() => return Err(native_storage_error(error).into()),
                            () = tokio::time::sleep(Duration::from_millis(50)) => {},
                        }
                    }
                    result => break result,
                }
            };
            match result {
                Ok(mut report) => {
                    if options.refresh_history {
                        self.attach_history(&mut report, cancellation).await;
                    }
                    if let Some(profile) = report.profile.as_mut() {
                        profile.total_millis = monotonic_millis(started.elapsed());
                    }
                    return Ok(report);
                }
                Err(failure)
                    if options.source_drift == SourceDriftPolicy::Rebuild
                        && attempt < 2
                        && matches!(failure.error(), ProjectError::SourceChangedDuringIndex) => {}
                Err(failure) => return Err(failure),
            }
        }
        Err(ProjectError::SourceChangedDuringIndex.into())
    }

    async fn index_once(
        &self,
        options: IndexOptions,
        cancellation: ProjectCancellation,
        request: &StorageRequest,
    ) -> Result<IndexReport, IndexFailure> {
        let progress = NativeBuildProgress::default();
        cancellation.attach_index_supervisor(progress.clone());
        let input = RocksIndexOptions {
            workers: Some(options.max_workers),
            force: options.force,
            additional_excludes: if options.admission_reconciliation
                == IndexAdmissionReconciliation::PreserveCurrent
                && options.additional_excludes.is_empty()
            {
                None
            } else {
                Some(options.additional_excludes.clone())
            },
            maximum_file_bytes: options.max_source_bytes,
            progress: Some(progress),
        };
        let operation = self.native.index(input, request);
        tokio::pin!(operation);
        let report = tokio::select! {
            biased;
            ()=cancellation.cancelled()=>{request.cancel(); operation.await.map_err(index_error)?},
            result=&mut operation=>result.map_err(index_error)?,
        };
        let output = IndexReport {
            incremental_files: report.incremental_files,
            project_id: self.database.project_id().clone(),
            generation_id: report.revision.generation_id,
            source_revision: report.revision.source_revision,
            parse_cache_contract_digest: report.parse_cache_contract_digest,
            content_digest: report
                .revision
                .content_digest
                .ok_or(ProjectError::IndexPublicationFailed)?,
            workers: report.workers,
            published: !report.unchanged,
            publication: if report.unchanged {
                IndexPublication::Skipped {
                    reason: "unchanged_source",
                    generation_remains_current: true,
                }
            } else {
                IndexPublication::Published
            },
            native: report.native,
            history: HistoryIndexStatus::Unavailable {
                reason: "not_requested",
            },
            issue_history: IssueHistoryIndexStatus::Unavailable {
                reason: "not_requested",
            },
            profile: options.profile.then_some(report.profile),
            retention: GenerationRetentionStatus::Attempted {
                report: RocksMaintenanceReport {
                    graph: report.retention_progress,
                    graph_error: report.retention_error,
                    parse_cache: report.parse_cache_retention,
                    parse_cache_error: report.parse_cache_retention_error,
                    auxiliary: report.auxiliary_retention,
                    auxiliary_error: report.auxiliary_retention_error,
                },
            },
            live_source: if report.fresh {
                IndexLiveSource::Matched
            } else if report.source_verification_error.is_some() {
                IndexLiveSource::Unverified
            } else {
                IndexLiveSource::ChangedAfterPublication
            },
        };
        if output.live_source != IndexLiveSource::Matched
            && options.source_drift == SourceDriftPolicy::Rebuild
        {
            return Err(ProjectError::SourceChangedDuringIndex.into());
        }
        Ok(output)
    }

    async fn attach_history(&self, report: &mut IndexReport, cancellation: ProjectCancellation) {
        let started = Instant::now();
        let history = async {
            let settings = load_project_source_settings(&self.root)
                .map_err(|_| HistoryIndexError::InvalidOptions)?;
            let channels = (settings.enable_churn(), settings.enable_co_change());
            if !channels.0 && !channels.1 {
                self.database
                    .clear_file_history(&report.project_id)
                    .await
                    .map_err(|_| HistoryIndexError::StorageUnavailable)?;
                return Err(HistoryIndexError::DisabledByProjectConfig);
            }
            let prepared = self
                .prepare_git_history_reusing(
                    HistoryIndexOptions::default(),
                    channels,
                    cancellation.clone(),
                )
                .await?;
            self.persist_git_history(report.project_id.clone(), prepared)
                .await
        }
        .await;
        report.history = match history {
            Ok(report) => HistoryIndexStatus::Indexed { report },
            Err(error) => HistoryIndexStatus::Unavailable {
                reason: history_unavailable_reason(error),
            },
        };
        if let Some(profile) = report.profile.as_mut() {
            profile.history_millis = monotonic_millis(started.elapsed());
        }
        let started = Instant::now();
        let issue_history = async {
            let settings = load_project_source_settings(&self.root)
                .map_err(|_| IssueHistoryIndexError::InvalidOptions)?;
            if !settings.enable_issue_history() {
                self.database
                    .clear_issue_history(&report.project_id, &report.generation_id)
                    .await
                    .map_err(|_| IssueHistoryIndexError::StorageUnavailable)?;
                return Err(IssueHistoryIndexError::DisabledByProjectConfig);
            }
            self.refresh_issue_history(IssueHistoryIndexRequest {
                project_id: report.project_id.clone(),
                generation_id: report.generation_id.clone(),
                options: IssueHistoryIndexOptions::default(),
                cancellation,
            })
            .await
        }
        .await;
        report.issue_history = match issue_history {
            Ok(report) => IssueHistoryIndexStatus::Indexed { report },
            Err(error) => IssueHistoryIndexStatus::Unavailable {
                reason: issue_history_unavailable_reason(error),
            },
        };
        if let Some(profile) = report.profile.as_mut() {
            profile.issue_history_millis = monotonic_millis(started.elapsed());
        }
    }

    /// Advance independent graph, parse-cache and auxiliary retention budgets.
    /// # Errors
    /// Returns failed admission or shutdown while preserving committed partial cleanup.
    pub async fn maintain_retention(&self) -> Result<GenerationRetentionStatus, ProjectError> {
        let request = StorageRequest::new(Duration::from_secs(30)).map_err(native_storage_error)?;
        self.native
            .maintain_retention(&request)
            .await
            .map(|report| GenerationRetentionStatus::Attempted { report })
            .map_err(native_error)
    }

    /// Drain native operations and close storage workers before releasing the project.
    /// # Errors
    /// Retains incomplete ownership for a later shutdown retry.
    pub async fn shutdown(&self) -> Result<(), ProjectError> {
        self.native.shutdown().await.map_err(native_error)
    }

    /// Drain the owned runtime before a command exits.
    /// # Errors
    /// Returns a native shutdown or outstanding-operation failure.
    pub async fn close(self) -> Result<(), ProjectError> {
        self.shutdown().await
    }
}

pub(super) fn native_error(error: RocksProjectError) -> ProjectError {
    match error {
        RocksProjectError::Source(error) => error,
        RocksProjectError::SourceChanged => ProjectError::SourceChangedDuringIndex,
        RocksProjectError::Storage(error) => native_storage_error(error),
        RocksProjectError::Native(cartograph_indexer::NativeRocksGraphError::Storage(error)) => {
            native_storage_error(error)
        }
        RocksProjectError::Native(cartograph_indexer::NativeRocksGraphError::Native(error)) => {
            if matches!(
                error,
                cartograph_indexer::NativePipelineError::Stage(
                    cartograph_indexer::StageRunError::Cancelled { .. }
                )
            ) {
                return ProjectError::RequestCancelled;
            }
            if let Some(failure) = error.file_failure().cloned() {
                return ProjectError::IndexStageFileFailed {
                    stage: error.stage(),
                    failure,
                };
            }
            ProjectError::IndexStageFailedWithReason {
                stage: error.stage(),
                reason: error
                    .reason()
                    .unwrap_or(PipelineFailureReason::FileProcessingFailed),
            }
        }
        RocksProjectError::Publication(error) => native_storage_error(error.primary),
        _ => ProjectError::IndexFailed,
    }
}
fn index_error(error: RocksProjectError) -> IndexFailure {
    let cleanup_failed =
        matches!(&error,RocksProjectError::Publication(error) if error.cleanup_failure.is_some());
    let failure = IndexFailure::from(native_error(error));
    if cleanup_failed {
        failure.with_failed_cleanup()
    } else {
        failure
    }
}
fn native_storage_error(error: RocksError) -> ProjectError {
    match error {
        RocksError::Cancelled | RocksError::Closed => ProjectError::RequestCancelled,
        RocksError::WriterBusy => ProjectError::IndexLeaseBusy,
        RocksError::GenerationChanged => ProjectError::SourceChangedDuringIndex,
        RocksError::Limit {
            resource: "operation_deadline",
        } => ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Reduce,
            reason: PipelineFailureReason::DeadlineExceeded,
        },
        RocksError::Limit { .. } => ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Reduce,
            reason: PipelineFailureReason::GenerationCapacityExceeded,
        },
        RocksError::Operation {
            operation: "operation_deadline",
        } => ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Publish,
            reason: PipelineFailureReason::DeadlineExceeded,
        },
        _ => ProjectError::IndexFailed,
    }
}

impl ProjectRuntime {
    /// Run one bounded native retention batch, keeping the current generation and readers safe.
    /// # Errors
    /// Rejects invalid limits, concurrent writers, cancellation or storage failure.
    pub async fn prune_generations(
        &self,
        keep: u8,
        generations: u16,
        versions: u32,
        cancellation: ProjectCancellation,
    ) -> Result<cartograph_db::rocks::PruneReport, ProjectError> {
        let request = StorageRequest::new(Duration::from_secs(30)).map_err(native_storage_error)?;
        let operation = self
            .native
            .prune_generations(keep, generations, versions, &request);
        tokio::pin!(operation);
        tokio::select! {
            biased;
            ()=cancellation.cancelled()=>{request.cancel();operation.await.map_err(native_error)},
            result=&mut operation=>result.map_err(native_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_native_request_keeps_deadline_classification()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = StorageRequest::new(Duration::from_nanos(1))?;
        std::thread::sleep(Duration::from_millis(1));
        let expected = ProjectError::IndexStageFailedWithReason {
            stage: PipelineStage::Reduce,
            reason: PipelineFailureReason::DeadlineExceeded,
        };
        assert_eq!(
            native_storage_error(request.check().err().ok_or("request unexpectedly active")?),
            expected
        );
        let error = cartograph_indexer::NativeRocksGraphError::Storage(
            request.check().err().ok_or("request unexpectedly active")?,
        );
        assert_eq!(native_error(RocksProjectError::Native(error)), expected);
        assert_eq!(
            native_error(RocksProjectError::Native(
                cartograph_indexer::NativeRocksGraphError::Storage(RocksError::Cancelled)
            )),
            ProjectError::RequestCancelled
        );
        Ok(())
    }
}
