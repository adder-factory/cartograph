use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    mem::size_of,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use cartograph_domain::NormalizedPath;
use ignore::{
    DirEntry, WalkBuilder,
    gitignore::{Gitignore, GitignoreBuilder},
};
use thiserror::Error;

use crate::{DiscoveryPolicy, SourceRoot};

const CARTOGRAPH_DIRECTORY: &str = ".cartograph";
const CARTOGRAPH_IGNORE_MARKER: &str = ".cartographignore";
const GIT_DIRECTORY: &str = ".git";
const MAX_CONFIGURED_SOURCE_FILES: usize = 10_000_000;
const MAX_CONFIGURED_PATH_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_GIT_PATH_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;
const MIN_NESTED_DIRECTORY_SCAN_LIMIT: usize = 4_096;
const NESTED_DIRECTORY_MULTIPLIER: usize = 64;
const MAX_NESTED_DIRECTORY_SCAN_LIMIT: usize = 10_000_000;
const MAX_GITMODULES_BYTES: u64 = 1024 * 1024;
const MAX_IGNORE_MARKER_BYTES: u64 = 1024 * 1024;
const TRACKED_PATH_OUTPUT_MULTIPLIER: u64 = 8;
const MINIMUM_GIT_PATH_OUTPUT_BYTES: u64 = 1024 * 1024;

/// One deterministic project-relative native source candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredSource {
    path: NormalizedPath,
    byte_size: u64,
}

impl DiscoveredSource {
    /// Bind a validated path to the size observed during project discovery.
    #[must_use]
    pub const fn new(path: NormalizedPath, byte_size: u64) -> Self {
        Self { path, byte_size }
    }

    /// Canonical project-relative path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// File size observed without opening or retaining source bytes.
    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    /// Conservative retained bytes for this path/size manifest record.
    #[must_use]
    pub fn modeled_retained_bytes(&self) -> u64 {
        u64::try_from(size_of::<Self>())
            .unwrap_or(u64::MAX)
            .saturating_mul(2)
            .saturating_add(u64::try_from(self.path.as_str().len()).unwrap_or(u64::MAX))
    }
}

/// Hard discovery bounds independent of source-file contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryLimits {
    max_files: usize,
    max_retained_path_bytes: u64,
}

impl DiscoveryLimits {
    /// Validate maximum file count and retained manifest bytes.
    /// # Errors
    ///
    /// Returns an error if either file-count or retained-path-byte bound is
    /// zero or exceeds its configured discovery maximum.
    pub const fn new(
        max_files: usize,
        max_retained_path_bytes: u64,
    ) -> Result<Self, DiscoveryLimitsError> {
        if max_files == 0 || max_files > MAX_CONFIGURED_SOURCE_FILES {
            return Err(DiscoveryLimitsError::InvalidFileLimit);
        }
        if max_retained_path_bytes == 0 || max_retained_path_bytes > MAX_CONFIGURED_PATH_BYTES {
            return Err(DiscoveryLimitsError::InvalidPathByteLimit);
        }
        Ok(Self {
            max_files,
            max_retained_path_bytes,
        })
    }

    /// Maximum supported native files returned by one discovery.
    #[must_use]
    pub const fn max_files(self) -> usize {
        self.max_files
    }

    /// Maximum modeled manifest memory retained by one discovery.
    #[must_use]
    pub const fn max_retained_path_bytes(self) -> u64 {
        self.max_retained_path_bytes
    }
}

/// A configured discovery bound was zero or exceeded its hard ceiling.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DiscoveryLimitsError {
    /// File-count policy was empty or unreasonably large.
    #[error("native discovery file limit is invalid")]
    InvalidFileLimit,
    /// Retained path-manifest policy was empty or unreasonably large.
    #[error("native discovery path-byte limit is invalid")]
    InvalidPathByteLimit,
}

/// Discovery policy plus a caller-owned cooperative cancellation probe.
pub struct SourceDiscoveryOptions<Cancel> {
    limits: DiscoveryLimits,
    cancelled: Cancel,
}

impl<Cancel> SourceDiscoveryOptions<Cancel> {
    /// Bind hard manifest limits and cooperative cancellation.
    pub const fn new(limits: DiscoveryLimits, cancelled: Cancel) -> Self {
        Self { limits, cancelled }
    }
}

/// Exact evidence that discovery skipped paths a project chose to exclude.
///
/// A partial index that looks complete is worse than a failed one, so an
/// exclusion always reports itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceExclusionEvidence {
    excluded_paths: u64,
    excluded_trees: u64,
}

impl SourceExclusionEvidence {
    /// Number of discovered files rejected by a configured exclusion glob or a
    /// `.cartographignore` pattern.
    #[must_use]
    pub const fn excluded_paths(&self) -> u64 {
        self.excluded_paths
    }

    /// Number of directory subtrees skipped without being descended into.
    ///
    /// A pruned tree is deliberately not expanded into a file count: walking it
    /// to produce one would defeat the pruning that keeps discovery bounded on
    /// vendored and generated directories.
    #[must_use]
    pub const fn excluded_trees(&self) -> u64 {
        self.excluded_trees
    }
}

