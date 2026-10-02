use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use cartograph_db::{InterchangeSnapshot, InterchangeSnapshotError, InterchangeSnapshotRequest};
use cartograph_domain::{ContentDigest, GenerationId, NormalizedPath};
use cartograph_extract::{SourceReadOptions, SourceRoot};
use cartograph_scip::{
    ScipExportOptions, ScipExportOptionsInput, ScipExportStats, ScipIndex, decode_scip_index,
    export_snapshot,
};
use serde::Serialize;

use crate::{
    IndexFailure, IndexOptions, IndexReport, MAXIMUM_SCIP_OVERLAY_BYTES, MAXIMUM_SCIP_OVERLAY_ROWS,
    ProjectCancellation, ProjectError, ProjectRuntime, SCIP_OVERLAY_RELATIVE_PATH, source_limits,
};

const DEFAULT_INTERCHANGE_TIMEOUT: Duration = Duration::from_mins(2);
const MAXIMUM_INTERCHANGE_TIMEOUT: Duration = Duration::from_mins(30);
const MAXIMUM_INTERCHANGE_ROWS: u64 = 5_000_000;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const DEFAULT_PACKAGE_VERSION: &str = "workspace";

/// Validated request for an immutable current-generation SCIP export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScipExportRequest {
    output: NormalizedPath,
    maximum_rows: u64,
    timeout: Duration,
}

impl ScipExportRequest {
    /// Restrict output to one project-relative regular artifact path.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `output` is not a safe
    /// project-relative artifact path or `maximum_rows` is zero or excessive.
    pub fn new(output: &str, maximum_rows: u64) -> Result<Self, ProjectError> {
        let output = artifact_path(output)?;
        if maximum_rows == 0 || maximum_rows > MAXIMUM_INTERCHANGE_ROWS {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            output,
            maximum_rows,
            timeout: DEFAULT_INTERCHANGE_TIMEOUT,
        })
    }

    /// Override the complete repeatable-read and source-export timeout.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `timeout` is zero or
    /// exceeds the maximum interchange operation duration.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, ProjectError> {
        if timeout.is_zero() || timeout > MAXIMUM_INTERCHANGE_TIMEOUT {
            return Err(ProjectError::InvalidOptions);
        }
        self.timeout = timeout;
        Ok(self)
    }
}

/// Validated byte, row, and worker bounds for a persistent SCIP overlay import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScipImportLimits {
    maximum_bytes: usize,
    maximum_rows: usize,
    workers: u16,
}

impl ScipImportLimits {
    /// Validate the complete bounded import envelope before any artifact is opened.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when the byte or row ceiling is
    /// zero or excessive, or `workers` is outside the supported indexing range.
    pub fn new(
        maximum_bytes: usize,
        maximum_rows: usize,
        workers: u16,
    ) -> Result<Self, ProjectError> {
        if maximum_bytes == 0
            || maximum_bytes > MAXIMUM_SCIP_OVERLAY_BYTES
            || maximum_rows == 0
            || maximum_rows > MAXIMUM_SCIP_OVERLAY_ROWS
        {
            return Err(ProjectError::InvalidOptions);
        }
        IndexOptions::default().with_max_workers(workers)?;
        Ok(Self {
            maximum_bytes,
            maximum_rows,
            workers,
        })
    }
}

/// Validated request to install and publish a persistent SCIP overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScipImportRequest {
    input: NormalizedPath,
    maximum_bytes: usize,
    maximum_rows: usize,
    workers: u16,
}

impl ScipImportRequest {
    /// Bound the project-relative input, protobuf bytes/rows, and index workers.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `input` is not a safe
    /// project-relative regular-artifact path.
    pub fn new(input: &str, limits: ScipImportLimits) -> Result<Self, ProjectError> {
        let input = artifact_path(input)?;
        Ok(Self {
            input,
            maximum_bytes: limits.maximum_bytes,
            maximum_rows: limits.maximum_rows,
            workers: limits.workers,
        })
    }
}

