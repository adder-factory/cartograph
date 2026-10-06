use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use cartograph_db::ExternalImportRecord;
use cartograph_domain::{NormalizedPath, ProjectId};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{ProjectCancellation, ProjectRuntime};

const MAX_MANIFEST_BYTES: u64 = 1_024 * 1_024;
const MAX_MANIFEST_TOTAL_BYTES: u64 = 16 * 1_024 * 1_024;
const MAX_WORKSPACE_MANIFESTS: usize = 512;
const MAX_WORKSPACE_PATTERNS: usize = 128;
const MAX_WORKSPACE_PATTERN_BYTES: usize = 4_096;
const MAX_WALK_ENTRIES: usize = 250_000;
const MAX_WALK_DEPTH: usize = 12;
const MAX_NESTED_HINTS: usize = 20;
const MAX_SCRIPT_FILES: usize = 2_048;
const MAX_SCRIPT_FILE_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_SCRIPT_TOTAL_BYTES: u64 = 32 * 1_024 * 1_024;
const MAX_EVIDENCE_PATHS: usize = 20;
const MAX_CONFIG_BYTES: u64 = 1_024 * 1_024;
const MAX_PNPM_WORKSPACE_BYTES: u64 = 256 * 1_024;
const MAX_PACKAGE_SPECIFIER_BYTES: usize = 512;
const MAX_PACKAGE_NAME_BYTES: usize = 214;
const UNSUPPORTED_PACKAGE_PREFIXES: [&str; 9] = [
    ".", "/", "#", "node:", "bun:", "deno:", "jsr:", "http:", "https:",
];

/// Evidence explaining why one declared package is retained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyUseEvidence {
    package: String,
    import_sites: u64,
    import_paths: Vec<String>,
    script_references: u64,
    allowlisted: bool,
    configured_provider: bool,
    type_companion: bool,
}

/// An exact external import absent from every applicable manifest bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndeclaredDependency {
    package: String,
    import_sites: u64,
    import_paths: Vec<String>,
}

/// Deterministic JavaScript package audit composed from manifests and indexed imports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyAuditReport {
    root_manifest: Option<String>,
    workspace_manifests: Vec<String>,
    nested_manifest_hints: Vec<String>,
    declared_runtime: Vec<String>,
    declared_development: Vec<String>,
    declared_optional: Vec<String>,
    declared_peer: Vec<String>,
    used: Vec<DependencyUseEvidence>,
    unused_runtime_candidates: Vec<String>,
    unused_development_candidates: Vec<String>,
    unused_optional_candidates: Vec<String>,
    unused_peer_candidates: Vec<String>,
    undeclared: Vec<UndeclaredDependency>,
    evidence_complete: bool,
    caveats: Vec<&'static str>,
}

/// Safe package-audit failures.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DependencyAuditError {
    #[error("the project root is unavailable")]
    /// The project root could not be resolved or read safely.
    ProjectRootUnavailable,
    #[error("a package manifest is malformed or exceeds its bound")]
    /// A package manifest is malformed or violates the workspace contract.
    InvalidManifest,
    #[error("workspace discovery exceeds a deterministic bound")]
    /// Workspace expansion exceeded its declared package ceiling.
    WorkspaceLimit,
    #[error("a workspace pattern is invalid")]
    /// A workspace member pattern is malformed or escapes the root.
    InvalidWorkspacePattern,
    #[error("the dependency audit was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
    #[error("indexed import evidence is unavailable")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
}

impl ProjectRuntime {
    /// Audit package declarations against exact current-generation imports and bounded live config.
    /// # Errors
    ///
    /// Returns an error when the project root or a manifest cannot be read and
    /// validated within bounds, workspace expansion is unsafe or excessive,
    /// current import evidence is unavailable, or cancellation wins.
    pub async fn audit_javascript_dependencies(
        &self,
        project_id: &ProjectId,
        cancellation: ProjectCancellation,
    ) -> Result<DependencyAuditReport, DependencyAuditError> {
        if cancellation.is_cancelled() {
            return Err(DependencyAuditError::Cancelled);
        }
        let root = self.root.clone();
        let disk_cancellation = cancellation.clone();
        let disk =
            tokio::task::spawn_blocking(move || collect_disk_evidence(&root, &disk_cancellation));
        let imports = self.database().current_external_imports(project_id);
        let (disk, imports) = tokio::join!(disk, imports);
        let disk = disk.map_err(|_| DependencyAuditError::InvalidManifest)??;
        let imports = imports.map_err(|_| DependencyAuditError::StorageUnavailable)?;
        if cancellation.is_cancelled() {
            return Err(DependencyAuditError::Cancelled);
        }
        Ok(compose_report(disk, imports))
    }
}