impl SourceRoot {
    /// Discover supported native sources under Git-compatible ignore rules.
    /// # Errors
    ///
    /// Returns an error if directory/ignore traversal fails, a path escapes the
    /// root, or file/retained-manifest bounds are exceeded.
    pub fn discover(
        &self,
        limits: DiscoveryLimits,
    ) -> Result<Vec<DiscoveredSource>, SourceDiscoveryError> {
        self.discover_with_cancellation(SourceDiscoveryOptions::new(limits, || false))
    }

    /// Walk without following symlinks while polling cancellation between entries.
    /// # Errors
    ///
    /// Returns an error on cancellation, unsafe/unreadable traversal, root
    /// escape, or exceeded file/retained-manifest bounds.
    pub fn discover_with_cancellation<Cancel>(
        &self,
        options: SourceDiscoveryOptions<Cancel>,
    ) -> Result<Vec<DiscoveredSource>, SourceDiscoveryError>
    where
        Cancel: FnMut() -> bool,
    {
        self.discover_with_evidence(options)
            .map(|(sources, _)| sources)
    }

    /// Discover sources and report exactly what configured exclusions skipped.
    /// # Errors
    ///
    /// Returns an error on cancellation, unsafe/unreadable traversal, root
    /// escape, or exceeded file/retained-manifest bounds.
    pub fn discover_with_evidence<Cancel>(
        &self,
        options: SourceDiscoveryOptions<Cancel>,
    ) -> Result<(Vec<DiscoveredSource>, SourceExclusionEvidence), SourceDiscoveryError>
    where
        Cancel: FnMut() -> bool,
    {
        let SourceDiscoveryOptions {
            limits,
            mut cancelled,
        } = options;
        if cancelled() {
            return Err(SourceDiscoveryError::Cancelled);
        }
        let root = self.canonical_path();
        if has_ignore_marker(root) {
            return Ok((Vec::new(), SourceExclusionEvidence::default()));
        }
        let policy = self.discovery_policy();
        let nested = find_nested_repositories(
            NestedRepositoryScan {
                root,
                policy,
                limits,
            },
            &mut cancelled,
        )?;
        let nested_roots = nested
            .iter()
            .map(|repository| repository.path.clone())
            .collect::<BTreeSet<_>>();
        let mut collector = SourceCollector::new(root, policy, limits);
        collector.collect_walk(
            WalkRequest {
                root,
                prefix: None,
                nested_roots: &nested_roots,
            },
            &mut cancelled,
        )?;
        collector.collect_tracked(root, None, &mut cancelled)?;
        for repository in nested {
            if !repository.allowed(policy) {
                continue;
            }
            let nested_root = root.join(repository.path.as_str());
            collector.collect_walk(
                WalkRequest {
                    root: &nested_root,
                    prefix: Some(&repository.path),
                    nested_roots: &nested_roots,
                },
                &mut cancelled,
            )?;
            collector.collect_tracked(&nested_root, Some(&repository.path), &mut cancelled)?;
        }
        collector.finish()
    }
}

/// Credential-safe project discovery failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SourceDiscoveryError {
    /// Parent, sibling, or deadline cancellation interrupted the walk.
    #[error("native source discovery was cancelled")]
    Cancelled,
    /// An entry could not be read safely and a partial index was refused.
    #[error("native source discovery could not read the project tree")]
    Walk,
    /// A source path could not be represented as UTF-8.
    #[error("native source discovery found a non-UTF-8 path")]
    NonUtf8Path,
    /// A path escaped canonical project-relative normalization.
    #[error("native source discovery found an invalid project path")]
    InvalidPath,
    /// File-count or retained path-memory policy was exceeded.
    #[error("native source discovery exceeded its configured limit")]
    Limit,
    /// The bounded discovery vector could not grow without aborting the process.
    #[error("native source discovery could not reserve bounded memory")]
    ResourceLimit,
}

struct SourceCollector<'a> {
    project_root: &'a Path,
    policy: &'a DiscoveryPolicy,
    limits: DiscoveryLimits,
    sources: BTreeMap<NormalizedPath, DiscoveredSource>,
    retained_bytes: u64,
    /// One compiled matcher per directory carrying a pattern `.cartographignore`.
    marker_matchers: BTreeMap<PathBuf, Option<Gitignore>>,
    excluded_paths: u64,
    excluded_trees: Arc<AtomicU64>,
}

#[derive(Clone, Copy)]
struct WalkRequest<'a> {
    root: &'a Path,
    prefix: Option<&'a NormalizedPath>,
    nested_roots: &'a BTreeSet<NormalizedPath>,
}

