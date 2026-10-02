use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    sync::OnceLock,
};

use cartograph_db::ImportInsight;
use cartograph_domain::{ContentDigest, ProjectId, SourceLanguage, SourceManifestDigestBuilder};
use cartograph_extract::{
    DiscoveredSource, SourceDiscoveryOptions, SourceLimits, SourceReadOptions, SourceRoot,
    substitute_module_alias,
};
use memchr::memchr_iter;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::{ProjectCancellation, ProjectRuntime, discovery_limits};

const MAXIMUM_IMPORT_RESULTS: usize = 1_000;
const MAXIMUM_IMPORT_HITS: usize = 250_000;
const MAXIMUM_SIGNATURE_BYTES: usize = 4_096;
const MAXIMUM_CONFIG_BYTES: u64 = 1024 * 1024;
const MAXIMUM_FILTER_BYTES: usize = 4_096;

const IMPORT_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".svelte", ".py", ".go",
    ".rs", ".java", ".kt", ".cs", ".php", ".rb", ".dart", ".swift",
];

const JS_TO_TS_REWRITES: &[(&str, &[&str])] = &[
    (".js", &[".ts", ".tsx", ".d.ts"]),
    (".jsx", &[".tsx"]),
    (".mjs", &[".mts"]),
    (".cjs", &[".cts"]),
];

/// Which indexed source families participate in an import audit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAuditSource {
    #[default]
    /// Represents the static import audit source.
    Static,
    /// Represents the literal import audit source.
    Literal,
    /// Represents the all import audit source.
    All,
}

impl ImportAuditSource {
    #[must_use]
    /// Returns whether AST-confirmed static imports are included.
    pub const fn includes_static(self) -> bool {
        matches!(self, Self::Static | Self::All)
    }

    #[must_use]
    /// Returns whether import-shaped string literals are included.
    pub const fn includes_literal(self) -> bool {
        matches!(self, Self::Literal | Self::All)
    }
}

/// Filesystem/module-resolution class for one import specifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportAuditTarget {
    /// Represents the file import audit target.
    File,
    /// Represents the directory import audit target.
    Directory,
    /// Represents the bare import audit target.
    Bare,
    /// Represents the unresolvable import audit target.
    Unresolvable,
    /// Represents the literal import audit target.
    Literal,
}

/// Whether a hit came from executable syntax or import-shaped string content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportOrigin {
    /// Represents the static import origin.
    Static,
    /// Represents the literal import origin.
    Literal,
}

/// Validated filtering and response policy for one audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportAuditOptions {
    source: ImportAuditSource,
    target: Option<ImportAuditTarget>,
    extension_missing: Option<bool>,
    dynamic: Option<bool>,
    path_filter: Option<String>,
    language: Option<String>,
    exclude_fixtures: bool,
    limit: usize,
}

impl ImportAuditOptions {
    /// Creates validated import-audit limits.
    ///
    /// # Errors
    ///
    /// Returns [`ImportAuditError::InvalidOptions`] when `limit` is zero or
    /// exceeds the maximum returned import count.
    pub fn new(limit: usize) -> Result<Self, ImportAuditError> {
        if limit == 0 || limit > MAXIMUM_IMPORT_RESULTS {
            return Err(ImportAuditError::InvalidOptions);
        }
        Ok(Self {
            source: ImportAuditSource::Static,
            target: None,
            extension_missing: None,
            dynamic: None,
            path_filter: None,
            language: None,
            exclude_fixtures: true,
            limit,
        })
    }

    #[must_use]
    /// Sets the source and returns the updated value.
    pub const fn with_source(mut self, source: ImportAuditSource) -> Self {
        self.source = source;
        self
    }

    #[must_use]
    /// Sets the target and returns the updated value.
    pub const fn with_target(mut self, target: Option<ImportAuditTarget>) -> Self {
        self.target = target;
        self
    }

    #[must_use]
    /// Sets the extension missing and returns the updated value.
    pub const fn with_extension_missing(mut self, value: Option<bool>) -> Self {
        self.extension_missing = value;
        self
    }

    #[must_use]
    /// Sets the dynamic and returns the updated value.
    pub const fn with_dynamic(mut self, value: Option<bool>) -> Self {
        self.dynamic = value;
        self
    }

    /// Sets the path filter and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns [`ImportAuditError::InvalidOptions`] when the filter is empty,
    /// oversized, contains NUL, is absolute, or contains a parent segment.
    pub fn with_path_filter(mut self, value: Option<&str>) -> Result<Self, ImportAuditError> {
        self.path_filter = value.map(validate_path_filter).transpose()?;
        Ok(self)
    }

    /// Sets the language and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns [`ImportAuditError::InvalidOptions`] when the language filter is
    /// empty, oversized, contains NUL, or is not a known source-language label.
    pub fn with_language(mut self, value: Option<&str>) -> Result<Self, ImportAuditError> {
        self.language = value.map(validate_language_filter).transpose()?;
        Ok(self)
    }

    #[must_use]
    /// Sets the exclude fixtures and returns the updated value.
    pub const fn with_exclude_fixtures(mut self, value: bool) -> Self {
        self.exclude_fixtures = value;
        self
    }
}

impl Default for ImportAuditOptions {
    fn default() -> Self {
        Self {
            source: ImportAuditSource::Static,
            target: None,
            extension_missing: None,
            dynamic: None,
            path_filter: None,
            language: None,
            exclude_fixtures: true,
            limit: 200,
        }
    }
}

/// One actionable import occurrence with exact source and resolution evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportHit {
    symbol_id: Option<String>,
    file: String,
    line: u32,
    specifier: String,
    signature: String,
    signature_truncated: bool,
    target: ImportAuditTarget,
    target_path: Option<String>,
    graph_target_path: Option<String>,
    extension_missing: bool,
    dynamic: bool,
    origin: ImportOrigin,
    language: String,
    confidence: Option<f32>,
    provenance: Option<String>,
    represented_sites: u64,
}