#[derive(Default)]
struct DeclaredDependencies {
    runtime: BTreeSet<String>,
    development: BTreeSet<String>,
    optional: BTreeSet<String>,
    peer: BTreeSet<String>,
    workspace_packages: BTreeSet<String>,
}

impl DeclaredDependencies {
    fn all(&self) -> BTreeSet<String> {
        self.runtime
            .iter()
            .chain(&self.development)
            .chain(&self.optional)
            .chain(&self.peer)
            .cloned()
            .collect()
    }
}

struct ManifestRecord {
    path: NormalizedPath,
    directory: PathBuf,
    value: Map<String, Value>,
}

struct DiskEvidence {
    root_manifest: Option<String>,
    workspace_manifests: Vec<String>,
    nested_manifest_hints: Vec<String>,
    declared: DeclaredDependencies,
    scripts: Vec<String>,
    bin_to_package: BTreeMap<String, String>,
    allowlist: BTreeSet<String>,
    configured_providers: BTreeSet<String>,
    runtime_shims: BTreeSet<String>,
}

#[derive(Default)]
struct MutableUsage {
    import_sites: u64,
    import_paths: BTreeSet<String>,
    script_references: u64,
    allowlisted: bool,
    configured_provider: bool,
    type_companion: bool,
}

fn collect_disk_evidence(
    root: &Path,
    cancellation: &ProjectCancellation,
) -> Result<DiskEvidence, DependencyAuditError> {
    let root =
        std::fs::canonicalize(root).map_err(|_| DependencyAuditError::ProjectRootUnavailable)?;
    let root_manifest = root.join("package.json");
    if !root_manifest.is_file() {
        return Ok(DiskEvidence {
            root_manifest: None,
            workspace_manifests: Vec::new(),
            nested_manifest_hints: discover_package_manifests(&root, cancellation)?
                .into_iter()
                .take(MAX_NESTED_HINTS)
                .map(|path| path.as_str().to_owned())
                .collect(),
            declared: DeclaredDependencies::default(),
            scripts: Vec::new(),
            bin_to_package: BTreeMap::new(),
            allowlist: read_dependencies_allowlist(&root),
            configured_providers: BTreeSet::new(),
            runtime_shims: BTreeSet::new(),
        });
    }
    let root_record = read_manifest(&root, &root_manifest)?;
    let patterns = workspace_patterns(&root, &root_record.value)?;
    let manifest_paths = workspace_manifest_paths(&root, &patterns, cancellation)?;
    let records = read_workspace_manifests(&root, manifest_paths, cancellation)?;
    let declared = declared_dependencies(&records);
    let scripts = manifest_scripts(&records);
    let bin_to_package = collect_bin_names(BinNameScan {
        root: &root,
        records: &records,
        declared: &declared,
        cancellation,
    })?;
    let configured_providers = configured_provider_packages(&records);
    let runtime_shims = runtime_shims(&root, &scripts, &declared);
    let mut scripts = scripts;
    scripts.extend(read_script_files(&root, &records, cancellation)?);
    Ok(DiskEvidence {
        root_manifest: Some("package.json".to_owned()),
        workspace_manifests: records
            .iter()
            .map(|record| record.path.as_str().to_owned())
            .collect(),
        nested_manifest_hints: Vec::new(),
        declared,
        scripts,
        bin_to_package,
        allowlist: read_dependencies_allowlist(&root),
        configured_providers,
        runtime_shims,
    })
}