impl<'a> SourceCollector<'a> {
    fn new(project_root: &'a Path, policy: &'a DiscoveryPolicy, limits: DiscoveryLimits) -> Self {
        Self {
            project_root,
            policy,
            limits,
            sources: BTreeMap::new(),
            retained_bytes: 0,
            marker_matchers: BTreeMap::new(),
            excluded_paths: 0,
            excluded_trees: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether any ancestor `.cartographignore` pattern file ignores this path.
    ///
    /// The walker applies these rules itself, but git-tracked collection does
    /// not go through the walker, so both paths must agree or an exclusion
    /// silently stops holding for tracked files.
    fn ignored_by_marker_patterns(&mut self, relative: &Path) -> bool {
        let mut directory = self.project_root.to_path_buf();
        let mut ancestors = vec![directory.clone()];
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                directory.push(component);
                ancestors.push(directory.clone());
            }
        }
        let absolute = self.project_root.join(relative);
        for ancestor in ancestors {
            let matcher = self
                .marker_matchers
                .entry(ancestor.clone())
                .or_insert_with(|| compile_ignore_marker(&ancestor));
            if let Some(matcher) = matcher.as_ref()
                && matcher
                    .matched_path_or_any_parents(&absolute, false)
                    .is_ignore()
            {
                return true;
            }
        }
        false
    }

    fn collect_walk<Cancel>(
        &mut self,
        input: WalkRequest<'_>,
        cancelled: &mut Cancel,
    ) -> Result<(), SourceDiscoveryError>
    where
        Cancel: FnMut() -> bool,
    {
        let WalkRequest {
            root: walk_root,
            prefix,
            nested_roots,
        } = input;
        let mut builder = WalkBuilder::new(walk_root);
        let filter = WalkFilter {
            root: walk_root.to_path_buf(),
            prefix: prefix.cloned(),
            policy: self.policy.clone(),
            nested_roots: nested_roots.clone(),
            excluded_trees: Arc::clone(&self.excluded_trees),
        };
        builder
            .standard_filters(true)
            .hidden(false)
            .follow_links(false)
            .sort_by_file_name(std::cmp::Ord::cmp)
            .add_custom_ignore_filename(CARTOGRAPH_IGNORE_MARKER)
            .filter_entry(move |entry| filter.includes(entry));
        for entry in builder.build() {
            if cancelled() {
                return Err(SourceDiscoveryError::Cancelled);
            }
            let entry = entry.map_err(|_| SourceDiscoveryError::Walk)?;
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let local = entry
                .path()
                .strip_prefix(walk_root)
                .map_err(|_| SourceDiscoveryError::Walk)?;
            let raw = prefixed_path(prefix, local)?;
            let metadata = entry.metadata().map_err(|_| SourceDiscoveryError::Walk)?;
            self.add(raw.as_str(), metadata.len())?;
        }
        Ok(())
    }

