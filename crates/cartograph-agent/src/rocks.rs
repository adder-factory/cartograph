//! Native project service using the existing supervised extractor and `RocksDB` owner.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use cartograph_db::rocks::{
    AsyncProjectStore, GraphBase, GraphInfo, GraphPolicy, GraphPublicationError,
    GraphPublicationInput, GraphRetention, GraphRevision, GraphWriteReport, PruneReport,
    ReadBudget, RocksError, StorageRequest, StorageRuntime, StorePolicy, StoredRevision,
    WorkerPolicy,
};
use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId, NormalizedPath};
use cartograph_extract::SourceRoot;
use cartograph_indexer::{
    NativeBuildProgress, NativeGenerationBuild, NativeParseCache, NativePipelineReport,
    NativeRocksGraphError, build_native_rocks_graph_observed,
};
use serde::Serialize;
use tokio::sync::{Mutex, Semaphore};

use crate::{
    DEFAULT_MAX_GENERATION_BYTES, DEFAULT_MAX_SOURCE_BYTES, IndexProfile, NativeIndexMetrics,
    PipelineBounds, ProjectError, ProjectSourcePolicy, SourceRevision, SourceRevisionRequest,
    finish_source_manifest, monotonic_millis, pipeline_config, project_identity_digest,
    project_source_policy_with_excludes, run_source_worker, select_worker_count,
    source_revision_with_options,
};

#[cfg(test)]
mod cache_tests;
mod incremental;
mod operations;
mod query;
pub use query::{RocksQuery, RocksQueryResult};

