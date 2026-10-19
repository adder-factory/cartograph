//! Shared source access remains rooted in the native project owner.
use super::{
    BTreeSet, CartographDatabase, DEFAULT_MAX_SOURCE_BYTES, NormalizedPath, Ordering, Path,
    ProjectCancellation, ProjectError, ProjectRuntime, SourceRevision, SourceScanRequest,
    project_source_policy_with_excludes, scan_source_path, source_context,
};
impl ProjectRuntime {
    /// Public database service for graph/search adapters; SQL internals stay private.
    #[must_use]
    pub const fn database(&self) -> &CartographDatabase {
        &self.database
    }
    /// Privacy-preserving stable identity stored instead of the absolute checkout path.
    #[must_use]
    pub fn root_identity(&self) -> &str {
        &self.root_identity
    }
    /// Canonical checkout root for explicit host/install operations.
    /// Callers must not copy it into errors, logs, or unrelated tool results.
    #[must_use]
    pub fn project_root_for_host_operations(&self) -> &Path {
        &self.root
    }
    /// Normalize a project-relative path or an existing absolute path contained by this checkout.
    /// # Errors
    ///
    /// Returns an error when the input is empty or contains NUL, a relative
    /// path is invalid, or an absolute path is missing, outside the checkout,
    /// not a file, or not representable as a normalized project path.
    pub fn normalized_project_path(&self, input: &str) -> Result<NormalizedPath, ProjectError> {
        if input.is_empty() || input.contains('\0') {
            return Err(ProjectError::InvalidOptions);
        }
        let path = Path::new(input);
        if !path.is_absolute() {
            return NormalizedPath::parse(input).map_err(|_| ProjectError::InvalidOptions);
        }
        let canonical = std::fs::canonicalize(path).map_err(|_| ProjectError::FileNotFound)?;
        if !canonical.starts_with(&self.root) || !canonical.is_file() {
            return Err(ProjectError::InvalidOptions);
        }
        let relative = canonical
            .strip_prefix(&self.root)
            .map_err(|_| ProjectError::InvalidOptions)?;
        let relative = relative.to_str().ok_or(ProjectError::InvalidOptions)?;
        NormalizedPath::parse(relative).map_err(|_| ProjectError::InvalidOptions)
    }
    /// Number of admitted full source-manifest observations by this runtime.
    ///
    /// Includes failed scans; queued requests cancelled before admission do not
    /// increment it. The counter saturates and contains no source or query data.
    #[must_use]
    pub fn source_scan_observations(&self) -> u64 {
        self.source_scans.observations.load(Ordering::Relaxed)
    }
    /// Bounded units of source-scan work completed by this runtime: the
    /// cancellation checkpoints its scans pass while discovering and hashing
    /// files. It advances throughout a long scan, so a supervisor can tell a
    /// slow scan from a stalled one. The counter saturates and contains no
    /// source data.
    #[must_use]
    pub fn source_scan_checkpoints(&self) -> u64 {
        self.source_scans.checkpoints.load(Ordering::Relaxed)
    }
    pub(super) async fn scan_source_batch(
        &self,
        capture_paths: BTreeSet<NormalizedPath>,
        additional_excludes: &[String],
        maximum_file_bytes: Option<usize>,
        cancellation: ProjectCancellation,
    ) -> Result<SourceRevision, ProjectError> {
        if capture_paths.len() > source_context::MAXIMUM_CONTEXT_BATCH {
            return Err(ProjectError::InvalidOptions);
        }
        let source_policy = project_source_policy_with_excludes(&self.root, additional_excludes)?;
        let max_source_bytes = source_policy
            .maximum_file_bytes
            .unwrap_or(DEFAULT_MAX_SOURCE_BYTES)
            .min(maximum_file_bytes.unwrap_or(DEFAULT_MAX_SOURCE_BYTES));
        scan_source_path(SourceScanRequest {
            root: self.root.clone(),
            permits: self.source_scan_permits.clone(),
            counters: self.source_scans.clone(),
            capture_paths,
            retain_scip_overlay: false,
            max_source_bytes,
            discovery_policy: source_policy.discovery,
            index_policy: source_policy.index,
            cancellation,
        })
        .await
    }
    pub(super) async fn scan_source(
        &self,
        capture_path: Option<NormalizedPath>,
        cancellation: ProjectCancellation,
    ) -> Result<SourceRevision, ProjectError> {
        let snapshot = self
            .database
            .project_snapshot_by_root(&self.root_identity)
            .await
            .map_err(|_| ProjectError::StatusFailed)?;
        let current = snapshot
            .as_ref()
            .and_then(|project| project.current.as_ref());
        let run_excludes =
            current.map_or(&[][..], |current| current.source_admission.run_excludes());
        let ceiling = current.and_then(|current| current.source_admission.maximum_file_bytes());
        self.scan_source_batch(
            capture_path.into_iter().collect(),
            run_excludes,
            ceiling,
            cancellation,
        )
        .await
    }
}