    fn collect_tracked<Cancel>(
        &mut self,
        repository_root: &Path,
        prefix: Option<&NormalizedPath>,
        cancelled: &mut Cancel,
    ) -> Result<(), SourceDiscoveryError>
    where
        Cancel: FnMut() -> bool,
    {
        let maximum_output = self
            .limits
            .max_retained_path_bytes
            .saturating_mul(TRACKED_PATH_OUTPUT_MULTIPLIER)
            .clamp(MINIMUM_GIT_PATH_OUTPUT_BYTES, MAX_GIT_PATH_OUTPUT_BYTES);
        let Some(paths) = git_tracked_paths(repository_root, maximum_output, cancelled)? else {
            return Ok(());
        };
        for local in paths {
            if cancelled() {
                return Err(SourceDiscoveryError::Cancelled);
            }
            let raw = prefixed_path(prefix, Path::new(&local))?;
            let actual = repository_root.join(&local);
            let metadata = match fs::symlink_metadata(actual) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                Ok(_) | Err(_) => continue,
            };
            if under_ignore_marker(self.project_root, Path::new(raw.as_str())) {
                continue;
            }
            self.add(raw.as_str(), metadata.len())?;
        }
        Ok(())
    }

    fn add(&mut self, raw: &str, byte_size: u64) -> Result<(), SourceDiscoveryError> {
        let path = NormalizedPath::parse(raw).map_err(|_| SourceDiscoveryError::InvalidPath)?;
        if is_internal_path(&path) || self.sources.contains_key(&path) {
            return Ok(());
        }
        if policy_excludes(self.policy, &path, false)
            || self.ignored_by_marker_patterns(Path::new(path.as_str()))
        {
            self.excluded_paths = self.excluded_paths.saturating_add(1);
            return Ok(());
        }
        if !self.policy.supports_path(path.as_str()) {
            return Ok(());
        }
        let source = DiscoveredSource::new(path.clone(), byte_size);
        let next_retained = self
            .retained_bytes
            .checked_add(source.modeled_retained_bytes())
            .ok_or(SourceDiscoveryError::Limit)?;
        if self.sources.len() >= self.limits.max_files
            || next_retained > self.limits.max_retained_path_bytes
        {
            return Err(SourceDiscoveryError::Limit);
        }
        self.sources.insert(path, source);
        self.retained_bytes = next_retained;
        Ok(())
    }

    fn finish(
        self,
    ) -> Result<(Vec<DiscoveredSource>, SourceExclusionEvidence), SourceDiscoveryError> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(self.sources.len())
            .map_err(|_| SourceDiscoveryError::ResourceLimit)?;
        output.extend(self.sources.into_values());
        Ok((
            output,
            SourceExclusionEvidence {
                excluded_paths: self.excluded_paths,
                excluded_trees: self.excluded_trees.load(Ordering::Relaxed),
            },
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NestedRepositoryKind {
    Submodule,
    Embedded,
}

struct NestedRepository {
    path: NormalizedPath,
    kind: NestedRepositoryKind,
}

#[derive(Clone, Copy)]
struct NestedRepositoryScan<'a> {
    root: &'a Path,
    policy: &'a DiscoveryPolicy,
    limits: DiscoveryLimits,
}

impl NestedRepository {
    const fn allowed(&self, policy: &DiscoveryPolicy) -> bool {
        match self.kind {
            NestedRepositoryKind::Submodule => policy.index_submodules(),
            NestedRepositoryKind::Embedded => policy.index_embedded_repositories(),
        }
    }
}

fn find_nested_repositories<Cancel>(
    input: NestedRepositoryScan<'_>,
    cancelled: &mut Cancel,
) -> Result<Vec<NestedRepository>, SourceDiscoveryError>
where
    Cancel: FnMut() -> bool,
{
    let NestedRepositoryScan {
        root,
        policy,
        limits,
    } = input;
    let submodules = read_gitmodule_paths(root)?;
    let maximum_directories = limits
        .max_files
        .saturating_mul(NESTED_DIRECTORY_MULTIPLIER)
        .clamp(
            MIN_NESTED_DIRECTORY_SCAN_LIMIT,
            MAX_NESTED_DIRECTORY_SCAN_LIMIT,
        );
    let mut stack = vec![root.to_path_buf()];
    let mut scanned = 0_usize;
    let mut repositories = BTreeMap::<NormalizedPath, NestedRepositoryKind>::new();
    let discovery = NestedDirectoryDiscovery {
        root,
        policy,
        submodules: &submodules,
    };
    while let Some(directory) = stack.pop() {
        if cancelled() {
            return Err(SourceDiscoveryError::Cancelled);
        }
        scanned = scanned.checked_add(1).ok_or(SourceDiscoveryError::Limit)?;
        if scanned > maximum_directories {
            return Err(SourceDiscoveryError::Limit);
        }
        let mut entries = fs::read_dir(&directory)
            .map_err(|_| SourceDiscoveryError::Walk)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| SourceDiscoveryError::Walk)?;
        entries.sort_unstable_by_key(fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            if cancelled() {
                return Err(SourceDiscoveryError::Cancelled);
            }
            let Some((path, normalized, kind)) = discovery.candidate(&entry)? else {
                continue;
            };
            if let Some(kind) = kind {
                repositories.insert(normalized, kind);
            }
            stack.push(path);
        }
    }
    Ok(repositories
        .into_iter()
        .map(|(path, kind)| NestedRepository { path, kind })
        .collect())
}

struct NestedDirectoryDiscovery<'a> {
    root: &'a Path,
    policy: &'a DiscoveryPolicy,
    submodules: &'a BTreeSet<NormalizedPath>,
}

impl NestedDirectoryDiscovery<'_> {
    fn candidate(
        &self,
        entry: &fs::DirEntry,
    ) -> Result<Option<(PathBuf, NormalizedPath, Option<NestedRepositoryKind>)>, SourceDiscoveryError>
    {
        let file_type = entry.file_type().map_err(|_| SourceDiscoveryError::Walk)?;
        if !file_type.is_dir() || file_type.is_symlink() {
            return Ok(None);
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(self.root)
            .map_err(|_| SourceDiscoveryError::Walk)?;
        let raw = relative.to_str().ok_or(SourceDiscoveryError::NonUtf8Path)?;
        let normalized =
            NormalizedPath::parse(raw).map_err(|_| SourceDiscoveryError::InvalidPath)?;
        if is_internal_path(&normalized)
            || policy_excludes(self.policy, &normalized, true)
            || has_ignore_marker(&path)
        {
            return Ok(None);
        }
        let kind = nested_repository_kind(&path, &normalized, self.submodules);
        Ok(Some((path, normalized, kind)))
    }
}

fn nested_repository_kind(
    path: &Path,
    normalized: &NormalizedPath,
    submodules: &BTreeSet<NormalizedPath>,
) -> Option<NestedRepositoryKind> {
    let metadata = fs::symlink_metadata(path.join(GIT_DIRECTORY)).ok()?;
    let file_type = metadata.file_type();
    if !file_type.is_dir() && !file_type.is_file() {
        return None;
    }
    Some(if submodules.contains(normalized) {
        NestedRepositoryKind::Submodule
    } else {
        NestedRepositoryKind::Embedded
    })
}