impl ImportHit {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> Option<&str> {
        self.symbol_id.as_deref()
    }

    #[must_use]
    /// Returns the file.
    pub fn file(&self) -> &str {
        &self.file
    }

    #[must_use]
    /// Returns the line.
    pub const fn line(&self) -> u32 {
        self.line
    }

    #[must_use]
    /// Returns the specifier.
    pub fn specifier(&self) -> &str {
        &self.specifier
    }

    #[must_use]
    /// Returns the target path.
    pub fn target_path(&self) -> Option<&str> {
        self.target_path.as_deref()
    }
}

/// Complete pre-filter counts plus a bounded deterministic result page.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportAuditReport {
    source: ImportAuditSource,
    source_revision: ContentDigest,
    total: usize,
    matched: usize,
    fixture_hits_excluded: usize,
    literal_hits_excluded_by_target: usize,
    truncated: bool,
    hits: Vec<ImportHit>,
}

/// Project identity, filters, and cooperative cancellation for one import audit.
pub struct ImportAuditRequest {
    project_id: ProjectId,
    options: ImportAuditOptions,
    cancellation: ProjectCancellation,
}

impl ImportAuditRequest {
    #[must_use]
    /// Creates a validated import audit request.
    pub const fn new(
        project_id: ProjectId,
        options: ImportAuditOptions,
        cancellation: ProjectCancellation,
    ) -> Self {
        Self {
            project_id,
            options,
            cancellation,
        }
    }
}

/// Credential-safe import audit failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ImportAuditError {
    #[error("import audit options are invalid")]
    /// Supplied options violate a documented bound or invariant.
    InvalidOptions,
    #[error("current import evidence is unavailable")]
    /// The required durable storage operation could not complete.
    StorageUnavailable,
    #[error("the complete current source corpus could not be audited")]
    /// Required source evidence could not be read safely.
    SourceUnavailable,
    #[error("the source or published generation changed during the import audit")]
    /// Live source no longer matches the generation or digest fence.
    SourceChanged,
    #[error("the import audit exceeded its explicit evidence bound")]
    /// Admitted evidence exceeded its declared item ceiling.
    EvidenceLimit,
    #[error("the import audit was cancelled")]
    /// The caller requested cancellation before the bounded operation completed.
    Cancelled,
}

impl ProjectRuntime {
    /// Audit every bounded current-generation import before applying response filters.
    /// # Errors
    ///
    /// Returns an error when current import storage or complete live source is
    /// unavailable, evidence exceeds its ceiling, source/generation identity
    /// changes during the audit, or cancellation wins.
    pub async fn audit_imports(
        &self,
        request: ImportAuditRequest,
    ) -> Result<ImportAuditReport, ImportAuditError> {
        let scanned = self
            .scan_imports_checked(ImportScanInput {
                project_id: &request.project_id,
                source: request.options.source,
                cancellation: request.cancellation,
            })
            .await?;
        Ok(build_report(scanned, &request.options))
    }

    pub(crate) async fn complete_static_import_hits(
        &self,
        project_id: &ProjectId,
        cancellation: ProjectCancellation,
    ) -> Result<Vec<ImportHit>, ImportAuditError> {
        self.scan_imports_checked(ImportScanInput {
            project_id,
            source: ImportAuditSource::Static,
            cancellation,
        })
        .await
        .map(|scanned| scanned.hits)
    }

    async fn scan_imports_checked(
        &self,
        input: ImportScanInput<'_>,
    ) -> Result<ScannedImports, ImportAuditError> {
        let before = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ImportAuditError::StorageUnavailable)?;
        let before = before.ok_or(ImportAuditError::SourceChanged)?;
        let current = before
            .current
            .as_ref()
            .ok_or(ImportAuditError::SourceChanged)?;
        if before.project_id != *input.project_id {
            return Err(ImportAuditError::SourceChanged);
        }
        let imports = if input.source.includes_static() {
            self.database()
                .current_import_evidence(input.project_id)
                .await
                .map_err(|_| ImportAuditError::StorageUnavailable)?
        } else {
            Vec::new()
        };
        if input.cancellation.is_cancelled() {
            return Err(ImportAuditError::Cancelled);
        }
        let permit = tokio::select! {
            biased;
            () = input.cancellation.cancelled() => return Err(ImportAuditError::Cancelled),
            result = self.source_scan_permits.clone().acquire_owned() => {
                result.map_err(|_| ImportAuditError::SourceUnavailable)?
            }
        };
        let root = self.root.clone();
        let worker_cancellation = input.cancellation.clone();
        let mut scanned = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            scan_import_corpus(
                ImportCorpusInput {
                    root: &root,
                    imports,
                    source_filter: input.source,
                },
                || worker_cancellation.is_cancelled(),
            )
        })
        .await
        .map_err(|_| ImportAuditError::SourceUnavailable)??;
        if input.cancellation.is_cancelled() {
            return Err(ImportAuditError::Cancelled);
        }
        let observed = self
            .scan_source(None, input.cancellation.clone())
            .await
            .map_err(|_| ImportAuditError::SourceUnavailable)?;
        if observed.digest.as_str() != current.source_revision {
            return Err(ImportAuditError::SourceChanged);
        }
        scanned.source_revision = observed.digest;
        let after = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ImportAuditError::StorageUnavailable)?;
        let unchanged = after.as_ref().is_some_and(|snapshot| {
            snapshot.project_id == before.project_id
                && snapshot.current.as_ref().is_some_and(|after_current| {
                    after_current.generation_id == current.generation_id
                        && after_current.source_revision == current.source_revision
                })
        });
        if !unchanged {
            return Err(ImportAuditError::SourceChanged);
        }
        Ok(scanned)
    }
}