/// Published result of an exact current-generation SCIP export.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScipExportReport {
    /// Generation fenced by the repeatable-read export.
    pub generation_id: GenerationId,
    /// Project-relative output path.
    pub output: String,
    /// Deterministic protobuf and graph accounting.
    pub stats: ScipExportStats,
}

/// Published result of installing and indexing a persistent SCIP overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScipImportReport {
    /// Project-relative source artifact path.
    pub input: String,
    /// Fixed persistent overlay location.
    pub overlay: &'static str,
    /// Foreign producer name when present.
    pub source_tool: String,
    /// Foreign producer version when present.
    pub source_version: String,
    /// Decoded document count.
    pub documents: u64,
    /// Decoded symbol count.
    pub symbols: u64,
    /// Decoded occurrence count.
    pub occurrences: u64,
    /// Exact typed Cartograph extension edge count.
    pub exact_typed_edges: u64,
    /// Encoded bytes installed.
    pub bytes: u64,
    /// Forced publication using the newly installed overlay.
    pub index: IndexReport,
}

struct DecodedImport {
    bytes: Vec<u8>,
    source_tool: String,
    source_version: String,
    documents: u64,
    symbols: u64,
    occurrences: u64,
    exact_typed_edges: u64,
}

struct ScipExportWork<'input> {
    root: &'input Path,
    repository_fingerprint: &'input ContentDigest,
    snapshot: InterchangeSnapshot,
    output: &'input NormalizedPath,
    cancellation: ProjectCancellation,
}

struct ScipImportRead<'input> {
    root: &'input Path,
    input: &'input NormalizedPath,
    maximum_bytes: usize,
    maximum_rows: usize,
    cancellation: ProjectCancellation,
}

#[derive(Clone, Copy)]
struct RelativeReadRequest<'input> {
    root: &'input Path,
    relative: &'input NormalizedPath,
    maximum_bytes: usize,
}