/// The root `package.json` plus every discovered workspace member manifest the
/// root's workspace patterns select, sorted, de-duplicated and bounded by the
/// member limit.
fn workspace_manifest_paths(
    root: &Path,
    patterns: &[String],
    cancellation: &ProjectCancellation,
) -> Result<Vec<PathBuf>, DependencyAuditError> {
    let mut manifest_paths = vec![root.join("package.json")];
    if !patterns.is_empty() {
        let matcher = WorkspaceMatcher::new(patterns)?;
        manifest_paths.extend(
            discover_package_manifests(root, cancellation)?
                .into_iter()
                .filter(|path| path.as_str() != "package.json")
                .filter(|path| matcher.matches(path.as_str()))
                .map(|path| root.join(path.as_str())),
        );
    }
    manifest_paths.sort();
    manifest_paths.dedup();
    if manifest_paths.len() > MAX_WORKSPACE_MANIFESTS {
        return Err(DependencyAuditError::WorkspaceLimit);
    }
    Ok(manifest_paths)
}

/// Read every selected manifest in order under one cumulative byte bound,
/// checking cancellation before each file.
fn read_workspace_manifests(
    root: &Path,
    manifest_paths: Vec<PathBuf>,
    cancellation: &ProjectCancellation,
) -> Result<Vec<ManifestRecord>, DependencyAuditError> {
    let mut records = Vec::with_capacity(manifest_paths.len());
    let mut total_bytes = 0_u64;
    for path in manifest_paths {
        if cancellation.is_cancelled() {
            return Err(DependencyAuditError::Cancelled);
        }
        let metadata =
            std::fs::metadata(&path).map_err(|_| DependencyAuditError::InvalidManifest)?;
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .filter(|bytes| *bytes <= MAX_MANIFEST_TOTAL_BYTES)
            .ok_or(DependencyAuditError::WorkspaceLimit)?;
        records.push(read_manifest(root, &path)?);
    }
    Ok(records)
}

fn compose_report(disk: DiskEvidence, imports: Vec<ExternalImportRecord>) -> DependencyAuditReport {
    let all_declared = disk.declared.all();
    let mut usage = import_usage(imports);
    ScriptUsageRecorder {
        usage: &mut usage,
        bin_to_package: &disk.bin_to_package,
        all_declared: &all_declared,
    }
    .record(&disk.scripts);
    mark_usage_flags(&mut usage, &disk.allowlist, &disk.configured_providers);
    mark_type_companions(&mut usage, &disk.declared.development);
    let used_names = usage.keys().cloned().collect::<BTreeSet<_>>();
    let unused_runtime_candidates = difference(&disk.declared.runtime, &used_names);
    let unused_development_candidates = difference(&disk.declared.development, &used_names);
    let unused_optional_candidates = difference(&disk.declared.optional, &used_names);
    let unused_peer_candidates = difference(&disk.declared.peer, &used_names);
    let undeclared = undeclared_dependencies(&usage, &all_declared, &disk);
    let used = declared_usage(usage, &all_declared);
    DependencyAuditReport {
        root_manifest: disk.root_manifest,
        workspace_manifests: disk.workspace_manifests,
        nested_manifest_hints: disk.nested_manifest_hints,
        declared_runtime: disk.declared.runtime.into_iter().collect(),
        declared_development: disk.declared.development.into_iter().collect(),
        declared_optional: disk.declared.optional.into_iter().collect(),
        declared_peer: disk.declared.peer.into_iter().collect(),
        used,
        unused_runtime_candidates,
        unused_development_candidates,
        unused_optional_candidates,
        unused_peer_candidates,
        undeclared,
        evidence_complete: true,
        caveats: vec![
            "Unused entries are removal candidates, not deletion instructions; opaque plugin and runtime loading can be invisible.",
            "Import evidence comes from the current indexed generation; script, bin, provider, allowlist, workspace, and ambient type signals are inspected separately.",
        ],
    }
}

fn import_usage(imports: Vec<ExternalImportRecord>) -> BTreeMap<String, MutableUsage> {
    let mut usage = BTreeMap::<String, MutableUsage>::new();
    for import in imports {
        if is_fixture_path(import.path()) {
            continue;
        }
        let Some(package) = package_name(import.module_specifier()) else {
            continue;
        };
        let evidence = usage.entry(package).or_default();
        evidence.import_sites = evidence.import_sites.saturating_add(import.site_count());
        if evidence.import_paths.len() < MAX_EVIDENCE_PATHS {
            evidence.import_paths.insert(import.path().to_owned());
        }
    }
    usage
}