struct ScannedImports {
    source_revision: ContentDigest,
    hits: Vec<ImportHit>,
}

fn finish_import_scan(
    digest: SourceManifestDigestBuilder,
    mut hits: Vec<ImportHit>,
) -> Result<ScannedImports, ImportAuditError> {
    hits.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.origin.cmp(&right.origin))
            .then_with(|| left.specifier.cmp(&right.specifier))
    });
    Ok(ScannedImports {
        source_revision: digest
            .finish()
            .map_err(|_| ImportAuditError::SourceUnavailable)?,
        hits,
    })
}

struct ImportScanInput<'scan> {
    project_id: &'scan ProjectId,
    source: ImportAuditSource,
    cancellation: ProjectCancellation,
}

struct ImportCorpusInput<'corpus> {
    root: &'corpus Path,
    imports: Vec<ImportInsight>,
    source_filter: ImportAuditSource,
}

struct PreparedImportCorpus<'corpus> {
    root: &'corpus Path,
    source_root: SourceRoot,
    files: Vec<DiscoveredSource>,
    read_limits: SourceLimits,
    known_paths: BTreeSet<String>,
    go_module: Option<String>,
    imports_by_path: BTreeMap<String, Vec<ImportInsight>>,
    digest: SourceManifestDigestBuilder,
    source_filter: ImportAuditSource,
}

#[derive(Clone, Copy)]
struct StaticHitInput<'hit> {
    known_paths: &'hit BTreeSet<String>,
    go_module: Option<&'hit str>,
    aliases: Option<&'hit TsAliases>,
    snapshot: &'hit cartograph_extract::SourceSnapshot,
    import: &'hit ImportInsight,
}

#[derive(Clone, Copy)]
struct LiteralSegment {
    start: usize,
    end: usize,
    start_line: u32,
    quote: u8,
}

fn prepare_import_corpus<'corpus, Cancel>(
    input: ImportCorpusInput<'corpus>,
    cancelled: &mut Cancel,
) -> Result<PreparedImportCorpus<'corpus>, ImportAuditError>
where
    Cancel: FnMut() -> bool,
{
    let source_policy = crate::project_source_policy(input.root)
        .map_err(|_| ImportAuditError::SourceUnavailable)?;
    let maximum_source_bytes = source_policy
        .maximum_file_bytes
        .unwrap_or(crate::DEFAULT_MAX_SOURCE_BYTES);
    let source_root = SourceRoot::open_with_policy(input.root, source_policy.discovery)
        .map_err(|_| ImportAuditError::SourceUnavailable)?;
    let discovery = discovery_limits().map_err(|_| ImportAuditError::SourceUnavailable)?;
    let read_limits = crate::source_limits_with_max(maximum_source_bytes)
        .map_err(|_| ImportAuditError::SourceUnavailable)?;
    let files = source_root
        .discover_with_cancellation(SourceDiscoveryOptions::new(discovery, cancelled))
        .map_err(map_source_error)?;
    let known_paths = files
        .iter()
        .map(|file| file.path().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let mut imports_by_path = BTreeMap::new();
    for import in input.imports {
        imports_by_path
            .entry(import.path().to_owned())
            .or_insert_with(Vec::new)
            .push(import);
    }
    Ok(PreparedImportCorpus {
        root: input.root,
        digest: SourceManifestDigestBuilder::new(files.len())
            .map_err(|_| ImportAuditError::SourceUnavailable)?,
        source_root,
        files,
        read_limits,
        known_paths,
        go_module: read_go_module(input.root),
        imports_by_path,
        source_filter: input.source_filter,
    })
}

fn scan_import_corpus<Cancel>(
    input: ImportCorpusInput<'_>,
    mut cancelled: Cancel,
) -> Result<ScannedImports, ImportAuditError>
where
    Cancel: FnMut() -> bool,
{
    let PreparedImportCorpus {
        root,
        source_root,
        files,
        read_limits,
        known_paths,
        go_module,
        mut imports_by_path,
        mut digest,
        source_filter,
    } = prepare_import_corpus(input, &mut cancelled)?;
    let mut hits = Vec::new();
    for file in &files {
        if cancelled() {
            return Err(ImportAuditError::Cancelled);
        }
        let snapshot = source_root
            .read_with_cancellation(
                file.path(),
                SourceReadOptions::new(read_limits, &mut cancelled),
            )
            .map_err(map_source_error)?;
        digest
            .push(file.path(), snapshot.content_hash())
            .map_err(|_| ImportAuditError::SourceUnavailable)?;
        scan_snapshot_imports(SnapshotImportScan {
            root,
            snapshot: &snapshot,
            known_paths: &known_paths,
            go_module: go_module.as_deref(),
            imports_by_path: &mut imports_by_path,
            source_filter,
            hits: &mut hits,
        })?;
    }
    if !imports_by_path.is_empty() {
        return Err(ImportAuditError::SourceChanged);
    }
    finish_import_scan(digest, hits)
}

struct SnapshotImportScan<'scan> {
    root: &'scan Path,
    snapshot: &'scan cartograph_extract::SourceSnapshot,
    known_paths: &'scan BTreeSet<String>,
    go_module: Option<&'scan str>,
    imports_by_path: &'scan mut BTreeMap<String, Vec<ImportInsight>>,
    source_filter: ImportAuditSource,
    hits: &'scan mut Vec<ImportHit>,
}