impl ProjectRuntime {
    /// Export the exact fresh PostgreSQL graph to a project-local SCIP artifact.
    /// # Errors
    ///
    /// Returns an error when the current generation is absent or stale, the
    /// repeatable-read graph exceeds its bounds or is unavailable, the output
    /// artifact cannot be written atomically, or cancellation wins.
    pub async fn export_scip_with_cancellation(
        &self,
        request: ScipExportRequest,
        cancellation: ProjectCancellation,
    ) -> Result<ScipExportReport, ProjectError> {
        let status = self.status_with_cancellation(cancellation.clone()).await?;
        if !status.fresh || cancellation.is_cancelled() {
            return if cancellation.is_cancelled() {
                Err(ProjectError::RequestCancelled)
            } else {
                Err(ProjectError::IndexFailed)
            };
        }
        let project_id = status
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.project_id.clone())
            .ok_or(ProjectError::StatusFailed)?;
        let snapshot = self
            .database
            .current_interchange_snapshot(InterchangeSnapshotRequest {
                project_id: &project_id,
                maximum_rows: request.maximum_rows,
                statement_timeout: request.timeout,
            })
            .await
            .map_err(interchange_error)?;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let root = self.root.clone();
        let repository_fingerprint = self.repository_fingerprint.clone();
        let output = request.output.clone();
        let worker_cancellation = cancellation.clone();
        let report = tokio::task::spawn_blocking(move || {
            export_scip_blocking(ScipExportWork {
                root: &root,
                repository_fingerprint: &repository_fingerprint,
                snapshot,
                output: &output,
                cancellation: worker_cancellation,
            })
        })
        .await
        .map_err(|_| ProjectError::IndexFailed)??;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        Ok(report)
    }

    /// Atomically install a validated SCIP artifact, force an index, and roll back on failure.
    /// # Errors
    ///
    /// Returns an error when the input is missing, unsafe, oversized, or invalid
    /// SCIP; overlay installation fails; forced indexing fails; the installed
    /// digest changes; or cancellation wins. A failed or cancelled forced index
    /// keeps its [`IndexFailure`], including whether bounded cleanup of its own
    /// staging generation also failed. Restoring the previous overlay then
    /// never replaces that failure: a restore that fails as well is reported
    /// beside it through [`IndexFailure::overlay_rollback_failed`].
    pub async fn import_scip_with_cancellation(
        &self,
        request: ScipImportRequest,
        cancellation: ProjectCancellation,
    ) -> Result<ScipImportReport, IndexFailure> {
        let root = self.root.clone();
        let input = request.input.clone();
        let read_cancellation = cancellation.clone();
        let maximum_bytes = request.maximum_bytes;
        let maximum_rows = request.maximum_rows;
        let decoded = tokio::task::spawn_blocking(move || {
            read_and_decode_import(ScipImportRead {
                root: &root,
                input: &input,
                maximum_bytes,
                maximum_rows,
                cancellation: read_cancellation,
            })
        })
        .await
        .map_err(|_| ProjectError::ScipOverlayInvalid)??;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled.into());
        }
        let DecodedImport {
            bytes,
            source_tool,
            source_version,
            documents,
            symbols,
            occurrences,
            exact_typed_edges,
        } = decoded;
        let byte_count = usize_to_u64(bytes.len());
        let requested_digest = blake3::hash(&bytes);
        // Validate the index options before the install, so nothing between
        // the install and the forced index can return without a rollback.
        let options = IndexOptions::default()
            .with_force(true)
            .with_max_workers(request.workers)?;
        let root = self.root.clone();
        let backup = tokio::task::spawn_blocking(move || install_overlay(&root, &bytes))
            .await
            .map_err(|_| ProjectError::ScipOverlayInvalid)??;

        let index =
            Box::pin(self.index_with_cancellation_detail(options, cancellation.clone())).await;
        let index = match index {
            Ok(index) => index,
            Err(failure) => {
                let rollback =
                    rollback_overlay_if_owned(self.root.clone(), requested_digest, backup).await;
                return Err(if rollback.is_ok() {
                    failure
                } else {
                    failure.with_failed_overlay_rollback()
                });
            }
        };
        let root = self.root.clone();
        let final_digest = tokio::task::spawn_blocking(move || read_overlay_digest(&root))
            .await
            .map_err(|_| ProjectError::ScipOverlayInvalid)??;
        if final_digest != Some(requested_digest) {
            return Err(ProjectError::IndexFailed.into());
        }
        Ok(ScipImportReport {
            input: request.input.into_string(),
            overlay: SCIP_OVERLAY_RELATIVE_PATH,
            source_tool,
            source_version,
            documents,
            symbols,
            occurrences,
            exact_typed_edges,
            bytes: byte_count,
            index,
        })
    }
}

fn export_scip_blocking(input: ScipExportWork<'_>) -> Result<ScipExportReport, ProjectError> {
    let ScipExportWork {
        root,
        repository_fingerprint,
        snapshot,
        output,
        cancellation,
    } = input;
    let source_root = SourceRoot::open(root).map_err(|_| ProjectError::ProjectRootUnavailable)?;
    let limits = source_limits()?;
    let project_name = package_name(root);
    let project_root_uri = format!("cartograph://{}", repository_fingerprint.as_str());
    let options = ScipExportOptions::new(ScipExportOptionsInput {
        project_name: &project_name,
        project_version: DEFAULT_PACKAGE_VERSION,
        tool_version: env!("CARGO_PKG_VERSION"),
        project_root_uri: &project_root_uri,
    })
    .map_err(|_| ProjectError::IndexFailed)?;
    let read_cancellation = cancellation.clone();
    let export = export_snapshot(&snapshot, &options, |raw_path| {
        if read_cancellation.is_cancelled() {
            return None;
        }
        let path = NormalizedPath::parse(raw_path).ok()?;
        let snapshot = source_root
            .read_with_cancellation(
                &path,
                SourceReadOptions::new(limits, || read_cancellation.is_cancelled()),
            )
            .ok()?;
        Some(String::from(snapshot.into_source()).into_bytes())
    })
    .map_err(|_| {
        if cancellation.is_cancelled() {
            ProjectError::RequestCancelled
        } else {
            ProjectError::IndexFailed
        }
    })?;
    if cancellation.is_cancelled() {
        return Err(ProjectError::RequestCancelled);
    }
    atomic_write_relative(root, output, export.bytes())?;
    Ok(ScipExportReport {
        generation_id: snapshot.generation_id,
        output: output.as_str().to_owned(),
        stats: export.stats(),
    })
}