struct ScriptUsageRecorder<'a> {
    usage: &'a mut BTreeMap<String, MutableUsage>,
    bin_to_package: &'a BTreeMap<String, String>,
    all_declared: &'a BTreeSet<String>,
}

impl ScriptUsageRecorder<'_> {
    fn record(&mut self, scripts: &[String]) {
        for script in scripts {
            for token in script_tokens(script) {
                if let Some(package) = self.bin_to_package.get(token) {
                    increment_script_reference(self.usage, package);
                } else if self.all_declared.contains(token) {
                    increment_script_reference(self.usage, token);
                }
            }
        }
    }
}

fn increment_script_reference(usage: &mut BTreeMap<String, MutableUsage>, package: &str) {
    let evidence = usage.entry(package.to_owned()).or_default();
    evidence.script_references = evidence.script_references.saturating_add(1);
}

fn mark_usage_flags(
    usage: &mut BTreeMap<String, MutableUsage>,
    allowlist: &BTreeSet<String>,
    configured_providers: &BTreeSet<String>,
) {
    for package in allowlist {
        usage.entry(package.clone()).or_default().allowlisted = true;
    }
    for package in configured_providers {
        usage
            .entry(package.clone())
            .or_default()
            .configured_provider = true;
    }
}

fn mark_type_companions(
    usage: &mut BTreeMap<String, MutableUsage>,
    development: &BTreeSet<String>,
) {
    let used_upstreams = usage.keys().cloned().collect::<BTreeSet<_>>();
    for package in development {
        if package == "@types/node"
            || types_upstream(package).is_some_and(|upstream| used_upstreams.contains(&upstream))
        {
            let evidence = usage.entry(package.clone()).or_default();
            evidence.type_companion = true;
        }
    }
}

fn undeclared_dependencies(
    usage: &BTreeMap<String, MutableUsage>,
    all_declared: &BTreeSet<String>,
    disk: &DiskEvidence,
) -> Vec<UndeclaredDependency> {
    usage
        .iter()
        .filter(|(package, evidence)| {
            evidence.import_sites > 0
                && !all_declared.contains(*package)
                && !disk.declared.workspace_packages.contains(*package)
                && !disk.runtime_shims.contains(*package)
                && !declared_types_companion(package, all_declared)
        })
        .map(|(package, evidence)| UndeclaredDependency {
            package: package.clone(),
            import_sites: evidence.import_sites,
            import_paths: evidence.import_paths.iter().cloned().collect(),
        })
        .collect()
}

fn declared_usage(
    usage: BTreeMap<String, MutableUsage>,
    all_declared: &BTreeSet<String>,
) -> Vec<DependencyUseEvidence> {
    usage
        .into_iter()
        .filter(|(package, _)| all_declared.contains(package))
        .map(|(package, evidence)| DependencyUseEvidence {
            package,
            import_sites: evidence.import_sites,
            import_paths: evidence.import_paths.into_iter().collect(),
            script_references: evidence.script_references,
            allowlisted: evidence.allowlisted,
            configured_provider: evidence.configured_provider,
            type_companion: evidence.type_companion,
        })
        .collect()
}

fn read_manifest(root: &Path, path: &Path) -> Result<ManifestRecord, DependencyAuditError> {
    let canonical =
        std::fs::canonicalize(path).map_err(|_| DependencyAuditError::InvalidManifest)?;
    if !canonical.starts_with(root) {
        return Err(DependencyAuditError::InvalidManifest);
    }
    let metadata =
        std::fs::metadata(&canonical).map_err(|_| DependencyAuditError::InvalidManifest)?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err(DependencyAuditError::InvalidManifest);
    }
    let bytes = std::fs::read(&canonical).map_err(|_| DependencyAuditError::InvalidManifest)?;
    let value = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or(DependencyAuditError::InvalidManifest)?;
    let relative = canonical
        .strip_prefix(root)
        .ok()
        .and_then(|path| NormalizedPath::parse(path.to_string_lossy().as_ref()).ok())
        .ok_or(DependencyAuditError::InvalidManifest)?;
    let directory = canonical
        .parent()
        .ok_or(DependencyAuditError::InvalidManifest)?
        .to_path_buf();
    Ok(ManifestRecord {
        path: relative,
        directory,
        value,
    })
}