fn scan_snapshot_imports(input: SnapshotImportScan<'_>) -> Result<(), ImportAuditError> {
    let SnapshotImportScan {
        root,
        snapshot,
        known_paths,
        go_module,
        imports_by_path,
        source_filter,
        hits,
    } = input;
    if let Some(file_imports) = imports_by_path.remove(snapshot.path().as_str()) {
        let aliases = javascript_family(snapshot.language())
            .then(|| read_ts_aliases(root, snapshot.path().as_str()))
            .flatten();
        for import in file_imports {
            push_bounded(
                hits,
                static_hit(StaticHitInput {
                    known_paths,
                    go_module,
                    aliases: aliases.as_ref(),
                    snapshot,
                    import: &import,
                })?,
            )?;
        }
    }
    if source_filter.includes_literal() && javascript_family(snapshot.language()) {
        for hit in scan_literal_imports(
            snapshot.path().as_str(),
            snapshot.language().as_str(),
            snapshot.source(),
        )? {
            push_bounded(hits, hit)?;
        }
    }
    Ok(())
}

fn static_hit(input: StaticHitInput<'_>) -> Result<ImportHit, ImportAuditError> {
    let start =
        usize::try_from(input.import.start_byte()).map_err(|_| ImportAuditError::SourceChanged)?;
    let end =
        usize::try_from(input.import.end_byte()).map_err(|_| ImportAuditError::SourceChanged)?;
    let signature = input
        .snapshot
        .source()
        .get(start..end)
        .ok_or(ImportAuditError::SourceChanged)?
        .trim();
    let (signature, signature_truncated) = bounded_signature(signature);
    let dynamic = is_dynamic_import_signature(&signature);
    let c_include_style = if matches!(
        input.snapshot.language(),
        SourceLanguage::C | SourceLanguage::Cpp | SourceLanguage::Cuda
    ) {
        if signature.contains('"') {
            Some(CIncludeStyle::Quoted)
        } else {
            Some(CIncludeStyle::Angled)
        }
    } else {
        None
    };
    let classification = classify_import(ClassifyRequest {
        known_paths: input.known_paths,
        importing_path: input.snapshot.path().as_str(),
        language: input.snapshot.language(),
        specifier: input.import.module_specifier(),
        go_module: input.go_module,
        c_include_style,
        aliases: input.aliases,
    });
    let line = one_based_line(input.snapshot.source(), start)?;
    Ok(ImportHit {
        symbol_id: Some(input.import.evidence_symbol_id().to_owned()),
        file: input.snapshot.path().as_str().to_owned(),
        line,
        specifier: input.import.module_specifier().to_owned(),
        signature,
        signature_truncated,
        target: classification.target,
        target_path: classification.target_path,
        graph_target_path: input.import.target_path().map(str::to_owned),
        extension_missing: classification.extension_missing,
        dynamic,
        origin: ImportOrigin::Static,
        language: input.import.language().to_ascii_lowercase(),
        confidence: Some(input.import.confidence()),
        provenance: Some(input.import.provenance().to_owned()),
        represented_sites: input.import.site_count(),
    })
}

fn build_report(scanned: ScannedImports, options: &ImportAuditOptions) -> ImportAuditReport {
    let total = scanned.hits.len();
    let fixture_hits_excluded = if options.exclude_fixtures {
        scanned
            .hits
            .iter()
            .filter(|hit| is_fixture_path(&hit.file))
            .count()
    } else {
        0
    };
    let literal_hits_excluded_by_target = options.target.map_or(0, |_| {
        scanned
            .hits
            .iter()
            .filter(|hit| hit.origin == ImportOrigin::Literal)
            .count()
    });
    let mut matched_hits = scanned
        .hits
        .into_iter()
        .filter(|hit| hit_matches(hit, options))
        .collect::<Vec<_>>();
    let matched = matched_hits.len();
    let truncated = matched > options.limit;
    matched_hits.truncate(options.limit);
    ImportAuditReport {
        source: options.source,
        source_revision: scanned.source_revision,
        total,
        matched,
        fixture_hits_excluded,
        literal_hits_excluded_by_target,
        truncated,
        hits: matched_hits,
    }
}

fn hit_matches(hit: &ImportHit, options: &ImportAuditOptions) -> bool {
    if options
        .target
        .is_some_and(|target| hit.origin == ImportOrigin::Literal || hit.target != target)
    {
        return false;
    }
    if options
        .extension_missing
        .is_some_and(|expected| hit.extension_missing != expected)
    {
        return false;
    }
    if options
        .dynamic
        .is_some_and(|expected| hit.dynamic != expected)
    {
        return false;
    }
    if options
        .path_filter
        .as_ref()
        .is_some_and(|prefix| !hit.file.starts_with(prefix))
    {
        return false;
    }
    if options.exclude_fixtures && is_fixture_path(&hit.file) {
        return false;
    }
    options
        .language
        .as_ref()
        .is_none_or(|language| hit.language == *language)
}

fn push_bounded(hits: &mut Vec<ImportHit>, hit: ImportHit) -> Result<(), ImportAuditError> {
    if hits.len() >= MAXIMUM_IMPORT_HITS {
        return Err(ImportAuditError::EvidenceLimit);
    }
    hits.try_reserve(1)
        .map_err(|_| ImportAuditError::EvidenceLimit)?;
    hits.push(hit);
    Ok(())
}

#[derive(Clone, Copy)]
enum CIncludeStyle {
    Quoted,
    Angled,
}

#[derive(Clone, Copy)]
struct ClassifyRequest<'a> {
    known_paths: &'a BTreeSet<String>,
    importing_path: &'a str,
    language: SourceLanguage,
    specifier: &'a str,
    go_module: Option<&'a str>,
    c_include_style: Option<CIncludeStyle>,
    aliases: Option<&'a TsAliases>,
}

struct ImportClassification {
    target: ImportAuditTarget,
    target_path: Option<String>,
    extension_missing: bool,
}