fn read_and_decode_import(input: ScipImportRead<'_>) -> Result<DecodedImport, ProjectError> {
    let ScipImportRead {
        root,
        input,
        maximum_bytes,
        maximum_rows,
        cancellation,
    } = input;
    let bytes = read_bounded_relative(
        RelativeReadRequest {
            root,
            relative: input,
            maximum_bytes,
        },
        || cancellation.is_cancelled(),
    )?;
    if cancellation.is_cancelled() {
        return Err(ProjectError::RequestCancelled);
    }
    let index = decode_scip_index(&bytes).map_err(|_| ProjectError::ScipOverlayInvalid)?;
    let (symbols, occurrences, exact_typed_edges, rows) = decoded_counts(&index)?;
    if index.documents.is_empty() || rows > usize_to_u64(maximum_rows) {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    Ok(DecodedImport {
        bytes,
        source_tool: index.tool_name,
        source_version: index.tool_version,
        documents: usize_to_u64(index.documents.len()),
        symbols,
        occurrences,
        exact_typed_edges,
    })
}

fn decoded_counts(index: &ScipIndex) -> Result<(u64, u64, u64, u64), ProjectError> {
    index.documents.iter().try_fold(
        (0_u64, 0_u64, 0_u64, usize_to_u64(index.documents.len())),
        |counts, document| {
            let symbols = usize_to_u64(document.symbols.len());
            let occurrences = usize_to_u64(document.occurrences.len());
            let exact = document.symbols.iter().try_fold(0_u64, |total, symbol| {
                total
                    .checked_add(usize_to_u64(symbol.cartograph_edges.len()))
                    .ok_or(ProjectError::ScipOverlayInvalid)
            })?;
            let relationships = document.symbols.iter().try_fold(0_u64, |total, symbol| {
                total
                    .checked_add(usize_to_u64(symbol.relationships.len()))
                    .ok_or(ProjectError::ScipOverlayInvalid)
            })?;
            Ok((
                counts
                    .0
                    .checked_add(symbols)
                    .ok_or(ProjectError::ScipOverlayInvalid)?,
                counts
                    .1
                    .checked_add(occurrences)
                    .ok_or(ProjectError::ScipOverlayInvalid)?,
                counts
                    .2
                    .checked_add(exact)
                    .ok_or(ProjectError::ScipOverlayInvalid)?,
                counts
                    .3
                    .checked_add(symbols)
                    .and_then(|rows| rows.checked_add(occurrences))
                    .and_then(|rows| rows.checked_add(exact))
                    .and_then(|rows| rows.checked_add(relationships))
                    .ok_or(ProjectError::ScipOverlayInvalid)?,
            ))
        },
    )
}

fn install_overlay(root: &Path, bytes: &[u8]) -> Result<Option<Vec<u8>>, ProjectError> {
    let directory = ensure_overlay_directory(root)?;
    let target = directory.join("overlay.scip");
    let backup = read_optional_bounded(&target, MAXIMUM_SCIP_OVERLAY_BYTES, || false)?;
    atomic_write_path(&target, bytes)?;
    Ok(backup)
}

/// Restore the overlay that `install_overlay` replaced, or remove a new one,
/// unless the installed path no longer holds the requested bytes (another
/// writer replaced them, so they are not this import's to restore).
///
/// An error means only that the restore did not complete, for example
/// because the overlay path is no longer a readable regular file or its
/// directory cannot be written; the requested artifact may still be installed.
async fn rollback_overlay_if_owned(
    root: PathBuf,
    requested_digest: blake3::Hash,
    backup: Option<Vec<u8>>,
) -> Result<(), ProjectError> {
    tokio::task::spawn_blocking(move || {
        let target = root.join(SCIP_OVERLAY_RELATIVE_PATH);
        if read_path_digest(&target)? != Some(requested_digest) {
            return Ok(());
        }
        match backup {
            Some(bytes) => atomic_write_path(&target, &bytes),
            None => match std::fs::remove_file(target) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(ProjectError::ScipOverlayInvalid),
            },
        }
    })
    .await
    .map_err(|_| ProjectError::ScipOverlayInvalid)?
}