fn read_gitmodule_paths(root: &Path) -> Result<BTreeSet<NormalizedPath>, SourceDiscoveryError> {
    let path = root.join(".gitmodules");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) | Err(_) => return Ok(BTreeSet::new()),
    };
    if metadata.len() > MAX_GITMODULES_BYTES {
        return Err(SourceDiscoveryError::Limit);
    }
    let file = fs::File::open(path).map_err(|_| SourceDiscoveryError::Walk)?;
    let mut bytes = Vec::new();
    let reserve = usize::try_from(metadata.len()).map_err(|_| SourceDiscoveryError::Limit)?;
    bytes
        .try_reserve_exact(reserve)
        .map_err(|_| SourceDiscoveryError::ResourceLimit)?;
    file.take(MAX_GITMODULES_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| SourceDiscoveryError::Walk)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_GITMODULES_BYTES {
        return Err(SourceDiscoveryError::Limit);
    }
    let source = std::str::from_utf8(&bytes).map_err(|_| SourceDiscoveryError::Walk)?;
    let mut paths = BTreeSet::new();
    for line in source.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        if key.trim() != "path" {
            continue;
        }
        let path =
            NormalizedPath::parse(value.trim()).map_err(|_| SourceDiscoveryError::InvalidPath)?;
        paths.insert(path);
    }
    Ok(paths)
}

fn git_tracked_paths<Cancel>(
    root: &Path,
    maximum_output: u64,
    cancelled: &mut Cancel,
) -> Result<Option<Vec<String>>, SourceDiscoveryError>
where
    Cancel: FnMut() -> bool,
{
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Ok(None);
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    };
    let mut output = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SourceDiscoveryError::Cancelled);
        }
        let read = stdout
            .read(&mut chunk)
            .map_err(|_| SourceDiscoveryError::Walk)?;
        if read == 0 {
            break;
        }
        let next = output
            .len()
            .checked_add(read)
            .ok_or(SourceDiscoveryError::Limit)?;
        if u64::try_from(next).unwrap_or(u64::MAX) > maximum_output {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SourceDiscoveryError::Limit);
        }
        output
            .try_reserve(read)
            .map_err(|_| SourceDiscoveryError::ResourceLimit)?;
        output.extend_from_slice(&chunk[..read]);
    }
    let status = child.wait().map_err(|_| SourceDiscoveryError::Walk)?;
    if !status.success() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for raw in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw).map_err(|_| SourceDiscoveryError::NonUtf8Path)?;
        paths
            .try_reserve(1)
            .map_err(|_| SourceDiscoveryError::ResourceLimit)?;
        paths.push(path.to_owned());
    }
    Ok(Some(paths))
}

struct WalkFilter {
    root: PathBuf,
    prefix: Option<NormalizedPath>,
    policy: DiscoveryPolicy,
    nested_roots: BTreeSet<NormalizedPath>,
    excluded_trees: Arc<AtomicU64>,
}

impl WalkFilter {
    fn includes(&self, entry: &DirEntry) -> bool {
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name();
        if name == GIT_DIRECTORY || name == CARTOGRAPH_DIRECTORY {
            return false;
        }
        let is_directory = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        if is_directory && has_ignore_marker(entry.path()) {
            return false;
        }
        let Ok(local) = entry.path().strip_prefix(&self.root) else {
            return true;
        };
        let Ok(path) = prefixed_path(self.prefix.as_ref(), local) else {
            return true;
        };
        if is_directory && self.nested_roots.contains(&path) {
            return false;
        }
        // A `!` re-include can name a path inside an excluded directory, and a
        // pruned directory is never descended into. When any negation is
        // configured the walk keeps descending and every file is still filtered
        // on admission, so `benches/**` plus `!benches/src/lib.rs` works.
        if is_directory && self.policy.has_exclude_negations() {
            return true;
        }
        if policy_excludes(&self.policy, &path, is_directory) {
            if is_directory {
                self.excluded_trees.fetch_add(1, Ordering::Relaxed);
            }
            return false;
        }
        true
    }
}

fn prefixed_path(
    prefix: Option<&NormalizedPath>,
    local: &Path,
) -> Result<NormalizedPath, SourceDiscoveryError> {
    let local = local.to_str().ok_or(SourceDiscoveryError::NonUtf8Path)?;
    let raw = prefix.map_or_else(
        || local.to_owned(),
        |prefix| format!("{}/{local}", prefix.as_str()),
    );
    NormalizedPath::parse(&raw).map_err(|_| SourceDiscoveryError::InvalidPath)
}

fn policy_excludes(policy: &DiscoveryPolicy, path: &NormalizedPath, directory: bool) -> bool {
    if policy.excludes(path.as_str()) {
        return true;
    }
    directory && policy.excludes(&format!("{}/.cartograph-probe", path.as_str()))
}

fn is_internal_path(path: &NormalizedPath) -> bool {
    path.as_str()
        .split('/')
        .next()
        .is_some_and(|component| matches!(component, GIT_DIRECTORY | CARTOGRAPH_DIRECTORY))
}

fn under_ignore_marker(root: &Path, relative: &Path) -> bool {
    let Some(parent) = relative.parent() else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component);
        if has_ignore_marker(&current) {
            return true;
        }
    }
    false
}

/// Compile a directory's pattern `.cartographignore` into a gitignore matcher.
fn compile_ignore_marker(directory: &Path) -> Option<Gitignore> {
    let marker = directory.join(CARTOGRAPH_IGNORE_MARKER);
    let metadata = fs::metadata(&marker).ok()?;
    if !metadata.is_file() || !ignore_marker_carries_patterns(&marker, metadata.len()) {
        return None;
    }
    let mut builder = GitignoreBuilder::new(directory);
    if builder.add(&marker).is_some() {
        return None;
    }
    builder.build().ok()
}