/// Public failures preserve native errors without source, credentials, or paths.
#[derive(Debug, thiserror::Error)]
pub enum RocksProjectError {
    /// Shared source discovery or policy failure.
    #[error(transparent)]
    Source(#[from] ProjectError),
    /// Native storage admission or persistence failure.
    #[error(transparent)]
    Storage(#[from] RocksError),
    /// Native source-to-canonical build failure.
    #[error(transparent)]
    Native(#[from] NativeRocksGraphError),
    /// Publication and independent cleanup diagnostics.
    #[error(transparent)]
    Publication(#[from] GraphPublicationError),
    /// Source evidence cannot be represented as current.
    #[error("project source changed; run cartograph index and retry")]
    SourceChanged,
    /// This backend has no complete published graph.
    #[error("project has no RocksDB graph; run cartograph index")]
    NotIndexed,
    /// Invalid or unsupported query contract.
    #[error("RocksDB query arguments are invalid or unsupported")]
    InvalidQuery,
}

/// Explicit native index policy. Source exclusions remain in shared project config.
#[derive(Clone, Debug, Default)]
pub struct RocksIndexOptions {
    /// Maximum native workers, bounded to 1 through 16.
    pub workers: Option<u16>,
    /// Replace a complete generation even when its exact source is unchanged.
    pub force: bool,
    /// Replace run-scoped exclusions; `None` inherits the published policy.
    pub additional_excludes: Option<Vec<String>>,
    /// Source-file ceiling, also bounded by project configuration.
    pub maximum_file_bytes: Option<usize>,
    /// One-shot progress shared with an attached job host.
    pub progress: Option<NativeBuildProgress>,
}

impl RocksIndexOptions {
    fn source_admission(
        &self,
        inherited: Option<&StoredRevision>,
    ) -> Result<(Vec<String>, Option<usize>), RocksProjectError> {
        if self
            .workers
            .is_some_and(|workers| !(1..=16).contains(&workers))
        {
            return Err(RocksProjectError::InvalidQuery);
        }
        let excludes = self.additional_excludes.clone().unwrap_or_else(|| {
            inherited.map_or_else(Vec::new, |revision| revision.run_excludes.clone())
        });
        let ceiling = if self.additional_excludes.is_some() {
            self.maximum_file_bytes
        } else {
            self.maximum_file_bytes
                .or_else(|| inherited.and_then(|revision| revision.maximum_file_bytes))
        };
        Ok((excludes, ceiling))
    }
}

/// Exact current revision and its complete live-source comparison.
#[derive(Clone, Debug, Serialize)]
pub struct RocksStatus {
    /// Active native runtime.
    pub storage: &'static str,
    /// Native engine version.
    pub engine_version: &'static str,
    /// Complete current revision, if initialized.
    pub current: Option<StoredRevision>,
    /// Canonical graph counts, without hidden source/config values.
    pub counts: Option<GraphInfo>,
    /// Current complete source and source-policy identity.
    pub live_source_revision: ContentDigest,
    /// Current source and extraction contract both match the current revision.
    pub fresh: bool,
}

/// Native index outcome, preserving maintenance and post-publication freshness.
#[derive(Clone, Debug, Serialize)]
pub struct RocksIndexReport {
    /// Complete extraction metrics; absent when no build was necessary.
    pub native: Option<NativeIndexMetrics>,
    /// Selected extraction parallelism; zero when no extraction ran.
    pub workers: u16,
    /// Exact parsing-policy identity used by reusable file extraction.
    pub parse_cache_contract_digest: ContentDigest,
    /// Actual monotonic preparation, stage and total wall times.
    pub profile: IndexProfile,
    /// Exact extraction reuse from this build; absent for an unchanged generation.
    pub parse_cache: Option<cartograph_indexer::NativeParseCacheReport>,
    /// Independent extraction-cache cleanup outcome.
    pub parse_cache_retention: Option<cartograph_db::NativeParseCacheRetentionReport>,
    /// Redacted extraction-cache maintenance error, independent of graph publication.
    pub parse_cache_retention_error: Option<String>,
    /// Committed obsolete auxiliary-snapshot cleanup progress.
    pub auxiliary_retention: Option<cartograph_db::NativeAuxiliaryPruneReport>,
    /// Redacted auxiliary maintenance error, independent of graph publication.
    pub auxiliary_retention_error: Option<String>,
    /// Whether the existing exact generation was reused.
    pub unchanged: bool,
    /// File-local replacements that reused proven unchanged resolution inputs.
    /// Zero means either a complete build or an unchanged reconciliation.
    pub incremental_files: u16,
    /// Complete committed revision.
    pub revision: StoredRevision,
    /// Canonical row counts.
    pub counts: GraphInfo,
    /// Record-level comparison and durable write totals.
    pub writes: GraphWriteReport,
    /// Source was revalidated after publication.
    pub fresh: bool,
    /// The bounded retention traversal reached its end, independently of publication.
    pub retention_completed: Option<bool>,
    /// Committed progress from the bounded maintenance batch.
    pub retention_progress: Option<PruneReport>,
    /// Optional redacted maintenance diagnostic.
    pub retention_error: Option<String>,
    /// Source verification may fail after a successful durable publication.
    pub source_verification_error: Option<String>,
}

/// Independent native maintenance outcomes; a partial success remains explicit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RocksMaintenanceReport {
    /// Committed graph-retention progress.
    pub graph: Option<PruneReport>,
    /// Safe diagnostic when graph retention did not complete.
    pub graph_error: Option<String>,
    /// Committed parse-cache retention outcome.
    pub parse_cache: Option<cartograph_db::NativeParseCacheRetentionReport>,
    /// Safe diagnostic when parse-cache retention failed.
    pub parse_cache_error: Option<String>,
    /// Committed auxiliary cleanup progress.
    pub auxiliary: Option<cartograph_db::NativeAuxiliaryPruneReport>,
    /// Safe diagnostic when auxiliary cleanup failed.
    pub auxiliary_error: Option<String>,
}

impl RocksMaintenanceReport {
    /// Whether maintenance should back off after an actual failure.
    #[must_use]
    pub const fn maintenance_failed(&self) -> bool {
        self.graph_error.is_some()
            || self.parse_cache_error.is_some()
            || self.auxiliary_error.is_some()
            || match self.auxiliary.as_ref() {
                Some(report) => report.maintenance_failed(),
                None => false,
            }
    }
}

struct IndexCompletion {
    native: Option<NativePipelineReport>,
    workers: u16,
    incremental_files: u16,
    cache_contract: ContentDigest,
    publication: cartograph_db::rocks::GraphPublication,
    counts: GraphInfo,
    profile: IndexProfile,
    started: std::time::Instant,
}

struct PreparedGraph {
    input: GraphPublicationInput,
    native: Option<NativePipelineReport>,
    workers: u16,
    incremental_files: u16,
}

struct GraphBuild<'a> {
    source: SourceRevision,
    policy: ProjectSourcePolicy,
    options: &'a RocksIndexOptions,
    base: GraphBase,
    request: &'a StorageRequest,
    progress: &'a NativeBuildProgress,
    excludes: &'a [String],
    ceiling: Option<usize>,
    publication_policy: GraphPolicy,
}

fn graph_publication_policy() -> GraphPolicy {
    GraphPolicy {
        maximum_input_bytes: 352 * 1_048_576,
        ..GraphPolicy::default()
    }
}

#[cfg(test)]
struct SourceScanPause {
    entered: tokio::sync::Notify,
    gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    finished: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl SourceScanPause {
    fn wait(&self) -> Result<(), ProjectError> {
        self.entered.notify_one();
        let released = self
            .gate
            .0
            .lock()
            .map_err(|_| ProjectError::SourceScanFailed)?;
        let (released, _) = self
            .gate
            .1
            .wait_timeout_while(released, Duration::from_secs(10), |released| !*released)
            .map_err(|_| ProjectError::SourceScanFailed)?;
        self.finished
            .store(true, std::sync::atomic::Ordering::Release);
        if *released {
            Ok(())
        } else {
            Err(ProjectError::SourceScanFailed)
        }
    }
}

/// Project lifetime owns one bounded storage runtime; adapters explicitly drain it.
pub struct RocksProjectRuntime {
    root: PathBuf,
    storage: StorageRuntime,
    store: AsyncProjectStore,
    database: cartograph_db::NativeDatabase,
    scans: Arc<Semaphore>,
    #[cfg(test)]
    source_pause: std::sync::Mutex<Option<Arc<SourceScanPause>>>,
    indexing: Mutex<Option<incremental::WarmGraph>>,
    operations: std::sync::Mutex<operations::OperationRegistry>,
}

impl RocksProjectRuntime {
    /// Typed services sharing this project's existing native storage owner.
    #[must_use]
    pub const fn database(&self) -> &cartograph_db::NativeDatabase {
        &self.database
    }
    /// Canonical root for the owning host's source watcher and local transport.
    /// This path must not be included in public evidence or diagnostics.
    #[must_use]
    pub fn project_root_for_host_operations(&self) -> &Path {
        &self.root
    }

    /// Open this project's owned native database without resolving PostgreSQL.
    ///
    /// # Errors
    /// Rejects unsafe ownership, native primary lock contention, or invalid policy.
    pub async fn open(
        root: impl AsRef<Path>,
        request: &StorageRequest,
    ) -> Result<Arc<Self>, RocksProjectError> {
        request.check()?;
        let root = root.as_ref().to_path_buf();
        let root = run_source_worker(move || {
            let root =
                std::fs::canonicalize(root).map_err(|_| ProjectError::ProjectRootUnavailable)?;
            if !root.is_dir() {
                return Err(ProjectError::ProjectRootUnavailable);
            }
            Ok(root)
        })
        .await?;
        // The CLI owns at most one complete build. Its measured canonical input
        // plus 32 MiB workspace and bounded native buffers fit the kernel's
        // existing 512 MiB hard reservation ceiling.
        let storage = StorageRuntime::new(WorkerPolicy {
            pending_bytes: 512 * 1_048_576,
            ..WorkerPolicy::default()
        })?;
        let store = AsyncProjectStore::open(
            &storage,
            root.clone(),
            project_identity_digest(&root),
            StorePolicy::default(),
            request,
        )
        .await?;
        let database =
            cartograph_db::NativeDatabase::new(store.clone(), &project_identity_digest(&root));
        Ok(Arc::new(Self {
            root,
            storage,
            store,
            database,
            scans: Arc::new(Semaphore::new(1)),
            #[cfg(test)]
            source_pause: std::sync::Mutex::new(None),
            indexing: Mutex::new(None),
            operations: std::sync::Mutex::new(operations::OperationRegistry::default()),
        }))
    }

    /// Drain owned storage workers, preserving actual native commit results.
    ///
    /// # Errors
    /// Returns a bounded shutdown failure; the owner must be retained and retried.
    pub async fn shutdown(&self) -> Result<(), RocksProjectError> {
        let drained = self
            .drain_operations(&StorageRequest::new(Duration::from_secs(60))?)
            .await?;
        let storage = self
            .storage
            .shutdown(&StorageRequest::new(Duration::from_secs(60))?)
            .await;
        match drained {
            operations::OperationDrain::Clean => storage.map_err(Into::into),
            // A completed task failure must not prevent closing storage. An
            // incomplete drain above still returns before closing the pool.
            operations::OperationDrain::Failed => Err(operations::operation_error()),
        }
    }

    async fn scan(
        &self,
        request: &StorageRequest,
        retain_overlay: bool,
    ) -> Result<(SourceRevision, ProjectSourcePolicy), RocksProjectError> {
        let current = self.current(request).await?;
        let (excludes, ceiling) = current.map_or((Vec::new(), None), |(revision, _)| {
            (revision.run_excludes, revision.maximum_file_bytes)
        });
        self.scan_with_admission(request, retain_overlay, excludes, ceiling)
            .await
    }

    async fn scan_with_admission(
        &self,
        request: &StorageRequest,
        retain_overlay: bool,
        excludes: Vec<String>,
        ceiling: Option<usize>,
    ) -> Result<(SourceRevision, ProjectSourcePolicy), RocksProjectError> {
        request.check()?;
        let permit = tokio::select! {
            permit = self.scans.clone().acquire_owned() => permit.map_err(|_| RocksError::Closed)?,
            error = request.stopped() => return Err(error.into()),
        };
        let root = self.root.clone();
        let control = request.clone();
        #[cfg(test)]
        let pause = self
            .source_pause
            .lock()
            .map_err(|_| ProjectError::SourceScanFailed)?
            .clone();
        Ok(run_source_worker(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some(pause) = pause {
                pause.wait()?;
            }
            let mut policy = project_source_policy_with_excludes(&root, &excludes)?;
            if let Some(ceiling) = ceiling {
                if !(1..=32 * 1_048_576).contains(&ceiling) {
                    return Err(ProjectError::InvalidOptions);
                }
                policy.maximum_file_bytes = Some(
                    policy
                        .maximum_file_bytes
                        .map_or(ceiling, |configured| configured.min(ceiling)),
                );
            }
            let source = crate::source_scan::retry_complete_scan(|| {
                if control.check().is_err() {
                    return Err(ProjectError::RequestCancelled);
                }
                source_revision_with_options(
                    SourceRevisionRequest {
                        root: &root,
                        capture_paths: &BTreeSet::new(),
                        retain_scip_overlay: retain_overlay,
                        max_source_bytes: policy
                            .maximum_file_bytes
                            .unwrap_or(DEFAULT_MAX_SOURCE_BYTES),
                        discovery_policy: policy.discovery.clone(),
                        index_policy: policy.index,
                    },
                    || control.check().is_err(),
                )
            })?;
            Ok((source, policy))
        })
        .await?)
    }

    async fn current(
        &self,
        request: &StorageRequest,
    ) -> Result<Option<(StoredRevision, GraphInfo)>, RocksProjectError> {
        let result = self
            .store
            .read(
                None,
                ReadBudget::new(8, 1_048_576, Duration::from_secs(30))?,
                request,
                |snapshot, budget| {
                    Ok((
                        snapshot.revision().clone(),
                        snapshot.graph(budget)?.info().clone(),
                    ))
                },
            )
            .await;
        match result {
            Ok(value) => Ok(Some(value)),
            Err(RocksError::GenerationUnavailable) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Compare a complete live source manifest with the current native graph.
    ///
    /// # Errors
    /// Returns source, ownership, policy, or native read errors.
    async fn status_inner(
        &self,
        request: &StorageRequest,
    ) -> Result<RocksStatus, RocksProjectError> {
        let (source, _) = self.scan(request, false).await?;
        let current = self.current(request).await?;
        let fresh = current.as_ref().is_some_and(|(revision, _)| {
            revision.source_revision == source.digest
                && revision.digest_version == Some(GenerationDigestVersion::CURRENT)
        });
        Ok(RocksStatus {
            storage: "rocksdb",
            engine_version: cartograph_db::rocks::ENGINE_VERSION,
            current: current.as_ref().map(|(revision, _)| revision.clone()),
            counts: current.map(|(_, counts)| counts),
            live_source_revision: source.digest,
            fresh,
        })
    }

    /// Build native facts, fence their exact source, publish, and recheck freshness.
    ///
    /// # Errors
    /// Leaves the prior complete graph visible on build, source, or staging failure.
    async fn index_inner(
        &self,
        options: RocksIndexOptions,
        request: &StorageRequest,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        let mut warm = self
            .indexing
            .try_lock()
            .map_err(|_| RocksError::WriterBusy)?;
        let started = std::time::Instant::now();
        let current = self.current(request).await?;
        let inherited = current.as_ref().map(|(revision, _)| revision);
        let (excludes, ceiling) = options.source_admission(inherited)?;
        let (source, policy) = self
            .scan_with_admission(request, true, excludes.clone(), ceiling)
            .await?;
        let cache_contract =
            cartograph_indexer::native_parse_cache_contract_digest(policy.index.maximum_ast_depth);
        let preparation_millis = monotonic_millis(started.elapsed());
        let progress = options.progress.clone().unwrap_or_default();
        if !options.force
            && let Some(current) = current.as_ref().filter(|(revision, _)| {
                revision.source_revision == source.digest
                    && revision.digest_version == Some(GenerationDigestVersion::CURRENT)
                    && revision.run_excludes == excludes
                    && revision.maximum_file_bytes == ceiling
            })
        {
            return self
                .unchanged_report(
                    current,
                    cache_contract,
                    preparation_millis,
                    started,
                    request,
                )
                .await;
        }
        let base = current.as_ref().map_or(GraphBase::Empty, |(revision, _)| {
            GraphBase::Current(revision.generation_id.clone())
        });
        let pipeline_started = std::time::Instant::now();
        let index_policy = policy.index;
        let scip_overlay = source.scip_overlay.is_some();
        let observed_source = source.native_manifest.is_some();
        let PreparedGraph {
            input,
            native,
            workers,
            incremental_files,
        } = self
            .prepare_graph(
                &mut warm,
                GraphBuild {
                    source,
                    policy,
                    options: &options,
                    base,
                    request,
                    progress: &progress,
                    excludes: &excludes,
                    ceiling,
                    publication_policy: graph_publication_policy(),
                },
            )
            .await?;
        let counts = input.info()?;
        let retained_facts = input.shared_facts();
        if incremental_files > 0 {
            progress.publishing_incremental().await?;
        } else {
            progress.publishing().await?;
        }
        let publication = self.store.publish_graph(input, request).await?;
        *warm = Some(incremental::WarmGraph {
            generation: publication.revision.generation_id.clone(),
            policy: index_policy,
            scip_overlay,
            facts: retained_facts,
        });
        progress.finish(true, false).await;
        let pipeline_millis = monotonic_millis(pipeline_started.elapsed());
        let pipeline_stages = progress.status().await.stage_timings().to_vec();
        self.complete_index(
            IndexCompletion {
                native,
                workers,
                incremental_files,
                cache_contract,
                publication,
                counts,
                started,
                profile: IndexProfile {
                    preparation_millis,
                    source_manifest_reused: observed_source && incremental_files == 0,
                    pipeline_millis,
                    pipeline_stages,
                    ..IndexProfile::default()
                },
            },
            request,
        )
        .await
    }

    async fn complete_index(
        &self,
        completed: IndexCompletion,
        request: &StorageRequest,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        let IndexCompletion {
            native,
            workers,
            incremental_files,
            cache_contract,
            publication,
            counts,
            started,
            mut profile,
        } = completed;
        let (retention_progress, retention_error) = retention_outcome(&publication.retention);
        let (
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
        ) = self.maintain_auxiliary(&cache_contract, request).await;
        let (fresh, source_verification_error) = self
            .verify_publication(&publication.revision.generation_id, request)
            .await;
        let parse_cache = native.as_ref().map(NativePipelineReport::parse_cache);
        profile.total_millis = monotonic_millis(started.elapsed());
        Ok(RocksIndexReport {
            native: native.map(Into::into),
            workers,
            parse_cache_contract_digest: cache_contract,
            profile,
            parse_cache,
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
            unchanged: false,
            incremental_files,
            revision: publication.revision,
            counts,
            writes: publication.report,
            fresh,
            retention_completed: Some(retention_progress.is_some_and(|report| report.complete)),
            retention_progress,
            retention_error,
            source_verification_error,
        })
    }

    async fn verify_publication(
        &self,
        generation: &GenerationId,
        request: &StorageRequest,
    ) -> (bool, Option<String>) {
        match self.status_inner(request).await {
            Ok(status) => (
                status.fresh
                    && status
                        .current
                        .as_ref()
                        .is_some_and(|revision| &revision.generation_id == generation),
                None,
            ),
            Err(error) => (false, Some(error.to_string())),
        }
    }

    async fn unchanged_report(
        &self,
        current: &(StoredRevision, GraphInfo),
        cache_contract: ContentDigest,
        preparation_millis: u64,
        started: std::time::Instant,
        request: &StorageRequest,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        let (revision, counts) = current;
        let (retention_progress, retention_error) = self.maintain(request).await;
        let (
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
        ) = self.maintain_auxiliary(&cache_contract, request).await;
        let fresh = self.status_inner(request).await?.fresh;
        Ok(RocksIndexReport {
            native: None,
            workers: 0,
            parse_cache_contract_digest: cache_contract,
            profile: IndexProfile {
                preparation_millis,
                total_millis: monotonic_millis(started.elapsed()),
                ..IndexProfile::default()
            },
            parse_cache: None,
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
            unchanged: true,
            incremental_files: 0,
            revision: revision.clone(),
            counts: counts.clone(),
            writes: GraphWriteReport::default(),
            fresh,
            retention_completed: Some(retention_progress.is_some_and(|report| report.complete)),
            retention_progress,
            retention_error,
            source_verification_error: None,
        })
    }

    async fn maintain(&self, request: &StorageRequest) -> (Option<PruneReport>, Option<String>) {
        let result = async {
            self.store
                .prune(
                    2,
                    4,
                    10_000,
                    ReadBudget::new(20_000, 64 * 1_048_576, Duration::from_secs(30))?,
                    request,
                )
                .await
        }
        .await;
        retention_outcome(&match result {
            Ok(report) => GraphRetention::Completed(report),
            Err(error) => GraphRetention::Deferred(error),
        })
    }

    async fn maintain_auxiliary(
        &self,
        contract: &ContentDigest,
        request: &StorageRequest,
    ) -> (
        Option<cartograph_db::NativeParseCacheRetentionReport>,
        Option<String>,
        Option<cartograph_db::NativeAuxiliaryPruneReport>,
        Option<String>,
    ) {
        let cache = self
            .database
            .prune_native_parse_cache_with_request(
                self.database.project_id(),
                contract,
                cartograph_db::NativeParseCacheRetentionPolicy::automatic(),
                request,
            )
            .await;
        let auxiliary = self
            .database
            .prune_auxiliary_snapshots_with_request(request)
            .await;
        let (cache, cache_error) = match cache {
            Ok(report) => (Some(report), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let (auxiliary, auxiliary_error) = match auxiliary {
            Ok(report) => (Some(report), None),
            Err(error) => (None, Some(error.to_string())),
        };
        (cache, cache_error, auxiliary, auxiliary_error)
    }

    async fn prepare_graph(
        &self,
        warm: &mut Option<incremental::WarmGraph>,
        build: GraphBuild<'_>,
    ) -> Result<PreparedGraph, RocksProjectError> {
        if let Some((input, files)) = self.try_local_graph(warm, &build).await? {
            self.verify_graph_source(
                &input,
                &build.source,
                build.excludes,
                build.ceiling,
                build.request,
            )
            .await?;
            return Ok(PreparedGraph {
                input,
                native: None,
                workers: 1,
                incremental_files: files,
            });
        }
        // Release the old canonical payload before full extraction so the
        // existing whole-generation working-set ceiling still applies.
        *warm = None;
        let (input, native, workers) = self.build_graph(build).await?;
        Ok(PreparedGraph {
            input,
            native: Some(native),
            workers,
            incremental_files: 0,
        })
    }

    async fn build_graph(
        &self,
        input: GraphBuild<'_>,
    ) -> Result<(GraphPublicationInput, NativePipelineReport, u16), RocksProjectError> {
        let GraphBuild {
            mut source,
            policy,
            options,
            base,
            request,
            progress,
            excludes,
            ceiling,
            publication_policy,
        } = input;
        let workers = select_worker_count(
            source.files,
            source.source_bytes,
            options.workers.unwrap_or(16),
        );
        let pipeline = pipeline_config(
            PipelineBounds {
                workers,
                max_source_bytes: policy
                    .maximum_file_bytes
                    .unwrap_or(DEFAULT_MAX_SOURCE_BYTES),
                max_generation_bytes: policy
                    .maximum_generation_bytes
                    .unwrap_or(DEFAULT_MAX_GENERATION_BYTES),
            },
            policy.index,
        )?;
        let root = self.root.clone();
        let source_root = run_source_worker(move || {
            SourceRoot::open_with_policy(&root, policy.discovery)
                .map_err(|_| ProjectError::ProjectRootUnavailable)
        })
        .await?;
        let cache = NativeParseCache::from_native(self.database.clone()).with_reads(!options.force);
        let mut build = NativeGenerationBuild::new(source_root, pipeline).with_parse_cache(cache);
        if let Some(observed) = source.native_manifest.take() {
            build = build.with_observed_source(observed);
        }
        if let Some(overlay) = source.scip_overlay.take() {
            build = build.with_scip_overlay(overlay);
        }
        let generation_id = new_generation_id();
        let identity = GraphRevision {
            generation_id,
            source_revision: source.digest.clone(),
            base,
        };
        let (input, report) = build_native_rocks_graph_observed(
            build,
            identity,
            publication_policy,
            request,
            progress,
        )
        .await?;
        let input = input.with_source_admission(excludes.to_vec(), ceiling)?;
        self.verify_graph_source(&input, &source, excludes, ceiling, request)
            .await?;
        Ok((input, report, workers))
    }

    async fn verify_graph_source(
        &self,
        input: &GraphPublicationInput,
        source: &SourceRevision,
        excludes: &[String],
        ceiling: Option<usize>,
        request: &StorageRequest,
    ) -> Result<(), RocksProjectError> {
        // Check the bytes actually parsed, not only pre/post filesystem scans.
        // Sorting the bounded borrowed file set also prevents an ABA edit from
        // publishing facts under an unrelated source revision.
        let manifest = tokio::task::block_in_place(|| {
            let mut entries = input
                .files()
                .iter()
                .map(|file| {
                    Ok((
                        NormalizedPath::parse(&file.normalized_path)
                            .map_err(|_| ProjectError::SourceScanFailed)?,
                        file.content_hash.clone(),
                    ))
                })
                .collect::<Result<Vec<_>, ProjectError>>()?;
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            finish_source_manifest(&entries)
        })?;
        if manifest != source.admitted_source_digest {
            return Err(RocksProjectError::SourceChanged);
        }
        if self
            .scan_with_admission(request, false, excludes.to_vec(), ceiling)
            .await?
            .0
            .digest
            != source.digest
        {
            return Err(RocksProjectError::SourceChanged);
        }
        Ok(())
    }
}

fn retention_outcome(retention: &GraphRetention) -> (Option<PruneReport>, Option<String>) {
    match retention {
        GraphRetention::Completed(report) => (Some(*report), None),
        GraphRetention::Deferred(error) => (None, Some(error.to_string())),
    }
}

fn new_generation_id() -> GenerationId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hash = blake3::Hasher::new();
    hash.update(b"cartograph:rocks-cli-generation:v1");
    hash.update(&std::process::id().to_be_bytes());
    hash.update(
        &NEXT
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_be_bytes(),
    );
    hash.update(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
    GenerationId::from_uuid_v8(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cartograph_db::rocks::{Collection, RecordChange, RecordKey};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unchanged_indexes_resume_partial_retention_without_publishing()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("lib.rs"), "pub fn original() {}\n")?;
        let control = StorageRequest::new(Duration::from_secs(30))?;
        let runtime = RocksProjectRuntime::open(root.path(), &control).await?;
        let indexed = runtime
            .index(RocksIndexOptions::default(), &control)
            .await?;
        assert!(indexed.fresh);
        // Simulate an interrupted staging attempt larger than the automatic
        // 10,000-version batch, while preserving the complete current graph.
        let writer = runtime
            .store
            .begin_revision(
                new_generation_id(),
                indexed.revision.source_revision.clone(),
                Duration::from_secs(30),
                &control,
            )
            .await?;
        for batch in 0_u32..12 {
            let changes = (batch * 1_000..(batch + 1) * 1_000)
                .map(|index| {
                    Ok(RecordChange::put(
                        Collection::Graph,
                        RecordKey::new(index.to_be_bytes().to_vec())?,
                        vec![1],
                    ))
                })
                .collect::<Result<Vec<_>, RocksError>>()?;
            runtime.store.stage(&writer, changes, &control).await?;
        }
        runtime.store.fail(&writer, &control).await?;
        let first = runtime
            .index(RocksIndexOptions::default(), &control)
            .await?;
        assert!(first.unchanged && first.fresh);
        assert_eq!(first.revision, indexed.revision);
        assert_eq!(first.retention_completed, Some(false));
        let progress = first
            .retention_progress
            .ok_or("missing retention progress")?;
        assert_eq!(progress.versions_removed, 10_000);
        assert!(!progress.complete);
        let second = runtime
            .index(RocksIndexOptions::default(), &control)
            .await?;
        assert!(second.unchanged && second.fresh);
        assert_eq!(second.revision, indexed.revision);
        assert_eq!(second.retention_completed, Some(true));
        assert_eq!(
            second
                .retention_progress
                .ok_or("missing retention progress")?
                .versions_removed,
            2_000
        );
        runtime.shutdown().await?;
        Ok(())
    }
}