fn read_overlay_digest(root: &Path) -> Result<Option<blake3::Hash>, ProjectError> {
    read_path_digest(&root.join(SCIP_OVERLAY_RELATIVE_PATH))
}

fn read_path_digest(path: &Path) -> Result<Option<blake3::Hash>, ProjectError> {
    read_optional_bounded(path, MAXIMUM_SCIP_OVERLAY_BYTES, || false)
        .map(|bytes| bytes.map(|bytes| blake3::hash(&bytes)))
}

fn read_bounded_relative(
    request: RelativeReadRequest<'_>,
    cancelled: impl FnMut() -> bool,
) -> Result<Vec<u8>, ProjectError> {
    let RelativeReadRequest {
        root,
        relative,
        maximum_bytes,
    } = request;
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| ProjectError::ProjectRootUnavailable)?;
    let path = canonical_root.join(relative.as_str());
    let parent = path.parent().ok_or(ProjectError::InvalidOptions)?;
    let canonical_parent =
        std::fs::canonicalize(parent).map_err(|_| ProjectError::ScipOverlayInvalid)?;
    if !canonical_parent.starts_with(&canonical_root) || !canonical_parent.is_dir() {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    read_optional_bounded(&path, maximum_bytes, cancelled)?.ok_or(ProjectError::ScipOverlayInvalid)
}

fn read_optional_bounded(
    path: &Path,
    maximum_bytes: usize,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<Vec<u8>>, ProjectError> {
    let initial = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProjectError::ScipOverlayInvalid),
    };
    if initial.file_type().is_symlink() || !initial.is_file() {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    let metadata = file
        .metadata()
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    let expected = usize::try_from(metadata.len())
        .ok()
        .filter(|length| (1..=maximum_bytes).contains(length))
        .ok_or(ProjectError::ScipOverlayInvalid)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(expected)
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    let mut buffer = vec![0_u8; READ_BUFFER_BYTES];
    while bytes.len() < expected {
        if cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ProjectError::ScipOverlayInvalid),
        };
        if bytes.len().saturating_add(count) > maximum_bytes {
            return Err(ProjectError::ScipOverlayInvalid);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != expected {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    Ok(Some(bytes))
}

fn atomic_write_relative(
    root: &Path,
    relative: &NormalizedPath,
    bytes: &[u8],
) -> Result<(), ProjectError> {
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| ProjectError::ProjectRootUnavailable)?;
    let path = canonical_root.join(relative.as_str());
    let parent = path.parent().ok_or(ProjectError::InvalidOptions)?;
    let canonical_parent =
        std::fs::canonicalize(parent).map_err(|_| ProjectError::InvalidOptions)?;
    if !canonical_parent.starts_with(&canonical_root) || !canonical_parent.is_dir() {
        return Err(ProjectError::InvalidOptions);
    }
    atomic_write_path(&path, bytes)
}

fn atomic_write_path(path: &Path, bytes: &[u8]) -> Result<(), ProjectError> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    let parent = path.parent().ok_or(ProjectError::ScipOverlayInvalid)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ProjectError::ScipOverlayInvalid)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    }
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    temporary
        .persist(path)
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    #[cfg(unix)]
    sync_parent(parent)?;
    Ok(())
}