fn discover_package_manifests(
    root: &Path,
    cancellation: &ProjectCancellation,
) -> Result<Vec<NormalizedPath>, DependencyAuditError> {
    let mut output = Vec::new();
    let mut visited = 0_usize;
    let cancellation = Arc::new(cancellation.clone());
    let filter_cancellation = cancellation.clone();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .max_depth(Some(MAX_WALK_DEPTH))
        .filter_entry(move |entry| {
            !filter_cancellation.is_cancelled()
                && entry
                    .file_name()
                    .to_str()
                    .is_none_or(|name| !is_skipped_directory(name))
        })
        .build();
    for entry in walker {
        if cancellation.is_cancelled() {
            return Err(DependencyAuditError::Cancelled);
        }
        visited = visited.saturating_add(1);
        if visited > MAX_WALK_ENTRIES {
            return Err(DependencyAuditError::WorkspaceLimit);
        }
        let entry = entry.map_err(|_| DependencyAuditError::WorkspaceLimit)?;
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && entry.file_name() == "package.json"
        {
            let relative = entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(|path| NormalizedPath::parse(path.to_string_lossy().as_ref()).ok())
                .ok_or(DependencyAuditError::InvalidManifest)?;
            output.push(relative);
        }
    }
    output.sort();
    output.dedup();
    Ok(output)
}

fn is_skipped_directory(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | ".git" | ".cartograph" | "target" | "dist" | "vendor" | "coverage"
    )
}