fn classify_import(request: ClassifyRequest<'_>) -> ImportClassification {
    let relative = is_relative_specifier(request.specifier);
    let absolute = request.specifier.starts_with('/');
    if !relative && !absolute {
        return classify_bare_import(request);
    }
    let base = if absolute {
        normalize_join("", request.specifier.trim_start_matches('/'))
    } else {
        normalize_join(parent_path(request.importing_path), request.specifier)
    };
    let extension_missing = relative && !has_known_extension(request.specifier);
    base.map_or(
        ImportClassification {
            target: ImportAuditTarget::Unresolvable,
            target_path: None,
            extension_missing,
        },
        |base| classify_resolved_base(request.known_paths, &base, extension_missing),
    )
}

fn classify_bare_import(request: ClassifyRequest<'_>) -> ImportClassification {
    if request.language == SourceLanguage::Go
        && let Some(module) = request.go_module
        && let Some(stripped) = strip_module_alias(request.specifier, module)
    {
        return classify_resolved_base(request.known_paths, stripped, false);
    }
    if let Some(aliases) = request.aliases
        && let Some(classification) =
            classify_alias(request.known_paths, request.specifier, aliases)
    {
        return classification;
    }
    if matches!(request.c_include_style, Some(CIncludeStyle::Quoted)) {
        return classify_quoted_include(request);
    }
    ImportClassification {
        target: ImportAuditTarget::Bare,
        target_path: None,
        extension_missing: false,
    }
}

fn classify_quoted_include(request: ClassifyRequest<'_>) -> ImportClassification {
    let from_directory = parent_path(request.importing_path);
    if let Some(path) = normalize_join(from_directory, request.specifier) {
        let hit = classify_resolved_base(request.known_paths, &path, false);
        if hit.target != ImportAuditTarget::Unresolvable {
            return hit;
        }
    }
    let root_hit = classify_resolved_base(request.known_paths, request.specifier, false);
    if root_hit.target != ImportAuditTarget::Unresolvable {
        return root_hit;
    }
    ImportClassification {
        target: ImportAuditTarget::Unresolvable,
        target_path: None,
        extension_missing: false,
    }
}

fn classify_resolved_base(
    known_paths: &BTreeSet<String>,
    base: &str,
    extension_missing: bool,
) -> ImportClassification {
    if base.is_empty() {
        return ImportClassification {
            target: ImportAuditTarget::Directory,
            target_path: None,
            extension_missing,
        };
    }
    if known_paths.contains(base) {
        return resolved_file(base, extension_missing);
    }
    if let Some(path) = rewritten_import_path(known_paths, base) {
        return resolved_file(&path, extension_missing);
    }
    if let Some(path) = extension_import_path(known_paths, base) {
        return resolved_file(&path, extension_missing);
    }
    if let Some(path) = index_import_path(known_paths, base) {
        return ImportClassification {
            target: ImportAuditTarget::Directory,
            target_path: Some(path),
            extension_missing,
        };
    }
    if contains_directory_prefix(known_paths, base) {
        return ImportClassification {
            target: ImportAuditTarget::Directory,
            target_path: None,
            extension_missing,
        };
    }
    ImportClassification {
        target: ImportAuditTarget::Unresolvable,
        target_path: None,
        extension_missing,
    }
}

fn rewritten_import_path(known_paths: &BTreeSet<String>, base: &str) -> Option<String> {
    for (javascript, typescript) in JS_TO_TS_REWRITES {
        if let Some(stem) = base.strip_suffix(javascript) {
            for extension in *typescript {
                let candidate = format!("{stem}{extension}");
                if known_paths.contains(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn extension_import_path(known_paths: &BTreeSet<String>, base: &str) -> Option<String> {
    for extension in IMPORT_EXTENSIONS {
        let candidate = format!("{base}{extension}");
        if known_paths.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn index_import_path(known_paths: &BTreeSet<String>, base: &str) -> Option<String> {
    for extension in IMPORT_EXTENSIONS {
        let candidate = format!("{base}/index{extension}");
        if known_paths.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn contains_directory_prefix(known_paths: &BTreeSet<String>, base: &str) -> bool {
    let prefix = format!("{}/", base.trim_end_matches('/'));
    known_paths
        .range(prefix.clone()..)
        .next()
        .is_some_and(|path| path.starts_with(&prefix))
}

fn resolved_file(path: &str, extension_missing: bool) -> ImportClassification {
    ImportClassification {
        target: ImportAuditTarget::File,
        target_path: Some(path.to_owned()),
        extension_missing,
    }
}

fn is_relative_specifier(specifier: &str) -> bool {
    matches!(specifier, "." | "..") || specifier.starts_with("./") || specifier.starts_with("../")
}

fn has_known_extension(specifier: &str) -> bool {
    let last = specifier.rsplit('/').next().unwrap_or(specifier);
    IMPORT_EXTENSIONS
        .iter()
        .any(|extension| last.ends_with(extension))
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn normalize_join(base: &str, value: &str) -> Option<String> {
    let mut components = base
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for component in value.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            component if component.contains('\0') => return None,
            component => components.push(component.to_owned()),
        }
    }
    (!components.is_empty()).then(|| components.join("/"))
}

fn strip_module_alias<'a>(specifier: &'a str, module: &str) -> Option<&'a str> {
    if specifier == module {
        Some("")
    } else {
        specifier.strip_prefix(module)?.strip_prefix('/')
    }
}

fn read_go_module(root: &Path) -> Option<String> {
    let source = read_small_file(&root.join("go.mod"))?;
    source.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("module ")
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_whitespace))
            .map(str::to_owned)
    })
}

struct TsAliases {
    base_path: String,
    paths: BTreeMap<String, Vec<String>>,
}

fn read_ts_aliases(root: &Path, importing_path: &str) -> Option<TsAliases> {
    let mut directory = parent_path(importing_path).to_owned();
    loop {
        let config_path = if directory.is_empty() {
            root.join("tsconfig.json")
        } else {
            root.join(&directory).join("tsconfig.json")
        };
        if let Some(raw) = read_small_file(&config_path)
            && let Some(aliases) = parse_ts_aliases(&raw, &directory)
        {
            return Some(aliases);
        }
        let Some((parent, _)) = directory.rsplit_once('/') else {
            if directory.is_empty() {
                return None;
            }
            directory.clear();
            continue;
        };
        directory.truncate(parent.len());
    }
}

fn parse_ts_aliases(raw: &str, config_directory: &str) -> Option<TsAliases> {
    let parsed = serde_json::from_str::<Value>(&strip_json_comments(raw)).ok()?;
    let compiler = parsed.get("compilerOptions")?.as_object()?;
    let paths = compiler.get("paths")?.as_object()?;
    let base_url = compiler
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let base_path = normalize_join(config_directory, base_url)
        .or_else(|| config_directory.is_empty().then(String::new))?;
    let mut normalized = BTreeMap::new();
    for (pattern, substitutions) in paths {
        let values = substitutions
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !values.is_empty() {
            normalized.insert(pattern.clone(), values);
        }
    }
    (!normalized.is_empty()).then_some(TsAliases {
        base_path,
        paths: normalized,
    })
}

fn classify_alias(
    known_paths: &BTreeSet<String>,
    specifier: &str,
    aliases: &TsAliases,
) -> Option<ImportClassification> {
    for (pattern, substitutions) in &aliases.paths {
        let Some(tail) = alias_tail(specifier, pattern) else {
            continue;
        };
        for substitution in substitutions {
            let replaced = substitute_module_alias(substitution, tail);
            let Some(base) = normalize_join(&aliases.base_path, &replaced) else {
                continue;
            };
            let classification = classify_resolved_base(known_paths, &base, false);
            if classification.target != ImportAuditTarget::Unresolvable {
                return Some(classification);
            }
        }
        return Some(ImportClassification {
            target: ImportAuditTarget::Unresolvable,
            target_path: None,
            extension_missing: false,
        });
    }
    None
}

fn alias_tail<'a>(specifier: &'a str, pattern: &str) -> Option<&'a str> {
    let Some(wildcard) = pattern.find('*') else {
        return (specifier == pattern).then_some("");
    };
    let prefix = &pattern[..wildcard];
    let suffix = &pattern[wildcard + 1..];
    specifier.strip_circumfix(prefix, suffix)
}