/// Whether this directory carries an empty `.cartographignore` tree marker.
///
/// An empty marker keeps its v1 meaning: exclude this directory tree outright.
/// A marker with patterns is instead honored as a gitignore-semantics ignore
/// file, so a project can skip `benches/**` without losing the whole directory.
fn has_ignore_marker(directory: &Path) -> bool {
    let marker = directory.join(CARTOGRAPH_IGNORE_MARKER);
    fs::metadata(&marker).is_ok_and(|metadata| {
        metadata.is_file() && !ignore_marker_carries_patterns(&marker, metadata.len())
    })
}

/// Whether a marker file holds at least one non-comment pattern line.
fn ignore_marker_carries_patterns(marker: &Path, byte_size: u64) -> bool {
    if byte_size == 0 || byte_size > MAX_IGNORE_MARKER_BYTES {
        return false;
    }
    fs::read_to_string(marker).is_ok_and(|contents| {
        contents.lines().any(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#')
        })
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tempfile::tempdir;

    use super::*;
    use crate::NestedRepositoryPolicy;

    const DISCOVERY_FILES: usize = 32;
    const DISCOVERY_PATH_BYTES: u64 = 64 * 1024;

    fn limits() -> DiscoveryLimits {
        match DiscoveryLimits::new(DISCOVERY_FILES, DISCOVERY_PATH_BYTES) {
            Ok(limits) => limits,
            Err(error) => panic!("test discovery limits were invalid: {error}"),
        }
    }

    #[test]
    fn discovery_is_sorted_gitignore_aware_and_honors_cartograph_markers() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create discovery fixture: {error}"),
        };
        let root = directory.path();
        assert!(fs::create_dir(root.join(".git")).is_ok());
        assert!(fs::create_dir_all(root.join("src/ignored-tree")).is_ok());
        assert!(fs::create_dir_all(root.join("src/marker-tree")).is_ok());
        assert!(fs::create_dir_all(root.join(".cartograph")).is_ok());
        assert!(fs::write(root.join(".gitignore"), "src/ignored-tree/\n").is_ok());
        assert!(fs::write(root.join("src/z.ts"), "export const z = 1;\n").is_ok());
        assert!(fs::write(root.join("src/a.tsx"), "export const A = () => <div />;\n").is_ok());
        assert!(fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").is_ok());
        assert!(fs::write(root.join("src/readme.md"), "not native yet\n").is_ok());
        assert!(fs::write(root.join("src/ignored-tree/no.ts"), "ignored\n").is_ok());
        assert!(fs::write(root.join("src/marker-tree/.cartographignore"), "\n").is_ok());
        assert!(fs::write(root.join("src/marker-tree/no.js"), "ignored\n").is_ok());
        assert!(fs::write(root.join(".cartograph/secret.ts"), "ignored\n").is_ok());

        let source_root = match SourceRoot::open(root) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open discovery root: {error}"),
        };
        let sources = match source_root.discover(limits()) {
            Ok(sources) => sources,
            Err(error) => panic!("discovery failed: {error}"),
        };
        let paths = sources
            .iter()
            .map(|source| source.path().as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["Cargo.toml", "src/a.tsx", "src/z.ts"]);
        assert!(sources.iter().all(|source| source.byte_size() > 0));
    }

    #[test]
    fn exclusion_negation_rescues_a_path_inside_an_excluded_directory() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create exclusion fixture: {error}"),
        };
        let root = directory.path();
        assert!(fs::create_dir_all(root.join("benches/src")).is_ok());
        assert!(fs::write(root.join("keep.ts"), "export const keep = 1;\n").is_ok());
        assert!(fs::write(root.join("benches/harness.ts"), "export const bench = 1;\n").is_ok());
        assert!(fs::write(root.join("benches/src/lib.ts"), "export const lib = 1;\n").is_ok());

        let excluded = discover_with_excludes(root, &["benches/**".to_owned()]);
        assert_eq!(excluded, ["keep.ts"], "an excluded tree must be skipped");

        // Excluding a directory to dodge one file must not throw away the rest
        // of it, so a `!` pattern re-includes a path inside that directory.
        let rescued = discover_with_excludes(
            root,
            &["benches/**".to_owned(), "!benches/src/lib.ts".to_owned()],
        );
        assert_eq!(rescued, ["benches/src/lib.ts", "keep.ts"]);
    }

    #[test]
    fn pattern_ignore_files_exclude_while_an_empty_marker_still_drops_the_tree() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create marker fixture: {error}"),
        };
        let root = directory.path();
        assert!(fs::create_dir_all(root.join("benches")).is_ok());
        assert!(fs::write(root.join("keep.ts"), "export const keep = 1;\n").is_ok());
        assert!(fs::write(root.join("benches/harness.ts"), "export const bench = 1;\n").is_ok());

        // A marker carrying patterns is a gitignore-semantics ignore file.
        assert!(
            fs::write(
                root.join(CARTOGRAPH_IGNORE_MARKER),
                "# skip the harness\nbenches/**\n"
            )
            .is_ok()
        );
        assert_eq!(discover_with_excludes(root, &[]), ["keep.ts"]);

        // An empty marker keeps its v1 meaning: drop this tree outright.
        assert!(fs::write(root.join(CARTOGRAPH_IGNORE_MARKER), "\n").is_ok());
        assert_eq!(discover_with_excludes(root, &[]), [] as [String; 0]);
    }

    #[test]
    fn discovery_reports_what_configured_exclusions_skipped() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create exclusion-evidence fixture: {error}"),
        };
        let root = directory.path();
        assert!(fs::create_dir_all(root.join("vendored")).is_ok());
        assert!(fs::write(root.join("keep.ts"), "export const keep = 1;\n").is_ok());
        assert!(fs::write(root.join("vendored/copy.ts"), "export const copy = 1;\n").is_ok());

        let policy = match DiscoveryPolicy::new(
            &["vendored/**".to_owned()],
            NestedRepositoryPolicy::new(false, false),
        ) {
            Ok(policy) => policy,
            Err(error) => panic!("exclusion policy failed: {error}"),
        };
        let source_root = match SourceRoot::open_with_policy(root, policy) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open exclusion root: {error}"),
        };
        let (sources, evidence) = match source_root
            .discover_with_evidence(SourceDiscoveryOptions::new(limits(), || false))
        {
            Ok(result) => result,
            Err(error) => panic!("discovery failed: {error}"),
        };
        assert_eq!(sources.len(), 1);
        // A partial index must never look complete.
        assert!(
            evidence.excluded_trees() > 0 || evidence.excluded_paths() > 0,
            "an exclusion must report itself"
        );
    }

    fn discover_with_excludes(root: &Path, excludes: &[String]) -> Vec<String> {
        let policy = match DiscoveryPolicy::new(excludes, NestedRepositoryPolicy::new(false, false))
        {
            Ok(policy) => policy,
            Err(error) => panic!("exclusion policy failed: {error}"),
        };
        let source_root = match SourceRoot::open_with_policy(root, policy) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open exclusion root: {error}"),
        };
        match source_root.discover(limits()) {
            Ok(sources) => sources
                .iter()
                .map(|source| source.path().as_str().to_owned())
                .collect(),
            Err(error) => panic!("discovery failed: {error}"),
        }
    }

    #[test]
    fn discovery_fails_closed_on_limits_and_cancellation() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create discovery fixture: {error}"),
        };
        assert!(fs::write(directory.path().join("one.ts"), "const one = 1;\n").is_ok());
        assert!(fs::write(directory.path().join("two.ts"), "const two = 2;\n").is_ok());
        let source_root = match SourceRoot::open(directory.path()) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open discovery root: {error}"),
        };
        let one_file = match DiscoveryLimits::new(1, DISCOVERY_PATH_BYTES) {
            Ok(limits) => limits,
            Err(error) => panic!("test discovery limits were invalid: {error}"),
        };
        assert_eq!(
            source_root.discover(one_file),
            Err(SourceDiscoveryError::Limit)
        );

        let polls = AtomicUsize::new(0);
        let result = source_root
            .discover_with_cancellation(SourceDiscoveryOptions::new(limits(), || {
                polls.fetch_add(1, Ordering::AcqRel) > 0
            }));
        assert_eq!(result, Err(SourceDiscoveryError::Cancelled));
    }

    #[test]
    fn unsupported_markdown_does_not_consume_native_file_limits() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create discovery fixture: {error}"),
        };
        let docs = directory.path().join("docs");
        assert!(fs::create_dir(&docs).is_ok());
        for index in 0..=DISCOVERY_FILES {
            assert!(fs::write(docs.join(format!("note-{index:02}.md")), "plain prose\n").is_ok());
        }
        assert!(
            fs::write(
                directory.path().join("zz-native.ts"),
                "export const native = true;\n"
            )
            .is_ok()
        );
        let source_root = match SourceRoot::open(directory.path()) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open discovery root: {error}"),
        };
        let one_file = match DiscoveryLimits::new(1, DISCOVERY_PATH_BYTES) {
            Ok(limits) => limits,
            Err(error) => panic!("test discovery limits were invalid: {error}"),
        };
        let sources = match source_root.discover(one_file) {
            Ok(sources) => sources,
            Err(error) => panic!("plain Markdown exhausted native discovery: {error}"),
        };
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path().as_str(), "zz-native.ts");
    }

    #[test]
    fn root_marker_opts_the_entire_project_out() {
        let directory = match tempdir() {
            Ok(directory) => directory,
            Err(error) => panic!("could not create discovery fixture: {error}"),
        };
        assert!(fs::write(directory.path().join(CARTOGRAPH_IGNORE_MARKER), "\n").is_ok());
        assert!(fs::write(directory.path().join("visible.ts"), "const value = 1;\n").is_ok());
        let source_root = match SourceRoot::open(directory.path()) {
            Ok(source_root) => source_root,
            Err(error) => panic!("could not open discovery root: {error}"),
        };
        assert_eq!(source_root.discover(limits()), Ok(Vec::new()));
    }

    #[test]
    fn discovery_restores_tracked_ignored_and_local_ignore_negation_then_applies_config_excludes() {
        let directory = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let root = directory.path();
        run_git(root, &["init", "--initial-branch=main"]);
        assert!(fs::write(root.join("tracked.ts"), "export const tracked = 1;\n").is_ok());
        run_git(root, &["add", "tracked.ts"]);
        assert!(fs::write(root.join("ignored.ts"), "export const ignored = 1;\n").is_ok());
        assert!(fs::write(root.join("restored.ts"), "export const restored = 1;\n").is_ok());
        assert!(fs::write(root.join(".gitignore"), "*.ts\n").is_ok());
        assert!(fs::write(root.join(".ignore"), "!restored.ts\n").is_ok());

        let source_root =
            SourceRoot::open(root).unwrap_or_else(|error| panic!("source root failed: {error}"));
        let paths = source_root
            .discover(limits())
            .unwrap_or_else(|error| panic!("tracked discovery failed: {error}"))
            .into_iter()
            .map(|source| source.path().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["restored.ts", "tracked.ts"]);

        let policy = DiscoveryPolicy::new(
            &["tracked.ts".to_owned()],
            NestedRepositoryPolicy::new(true, true),
        )
        .unwrap_or_else(|error| panic!("custom discovery policy failed: {error}"));
        let source_root = SourceRoot::open_with_policy(root, policy)
            .unwrap_or_else(|error| panic!("custom source root failed: {error}"));
        let paths = source_root
            .discover(limits())
            .unwrap_or_else(|error| panic!("custom discovery failed: {error}"))
            .into_iter()
            .map(|source| source.path().as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["restored.ts"]);
    }

    #[test]
    fn nested_repository_policy_distinguishes_submodules_embedded_repos_and_parent_ignores() {
        let directory = tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let root = directory.path();
        run_git(root, &["init", "--initial-branch=main"]);
        assert!(fs::create_dir_all(root.join("modules/sub")).is_ok());
        assert!(fs::write(root.join("modules/sub/.git"), "gitdir: unavailable\n").is_ok());
        assert!(
            fs::write(
                root.join("modules/sub/sub.ts"),
                "export const submodule = true;\n"
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join(".gitmodules"),
                "[submodule \"modules/sub\"]\n  path = modules/sub\n  url = local\n"
            )
            .is_ok()
        );
        assert!(fs::create_dir_all(root.join("third_party_sdk")).is_ok());
        run_git(
            &root.join("third_party_sdk"),
            &["init", "--initial-branch=main"],
        );
        assert!(
            fs::write(
                root.join("third_party_sdk/sdk.ts"),
                "export const embedded = true;\n"
            )
            .is_ok()
        );
        assert!(fs::write(root.join(".gitignore"), "third_party_sdk/\n").is_ok());
        assert!(fs::create_dir_all(root.join(".private-worktrees/example")).is_ok());
        run_git(
            &root.join(".private-worktrees/example"),
            &["init", "--initial-branch=main"],
        );
        assert!(
            fs::write(
                root.join(".private-worktrees/example/private.ts"),
                "export const privateWorktree = true;\n"
            )
            .is_ok()
        );
        assert!(
            fs::write(
                root.join(".gitignore"),
                "third_party_sdk/\n.private-worktrees/\n"
            )
            .is_ok()
        );

        assert_eq!(
            nested_paths(root, true, true),
            [
                ".private-worktrees/example/private.ts",
                "modules/sub/sub.ts",
                "third_party_sdk/sdk.ts"
            ]
        );
        assert_eq!(nested_paths(root, true, false), ["modules/sub/sub.ts"]);
        assert_eq!(nested_paths(root, false, true), [] as [String; 0]);
        assert_eq!(
            nested_paths_with_excludes(root, true, true, &[".private-worktrees/**".to_owned()]),
            ["modules/sub/sub.ts", "third_party_sdk/sdk.ts"]
        );
    }

    fn nested_paths(root: &Path, submodules: bool, embedded: bool) -> Vec<String> {
        nested_paths_with_excludes(root, submodules, embedded, &[])
    }

    fn nested_paths_with_excludes(
        root: &Path,
        submodules: bool,
        embedded: bool,
        excludes: &[String],
    ) -> Vec<String> {
        let policy =
            DiscoveryPolicy::new(excludes, NestedRepositoryPolicy::new(submodules, embedded))
                .unwrap_or_else(|error| panic!("nested policy failed: {error}"));
        SourceRoot::open_with_policy(root, policy)
            .unwrap_or_else(|error| panic!("nested source root failed: {error}"))
            .discover(limits())
            .unwrap_or_else(|error| panic!("nested discovery failed: {error}"))
            .into_iter()
            .map(|source| source.path().as_str().to_owned())
            .collect()
    }

    fn run_git(root: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .status()
            .unwrap_or_else(|error| panic!("git fixture failed: {error}"));
        assert!(status.success(), "git fixture returned {status}");
    }
}