fn workspace_patterns(
    root: &Path,
    manifest: &Map<String, Value>,
) -> Result<Vec<String>, DependencyAuditError> {
    let mut patterns = Vec::new();
    match manifest.get("workspaces") {
        Some(Value::Array(values)) => append_string_values(&mut patterns, values)?,
        Some(Value::Object(value)) => {
            if let Some(Value::Array(values)) = value.get("packages") {
                append_string_values(&mut patterns, values)?;
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(DependencyAuditError::InvalidManifest),
    }
    append_pnpm_patterns(root, &mut patterns)?;
    if patterns.len() > MAX_WORKSPACE_PATTERNS {
        return Err(DependencyAuditError::WorkspaceLimit);
    }
    patterns.sort();
    patterns.dedup();
    Ok(patterns)
}

fn append_string_values(
    output: &mut Vec<String>,
    values: &[Value],
) -> Result<(), DependencyAuditError> {
    for value in values {
        let pattern = value
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= MAX_WORKSPACE_PATTERN_BYTES)
            .ok_or(DependencyAuditError::InvalidWorkspacePattern)?;
        output.push(pattern.to_owned());
    }
    Ok(())
}

fn append_pnpm_patterns(root: &Path, output: &mut Vec<String>) -> Result<(), DependencyAuditError> {
    let path = root.join("pnpm-workspace.yaml");
    if !path.is_file() {
        return Ok(());
    }
    let metadata =
        std::fs::metadata(&path).map_err(|_| DependencyAuditError::InvalidWorkspacePattern)?;
    if metadata.len() > MAX_PNPM_WORKSPACE_BYTES {
        return Err(DependencyAuditError::WorkspaceLimit);
    }
    let source =
        std::fs::read_to_string(path).map_err(|_| DependencyAuditError::InvalidWorkspacePattern)?;
    let mut in_packages = false;
    for raw in source.lines() {
        let line = raw.trim();
        if line == "packages:" {
            in_packages = true;
            continue;
        }
        if in_packages && !raw.starts_with(char::is_whitespace) && !line.is_empty() {
            in_packages = false;
        }
        if !in_packages || !line.starts_with('-') {
            continue;
        }
        let value = line[1..].trim().trim_matches(['\'', '"']);
        if value.is_empty() || value.len() > MAX_WORKSPACE_PATTERN_BYTES {
            return Err(DependencyAuditError::InvalidWorkspacePattern);
        }
        output.push(value.to_owned());
    }
    Ok(())
}

struct WorkspaceMatcher {
    include: GlobSet,
    exclude: Option<GlobSet>,
}

impl WorkspaceMatcher {
    fn new(patterns: &[String]) -> Result<Self, DependencyAuditError> {
        let mut include = GlobSetBuilder::new();
        let mut exclude = GlobSetBuilder::new();
        let mut exclusions = 0_usize;
        for pattern in patterns {
            let (negated, pattern) = pattern
                .strip_prefix('!')
                .map_or((false, pattern.as_str()), |value| (true, value));
            let pattern = format!("{}/package.json", pattern.trim_end_matches('/'));
            let glob =
                Glob::new(&pattern).map_err(|_| DependencyAuditError::InvalidWorkspacePattern)?;
            if negated {
                exclude.add(glob);
                exclusions += 1;
            } else {
                include.add(glob);
            }
        }
        Ok(Self {
            include: include
                .build()
                .map_err(|_| DependencyAuditError::InvalidWorkspacePattern)?,
            exclude: (exclusions > 0)
                .then(|| exclude.build())
                .transpose()
                .map_err(|_| DependencyAuditError::InvalidWorkspacePattern)?,
        })
    }

    fn matches(&self, path: &str) -> bool {
        self.include.is_match(path)
            && self
                .exclude
                .as_ref()
                .is_none_or(|exclude| !exclude.is_match(path))
    }
}

fn declared_dependencies(records: &[ManifestRecord]) -> DeclaredDependencies {
    let mut declared = DeclaredDependencies::default();
    for record in records {
        append_object_keys(&record.value, "dependencies", &mut declared.runtime);
        append_object_keys(&record.value, "devDependencies", &mut declared.development);
        append_object_keys(
            &record.value,
            "optionalDependencies",
            &mut declared.optional,
        );
        append_object_keys(&record.value, "peerDependencies", &mut declared.peer);
        if let Some(Value::String(name)) = record.value.get("name")
            && !name.is_empty()
        {
            declared.workspace_packages.insert(name.clone());
        }
    }
    declared
}

fn append_object_keys(manifest: &Map<String, Value>, key: &str, output: &mut BTreeSet<String>) {
    if let Some(Value::Object(values)) = manifest.get(key) {
        output.extend(values.keys().filter(|key| !key.is_empty()).cloned());
    }
}

fn manifest_scripts(records: &[ManifestRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| record.value.get("scripts").and_then(Value::as_object))
        .flat_map(|scripts| scripts.values().filter_map(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

#[derive(Clone, Copy)]
struct BinNameScan<'a> {
    root: &'a Path,
    records: &'a [ManifestRecord],
    declared: &'a DeclaredDependencies,
    cancellation: &'a ProjectCancellation,
}

fn collect_bin_names(
    input: BinNameScan<'_>,
) -> Result<BTreeMap<String, String>, DependencyAuditError> {
    let BinNameScan {
        root,
        records,
        declared,
        cancellation,
    } = input;
    let mut output = BTreeMap::new();
    for package in declared.all() {
        if cancellation.is_cancelled() {
            return Err(DependencyAuditError::Cancelled);
        }
        output.insert(package.clone(), package.clone());
        if let Some(basename) = package.rsplit('/').next() {
            output.entry(basename.to_owned()).or_insert(package.clone());
        }
        for bin in package_bin_names(root, records, &package) {
            output.insert(bin, package.clone());
        }
    }
    Ok(output)
}

fn package_bin_names(root: &Path, records: &[ManifestRecord], package: &str) -> Vec<String> {
    for directory in records
        .iter()
        .map(|record| record.directory.as_path())
        .chain(std::iter::once(root))
    {
        let path = directory
            .join("node_modules")
            .join(package)
            .join("package.json");
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        return manifest_bin_names(&value, package);
    }
    Vec::new()
}

fn manifest_bin_names(value: &Value, package: &str) -> Vec<String> {
    match value.get("bin") {
        Some(Value::String(_)) => package
            .rsplit('/')
            .next()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        Some(Value::Object(bins)) => bins.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn read_script_files(
    root: &Path,
    records: &[ManifestRecord],
    cancellation: &ProjectCancellation,
) -> Result<Vec<String>, DependencyAuditError> {
    let mut roots = records
        .iter()
        .map(|record| record.directory.join("scripts"))
        .chain(std::iter::once(root.join("scripts")))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    let mut state = ScriptReadState::default();
    for script_root in roots {
        read_script_root(&script_root, cancellation, &mut state)?;
    }
    Ok(state.output)
}

#[derive(Default)]
struct ScriptReadState {
    output: Vec<String>,
    files: usize,
    bytes: u64,
}

fn read_script_root(
    script_root: &Path,
    cancellation: &ProjectCancellation,
    state: &mut ScriptReadState,
) -> Result<(), DependencyAuditError> {
    for entry in WalkBuilder::new(script_root)
        .hidden(true)
        .follow_links(false)
        .max_depth(Some(5))
        .build()
    {
        if cancellation.is_cancelled() {
            return Err(DependencyAuditError::Cancelled);
        }
        let entry = entry.map_err(|_| DependencyAuditError::WorkspaceLimit)?;
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || !is_script_extension(entry.path())
        {
            continue;
        }
        append_script_file(&entry, state)?;
    }
    Ok(())
}

fn append_script_file(
    entry: &ignore::DirEntry,
    state: &mut ScriptReadState,
) -> Result<(), DependencyAuditError> {
    state.files = state.files.saturating_add(1);
    if state.files > MAX_SCRIPT_FILES {
        return Err(DependencyAuditError::WorkspaceLimit);
    }
    let metadata = entry
        .metadata()
        .map_err(|_| DependencyAuditError::WorkspaceLimit)?;
    if metadata.len() > MAX_SCRIPT_FILE_BYTES {
        return Err(DependencyAuditError::WorkspaceLimit);
    }
    state.bytes = state
        .bytes
        .checked_add(metadata.len())
        .filter(|value| *value <= MAX_SCRIPT_TOTAL_BYTES)
        .ok_or(DependencyAuditError::WorkspaceLimit)?;
    state.output.push(
        std::fs::read_to_string(entry.path()).map_err(|_| DependencyAuditError::WorkspaceLimit)?,
    );
    Ok(())
}

fn is_script_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            matches!(
                extension,
                "ts" | "tsx" | "mts" | "cts" | "js" | "mjs" | "cjs" | "sh" | "bash"
            )
        })
}

fn configured_provider_packages(records: &[ManifestRecord]) -> BTreeSet<String> {
    let mut output = BTreeSet::new();
    for record in records {
        for basename in [
            "vitest.config.ts",
            "vitest.config.js",
            "vitest.config.mjs",
            "vitest.config.cjs",
            "vite.config.ts",
            "vite.config.js",
            "vite.config.mjs",
            "vite.config.cjs",
        ] {
            let path = record.directory.join(basename);
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            let compact = source
                .chars()
                .filter(|character| !character.is_ascii_whitespace())
                .collect::<String>();
            if contains_provider(&compact, "v8") {
                output.insert("@vitest/coverage-v8".to_owned());
            }
            if contains_provider(&compact, "istanbul") {
                output.insert("@vitest/coverage-istanbul".to_owned());
            }
        }
    }
    output
}

fn contains_provider(source: &str, provider: &str) -> bool {
    ['\'', '"', '`']
        .into_iter()
        .any(|quote| source.contains(&format!("provider:{quote}{provider}{quote}")))
}

fn runtime_shims(
    root: &Path,
    scripts: &[String],
    declared: &DeclaredDependencies,
) -> BTreeSet<String> {
    let mut output = BTreeSet::new();
    if !declared.all().contains("vitest")
        && (root.join("bunfig.toml").is_file()
            || scripts.iter().any(|script| script.contains("bun test")))
    {
        output.insert("vitest".to_owned());
    }
    output
}

fn read_dependencies_allowlist(root: &Path) -> BTreeSet<String> {
    let path = root.join(".cartograph/config.json");
    let Ok(metadata) = std::fs::metadata(&path) else {
        return BTreeSet::new();
    };
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return BTreeSet::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeSet::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return BTreeSet::new();
    };
    value
        .get("dependenciesAllowlist")
        .or_else(|| value.pointer("/analysis/dependenciesAllowlist"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_PACKAGE_NAME_BYTES)
        .map(ToOwned::to_owned)
        .collect()
}

fn script_tokens(source: &str) -> impl Iterator<Item = &str> {
    source
        .split(|character: char| {
            !(character.is_ascii_alphanumeric()
                || matches!(character, '@' | '_' | '-' | '.' | '/' | ':'))
        })
        .filter(|token| !token.is_empty())
        .map(|token| token.strip_prefix("./node_modules/.bin/").unwrap_or(token))
}

fn package_name(specifier: &str) -> Option<String> {
    let specifier = normalized_package_specifier(specifier)?;
    let package = package_from_specifier(specifier)?;
    (!is_node_builtin(&package) && plausible_package_name(&package)).then_some(package)
}

fn normalized_package_specifier(specifier: &str) -> Option<&str> {
    let specifier = specifier.trim();
    if invalid_package_specifier(specifier) {
        return None;
    }
    specifier
        .strip_prefix("npm:")
        .unwrap_or(specifier)
        .split(['?', '#'])
        .next()
}

fn package_from_specifier(specifier: &str) -> Option<String> {
    let mut parts = specifier.split('/');
    let first = parts.next()?;
    let package = if first.starts_with('@') {
        format!("{first}/{}", parts.next()?)
    } else {
        first.to_owned()
    };
    Some(package)
}

fn invalid_package_specifier(specifier: &str) -> bool {
    if specifier.is_empty()
        || specifier.len() > MAX_PACKAGE_SPECIFIER_BYTES
        || specifier.contains('\0')
    {
        return true;
    }
    UNSUPPORTED_PACKAGE_PREFIXES
        .iter()
        .any(|prefix| specifier.starts_with(prefix))
}

fn plausible_package_name(package: &str) -> bool {
    package.len() <= MAX_PACKAGE_NAME_BYTES
        && package
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
        && package.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '@' | '_' | '-' | '.' | '/')
        })
}