fn read_small_file(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAXIMUM_CONFIG_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn strip_json_comments(source: &str) -> String {
    JsonCommentStripper::new(source).finish()
}

struct JsonCommentStripper<'source> {
    source: &'source str,
    output: Vec<u8>,
    index: usize,
    quote: Option<u8>,
}

impl<'source> JsonCommentStripper<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            output: Vec::with_capacity(source.len()),
            index: 0,
            quote: None,
        }
    }

    fn finish(mut self) -> String {
        while self.index < self.source.len() {
            self.copy_next();
        }
        String::from_utf8(self.output).unwrap_or_else(|_| self.source.to_owned())
    }

    fn copy_next(&mut self) {
        let bytes = self.source.as_bytes();
        let byte = bytes[self.index];
        if self.copy_quoted_byte(bytes, byte) {
            return;
        }
        if matches!(byte, b'"' | b'\'') {
            self.quote = Some(byte);
            self.output.push(byte);
            self.index += 1;
            return;
        }
        if self.skip_comment(bytes, byte) {
            return;
        }
        self.output.push(byte);
        self.index += 1;
    }

    fn copy_quoted_byte(&mut self, bytes: &[u8], byte: u8) -> bool {
        let Some(active) = self.quote else {
            return false;
        };
        self.output.push(byte);
        if byte == b'\\' && self.index + 1 < bytes.len() {
            self.index += 1;
            self.output.push(bytes[self.index]);
        } else if byte == active {
            self.quote = None;
        }
        self.index += 1;
        true
    }

    fn skip_comment(&mut self, bytes: &[u8], byte: u8) -> bool {
        if byte != b'/' {
            return false;
        }
        if bytes.get(self.index + 1) == Some(&b'/') {
            self.index += 2;
            while self.index < bytes.len() && bytes[self.index] != b'\n' {
                self.index += 1;
            }
            return true;
        }
        if bytes.get(self.index + 1) != Some(&b'*') {
            return false;
        }
        self.index += 2;
        while self.index + 1 < bytes.len()
            && !(bytes[self.index] == b'*' && bytes[self.index + 1] == b'/')
        {
            self.index += 1;
        }
        self.index = (self.index + 2).min(bytes.len());
        true
    }
}

fn scan_literal_imports(
    path: &str,
    language: &str,
    source: &str,
) -> Result<Vec<ImportHit>, ImportAuditError> {
    if !source.contains("import") && !source.contains("require") {
        return Ok(Vec::new());
    }
    let mut scanner = LiteralScanner::new(path, language, source);
    scanner.scan()?;
    Ok(scanner.hits)
}

struct LiteralScanner<'a> {
    path: &'a str,
    language: &'a str,
    source: &'a str,
    bytes: &'a [u8],
    index: usize,
    line: u32,
    previous_significant: u8,
    hits: Vec<ImportHit>,
    seen: BTreeSet<(u32, String, ImportOrigin)>,
}

impl<'a> LiteralScanner<'a> {
    fn new(path: &'a str, language: &'a str, source: &'a str) -> Self {
        Self {
            path,
            language,
            source,
            bytes: source.as_bytes(),
            index: 0,
            line: 1,
            previous_significant: 0,
            hits: Vec::new(),
            seen: BTreeSet::new(),
        }
    }