fn ensure_overlay_directory(root: &Path) -> Result<PathBuf, ProjectError> {
    let canonical_root =
        std::fs::canonicalize(root).map_err(|_| ProjectError::ProjectRootUnavailable)?;
    let mut current = canonical_root.clone();
    for component in [".cartograph", "scip"] {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|_| ProjectError::ScipOverlayInvalid)?;
            }
            Ok(_) | Err(_) => return Err(ProjectError::ScipOverlayInvalid),
        }
    }
    let canonical =
        std::fs::canonicalize(&current).map_err(|_| ProjectError::ScipOverlayInvalid)?;
    if !canonical.starts_with(&canonical_root) {
        return Err(ProjectError::ScipOverlayInvalid);
    }
    Ok(canonical)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), ProjectError> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ProjectError::ScipOverlayInvalid)?;
    Ok(())
}

fn package_name(root: &Path) -> String {
    root.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && !value.contains('\0'))
        .map_or_else(|| "project".to_owned(), std::borrow::ToOwned::to_owned)
}

fn artifact_path(raw: &str) -> Result<NormalizedPath, ProjectError> {
    let path = NormalizedPath::parse(raw).map_err(|_| ProjectError::InvalidOptions)?;
    if path.as_str() == SCIP_OVERLAY_RELATIVE_PATH
        || path.as_str() == ".git"
        || path.as_str().starts_with(".git/")
    {
        return Err(ProjectError::InvalidOptions);
    }
    Ok(path)
}

const fn interchange_error(error: InterchangeSnapshotError) -> ProjectError {
    match error {
        InterchangeSnapshotError::InvalidBounds | InterchangeSnapshotError::RowBoundExceeded => {
            ProjectError::InvalidOptions
        }
        InterchangeSnapshotError::CurrentGenerationUnavailable => ProjectError::StatusFailed,
        InterchangeSnapshotError::CorruptStoredValue
        | InterchangeSnapshotError::DatabaseOperation => ProjectError::IndexFailed,
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use cartograph_scip::{ScipDocument, ScipIndex, encode_scip_index};

    use super::*;

    #[test]
    fn import_reader_rejects_symlinks_and_enforces_decoded_rows() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let index = ScipIndex {
            tool_name: "foreign".to_owned(),
            tool_version: "1".to_owned(),
            project_root: "file:///project".to_owned(),
            documents: vec![ScipDocument {
                relative_path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                occurrences: Vec::new(),
                symbols: Vec::new(),
            }],
        };
        let bytes =
            encode_scip_index(&index).unwrap_or_else(|error| panic!("SCIP encode failed: {error}"));
        std::fs::write(directory.path().join("index.scip"), bytes)
            .unwrap_or_else(|error| panic!("SCIP write failed: {error}"));
        let input = NormalizedPath::parse("index.scip")
            .unwrap_or_else(|error| panic!("path failed: {error}"));
        assert!(
            read_and_decode_import(ScipImportRead {
                root: directory.path(),
                input: &input,
                maximum_bytes: 1024 * 1024,
                maximum_rows: 1,
                cancellation: ProjectCancellation::new(),
            })
            .is_ok()
        );
        assert_eq!(
            read_and_decode_import(ScipImportRead {
                root: directory.path(),
                input: &input,
                maximum_bytes: 1024 * 1024,
                maximum_rows: 0,
                cancellation: ProjectCancellation::new(),
            })
            .map(|decoded| decoded.documents),
            Err(ProjectError::ScipOverlayInvalid)
        );
    }

    #[test]
    fn atomic_overlay_install_returns_exact_backup() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let first = [1_u8, 2, 3];
        let second = [4_u8, 5, 6];
        assert_eq!(
            install_overlay(directory.path(), &first)
                .unwrap_or_else(|error| panic!("first install failed: {error}")),
            None
        );
        assert_eq!(
            install_overlay(directory.path(), &second)
                .unwrap_or_else(|error| panic!("second install failed: {error}")),
            Some(first.to_vec())
        );
        assert_eq!(
            std::fs::read(directory.path().join(SCIP_OVERLAY_RELATIVE_PATH))
                .unwrap_or_else(|error| panic!("overlay read failed: {error}")),
            second
        );
    }
}