fn is_node_builtin(package: &str) -> bool {
    matches!(
        package,
        "assert"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "domain"
            | "events"
            | "fs"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "repl"
            | "stream"
            | "string_decoder"
            | "sys"
            | "timers"
            | "tls"
            | "trace_events"
            | "tty"
            | "url"
            | "util"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
            | "sqlite"
            | "test"
    )
}

fn is_fixture_path(path: &str) -> bool {
    [
        "docs/test-beds/",
        "__tests__/fixtures/",
        "test/fixtures/",
        "spec/fixtures/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

fn difference(declared: &BTreeSet<String>, used: &BTreeSet<String>) -> Vec<String> {
    declared.difference(used).cloned().collect()
}

fn types_upstream(package: &str) -> Option<String> {
    let package = package.strip_prefix("@types/")?;
    package.split_once("__").map_or_else(
        || Some(package.to_owned()),
        |(scope, name)| Some(format!("@{scope}/{name}")),
    )
}

fn declared_types_companion(package: &str, declared: &BTreeSet<String>) -> bool {
    let companion = if let Some(scoped) = package.strip_prefix('@') {
        let Some((scope, name)) = scoped.split_once('/') else {
            return false;
        };
        format!("@types/{scope}__{name}")
    } else {
        format!("@types/{package}")
    };
    declared.contains(&companion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_normalization_is_scoped_builtin_and_protocol_aware() {
        assert_eq!(package_name("lodash/debounce").as_deref(), Some("lodash"));
        assert_eq!(
            package_name("@scope/pkg/subpath").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(package_name("npm:zod/v4").as_deref(), Some("zod"));
        assert_eq!(package_name("node:sqlite"), None);
        assert_eq!(package_name("fs/promises"), None);
        assert_eq!(package_name("../local"), None);
    }

    #[test]
    fn workspace_matcher_honors_positive_and_negative_patterns() {
        let matcher =
            WorkspaceMatcher::new(&["packages/*".to_owned(), "!packages/private".to_owned()])
                .unwrap_or_else(|error| panic!("workspace matcher failed: {error}"));
        assert!(matcher.matches("packages/api/package.json"));
        assert!(!matcher.matches("packages/private/package.json"));
        assert!(!matcher.matches("examples/demo/package.json"));
    }

    #[test]
    fn script_tokens_preserve_scoped_bins_and_local_bin_paths() {
        let tokens =
            script_tokens("bunx @scope/tool && ./node_modules/.bin/vitest run").collect::<Vec<_>>();
        assert!(tokens.contains(&"@scope/tool"));
        assert!(tokens.contains(&"vitest"));
    }
}