    fn scan(&mut self) -> Result<(), ImportAuditError> {
        while self.index < self.bytes.len() {
            let byte = self.bytes[self.index];
            if byte == b'/' && self.peek(1) == Some(b'/') {
                self.skip_line_comment();
            } else if byte == b'/' && self.peek(1) == Some(b'*') {
                self.skip_block_comment();
            } else if byte == b'/'
                && (self.previous_significant == 0
                    || expression_precedes(self.previous_significant))
            {
                self.skip_regex();
            } else if matches!(byte, b'"' | b'\'' | b'`') {
                self.scan_literal(byte)?;
                self.previous_significant = byte;
            } else {
                if !byte.is_ascii_whitespace() {
                    self.previous_significant = byte;
                }
                self.advance(1);
            }
        }
        Ok(())
    }

    fn scan_literal(&mut self, quote: u8) -> Result<(), ImportAuditError> {
        self.advance(1);
        let mut segment_start = self.index;
        let mut segment_line = self.line;
        while self.index < self.bytes.len() {
            let byte = self.bytes[self.index];
            if byte == b'\\' {
                self.advance(2);
            } else if byte == quote {
                self.emit_segment(LiteralSegment {
                    start: segment_start,
                    end: self.index,
                    start_line: segment_line,
                    quote,
                })?;
                self.advance(1);
                return Ok(());
            } else if quote == b'`' && byte == b'$' && self.peek(1) == Some(b'{') {
                self.emit_segment(LiteralSegment {
                    start: segment_start,
                    end: self.index,
                    start_line: segment_line,
                    quote,
                })?;
                self.advance(2);
                self.drain_template_expression()?;
                segment_start = self.index;
                segment_line = self.line;
            } else {
                self.advance(1);
            }
        }
        self.emit_segment(LiteralSegment {
            start: segment_start,
            end: self.index,
            start_line: segment_line,
            quote,
        })
    }

    fn drain_template_expression(&mut self) -> Result<(), ImportAuditError> {
        let mut depth = 1_usize;
        while self.index < self.bytes.len() && depth > 0 {
            match self.bytes[self.index] {
                b'{' => {
                    depth = depth.saturating_add(1);
                    self.advance(1);
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    self.advance(1);
                }
                quote @ (b'"' | b'\'' | b'`') => self.scan_literal(quote)?,
                _ => self.advance(1),
            }
        }
        Ok(())
    }

    fn emit_segment(&mut self, input: LiteralSegment) -> Result<(), ImportAuditError> {
        let Some(segment) = self.source.get(input.start..input.end) else {
            return Err(ImportAuditError::SourceUnavailable);
        };
        for pattern in literal_patterns()? {
            for captures in pattern.captures_iter(segment) {
                let Some(full) = captures.get(0) else {
                    continue;
                };
                let Some(specifier) = captures.get(1) else {
                    continue;
                };
                let preceding = &segment[..full.start()];
                let line = input.start_line.saturating_add(
                    u32::try_from(preceding.bytes().filter(|byte| *byte == b'\n').count())
                        .unwrap_or(u32::MAX),
                );
                let key = (line, specifier.as_str().to_owned(), ImportOrigin::Literal);
                if !self.seen.insert(key) {
                    continue;
                }
                if self.hits.len() >= MAXIMUM_IMPORT_HITS {
                    return Err(ImportAuditError::EvidenceLimit);
                }
                let (signature, signature_truncated) = bounded_signature(full.as_str());
                self.hits
                    .try_reserve(1)
                    .map_err(|_| ImportAuditError::EvidenceLimit)?;
                self.hits.push(ImportHit {
                    symbol_id: None,
                    file: self.path.to_owned(),
                    line,
                    specifier: specifier.as_str().to_owned(),
                    signature,
                    signature_truncated,
                    target: ImportAuditTarget::Literal,
                    target_path: None,
                    graph_target_path: None,
                    extension_missing: is_relative_specifier(specifier.as_str())
                        && !has_known_extension(specifier.as_str()),
                    dynamic: false,
                    origin: ImportOrigin::Literal,
                    language: self.language.to_owned(),
                    confidence: None,
                    provenance: Some(if input.quote == b'`' {
                        "literal-template-scan".to_owned()
                    } else {
                        "literal-string-scan".to_owned()
                    }),
                    represented_sites: 1,
                });
            }
        }
        Ok(())
    }

    fn skip_line_comment(&mut self) {
        while self.index < self.bytes.len() && self.bytes[self.index] != b'\n' {
            self.advance(1);
        }
        self.previous_significant = 0;
    }

    fn skip_block_comment(&mut self) {
        self.advance(2);
        while self.index + 1 < self.bytes.len()
            && !(self.bytes[self.index] == b'*' && self.bytes[self.index + 1] == b'/')
        {
            self.advance(1);
        }
        self.advance(2);
        self.previous_significant = 0;
    }

    fn skip_regex(&mut self) {
        self.advance(1);
        while self.index < self.bytes.len() {
            match self.bytes[self.index] {
                b'\\' => self.advance(2),
                b'[' => self.skip_regex_class(),
                b'/' => {
                    self.advance(1);
                    break;
                }
                b'\n' => break,
                _ => self.advance(1),
            }
        }
        while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_lowercase() {
            self.advance(1);
        }
        self.previous_significant = b'/';
    }

    fn skip_regex_class(&mut self) {
        self.advance(1);
        while self.index < self.bytes.len() && self.bytes[self.index] != b']' {
            if self.bytes[self.index] == b'\\' {
                self.advance(2);
            } else {
                self.advance(1);
            }
        }
        self.advance(1);
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.index.saturating_add(offset)).copied()
    }

    fn advance(&mut self, amount: usize) {
        let end = self.index.saturating_add(amount).min(self.bytes.len());
        self.line = self.line.saturating_add(
            u32::try_from(memchr_iter(b'\n', &self.bytes[self.index..end]).count())
                .unwrap_or(u32::MAX),
        );
        self.index = end;
    }
}

fn literal_patterns() -> Result<&'static [Regex], ImportAuditError> {
    static PATTERNS: OnceLock<Result<Vec<Regex>, regex::Error>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            [
                r#"\bimport\b[^'"`\n]*?\bfrom\s*['"]([^'"\s]+)['"]"#,
                r#"\bimport\s*['"]([^'"\s]+)['"]"#,
                r#"\bimport\(\s*['"]([^'"\s]+)['"]\s*\)"#,
                r#"\brequire\(\s*['"]([^'"\s]+)['"]\s*\)"#,
            ]
            .into_iter()
            .map(Regex::new)
            .collect()
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(|_| ImportAuditError::SourceUnavailable)
}

fn expression_precedes(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b'['
            | b'{'
            | b','
            | b';'
            | b':'
            | b'?'
            | b'='
            | b'+'
            | b'-'
            | b'*'
            | b'%'
            | b'&'
            | b'|'
            | b'^'
            | b'~'
            | b'!'
            | b'<'
            | b'>'
    )
}

fn bounded_signature(value: &str) -> (String, bool) {
    let boundary = value.floor_char_boundary(MAXIMUM_SIGNATURE_BYTES);
    (value[..boundary].to_owned(), boundary < value.len())
}

fn is_dynamic_import_signature(signature: &str) -> bool {
    let signature = signature.trim_start();
    signature
        .strip_prefix("import")
        .is_some_and(|suffix| suffix.trim_start().starts_with('('))
        || signature.contains("require(")
}

fn one_based_line(source: &str, start: usize) -> Result<u32, ImportAuditError> {
    let prefix = source
        .as_bytes()
        .get(..start)
        .ok_or(ImportAuditError::SourceChanged)?;
    let newlines = memchr_iter(b'\n', prefix).count();
    u32::try_from(newlines)
        .ok()
        .and_then(|line| line.checked_add(1))
        .ok_or(ImportAuditError::SourceUnavailable)
}

fn javascript_family(language: SourceLanguage) -> bool {
    matches!(
        language,
        SourceLanguage::JavaScript
            | SourceLanguage::Jsx
            | SourceLanguage::TypeScript
            | SourceLanguage::Tsx
    )
}

fn is_fixture_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower
        .split('/')
        .any(|component| matches!(component, "fixture" | "fixtures" | "test-bed" | "test-beds"))
        || lower
            .rsplit_once('.')
            .is_some_and(|(stem, extension)| !extension.is_empty() && stem.ends_with(".fixture"))
}

fn validate_path_filter(value: &str) -> Result<String, ImportAuditError> {
    let value = value.trim_start_matches("./");
    if value.is_empty()
        || value.len() > MAXIMUM_FILTER_BYTES
        || value.starts_with(['/', '\\'])
        || value
            .split(['/', '\\'])
            .any(|component| component == ".." || component.contains('\0'))
    {
        return Err(ImportAuditError::InvalidOptions);
    }
    Ok(value.replace('\\', "/"))
}

fn validate_language_filter(value: &str) -> Result<String, ImportAuditError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ImportAuditError::InvalidOptions);
    }
    Ok(value)
}

fn map_source_error<Error>(_error: Error) -> ImportAuditError {
    ImportAuditError::SourceUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON_URL_FIXTURE: &str =
        r#"{"compilerOptions":{"baseUrl":"https://example.test"}} // comment"#;
    const EXAMPLE_TEST_URL: &str = "https://example.test";

    #[test]
    fn classifier_distinguishes_file_directory_bare_and_missing() {
        let paths = ["src/file.ts", "src/dir/index.ts", "src/typed.ts"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let classify = |specifier| {
            classify_import(ClassifyRequest {
                known_paths: &paths,
                importing_path: "src/main.ts",
                language: SourceLanguage::TypeScript,
                specifier,
                go_module: None,
                c_include_style: None,
                aliases: None,
            })
        };
        assert_eq!(classify("./file").target, ImportAuditTarget::File);
        assert_eq!(classify("./dir").target, ImportAuditTarget::Directory);
        assert_eq!(classify("react").target, ImportAuditTarget::Bare);
        assert_eq!(
            classify("./missing").target,
            ImportAuditTarget::Unresolvable
        );
        assert_eq!(
            classify("./typed.js").target_path.as_deref(),
            Some("src/typed.ts")
        );
    }

    #[test]
    fn literal_scanner_ignores_real_imports_and_finds_embedded_code() {
        let source = r#"
import real from './real';
const generated = `
  import { x } from './inside';
  const y = require("pkg");
`;
const fixture = "await import('./lazy')";
const regex = /require\('fake'\)/;
"#;
        let hits = scan_literal_imports("src/codegen.ts", "typescript", source)
            .unwrap_or_else(|error| panic!("literal scan failed: {error}"));
        let specs = hits
            .iter()
            .map(|hit| hit.specifier.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(specs, BTreeSet::from(["./inside", "./lazy", "pkg"]));
    }

    #[test]
    fn json_comment_stripper_preserves_comment_tokens_inside_strings() {
        let stripped = strip_json_comments(JSON_URL_FIXTURE);
        let parsed: Value = serde_json::from_str(&stripped)
            .unwrap_or_else(|error| panic!("stripped JSON failed: {error}"));
        assert_eq!(parsed["compilerOptions"]["baseUrl"], EXAMPLE_TEST_URL);
    }

    #[test]
    fn fixture_detection_matches_nested_and_filename_conventions() {
        assert!(is_fixture_path("src/fixtures/sample.ts"));
        assert!(is_fixture_path("docs/test-beds/python/example.py"));
        assert!(is_fixture_path("src/sample.fixture.ts"));
        assert!(!is_fixture_path("src/fixtureFactory.ts"));
    }
}
