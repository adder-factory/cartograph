use crate::{BoundedU64Field, optional_bounded_u64};
use cartograph_domain::SourceLanguage;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use thiserror::Error;

const CONFIG_DIRECTORY: &str = ".cartograph";
const CONFIG_FILE: &str = "config.json";
const CONFIG_LOCK_FILE: &str = "config.lock";
const CONFIG_LOCK_WAIT: Duration = Duration::from_millis(250);
const CONFIG_LOCK_RETRY: Duration = Duration::from_millis(5);
const MAXIMUM_CONFIG_BYTES: u64 = 1024 * 1024;
const MAXIMUM_PROJECT_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_PROJECT_AST_DEPTH: usize = 256;
const MINIMUM_PROJECT_AST_DEPTH: usize = 64;
const MAXIMUM_PROJECT_AST_DEPTH: usize = 1_024;
const MAXIMUM_PROJECT_GENERATION_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAXIMUM_PROJECT_SPILL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAXIMUM_PROJECT_SPILL_ROWS: u64 = 10_000_000_000;
const MAXIMUM_PROJECT_EXCLUDES: usize = 4_096;
const MAXIMUM_PROJECT_EXCLUDE_BYTES: usize = 4_096;

/// Secret-safe project policy and atomic configuration I/O failures.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProjectConfigError {
    /// The requested project could not be opened safely.
    #[error("Cartograph project configuration path is unavailable")]
    ProjectUnavailable,
    /// Configuration exceeds the bounded read/write ceiling.
    #[error("Cartograph project configuration is too large")]
    ConfigTooLarge,
    /// Configuration is malformed or violates a required bound.
    #[error("Cartograph project configuration is invalid")]
    InvalidConfig,
    /// A named public numeric setting is out of range.
    #[error(
        "Cartograph project configuration field `{field}` must be between {minimum} and {maximum}"
    )]
    NumericFieldOutOfRange {
        /// Stable public setting name.
        field: &'static str,
        /// Inclusive minimum.
        minimum: u64,
        /// Inclusive maximum.
        maximum: u64,
    },
    /// A bounded private atomic write failed.
    #[error("Cartograph project configuration cannot be written safely")]
    WriteFailed,
    /// Another writer changed configuration or held the bounded lock too long.
    #[error("Cartograph project configuration changed concurrently")]
    ConcurrentModification,
}

/// Non-secret source discovery settings read from the shared project config boundary.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProjectSourceSettings {
    maximum_file_bytes: Option<usize>,
    maximum_ast_depth: Option<usize>,
    maximum_generation_bytes: Option<u64>,
    generation_storage: ProjectGenerationStorage,
    maximum_spill_bytes: Option<u64>,
    maximum_spill_rows: Option<u64>,
    languages: Vec<SourceLanguage>,
    includes: Option<Vec<String>>,
    excludes: Vec<String>,
    features: SourceFeatureFlags,
    duplicate_code_allowlist: Vec<String>,
}

/// Project preference for native generation working-set ownership.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectGenerationStorage {
    /// Select PostgreSQL spill only when the immutable source manifest is large.
    #[default]
    Auto,
    /// Retain the complete canonicalization payload in Rust memory.
    Memory,
    /// Use generation-fenced PostgreSQL spill regardless of manifest size.
    Postgres,
}

impl ProjectGenerationStorage {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "memory" => Some(Self::Memory),
            "postgres" => Some(Self::Postgres),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
enum SourceFeature {
    ExtractDocstrings = 1 << 0,
    TrackCallSites = 1 << 1,
    IndexSubmodules = 1 << 2,
    IndexEmbeddedRepositories = 1 << 3,
    Centrality = 1 << 4,
    Betweenness = 1 << 5,
    Churn = 1 << 6,
    CoChange = 1 << 7,
    Biomarkers = 1 << 8,
    IssueHistory = 1 << 9,
    ConfigReferences = 1 << 10,
    SqlReferences = 1 << 11,
    BuildContextReferences = 1 << 12,
    StringImports = 1 << 13,
    DuplicateCodePartialClones = 1 << 14,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceFeatureFlags(u16);

impl SourceFeatureFlags {
    const DEFAULT_BITS: u16 = (1 << 14) - 1;

    const fn enabled(self, feature: SourceFeature) -> bool {
        self.0 & feature as u16 != 0
    }

    const fn set(&mut self, feature: SourceFeature, enabled: bool) {
        let mask = feature as u16;
        if enabled {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }
}

impl Default for SourceFeatureFlags {
    fn default() -> Self {
        Self(Self::DEFAULT_BITS)
    }
}

impl std::fmt::Debug for ProjectSourceSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("ProjectSourceSettings");
        self.debug_source_fields(&mut debug);
        self.debug_feature_fields(&mut debug);
        debug.finish()
    }
}

impl ProjectSourceSettings {
    fn debug_source_fields(&self, debug: &mut std::fmt::DebugStruct<'_, '_>) {
        debug
            .field("maximum_file_bytes", &self.maximum_file_bytes)
            .field("maximum_ast_depth", &self.maximum_ast_depth())
            .field("maximum_generation_bytes", &self.maximum_generation_bytes)
            .field("generation_storage", &self.generation_storage)
            .field("maximum_spill_bytes", &self.maximum_spill_bytes)
            .field("maximum_spill_rows", &self.maximum_spill_rows)
            .field("configured_languages", &self.languages.len())
            .field(
                "include_patterns",
                &self.includes.as_ref().map_or(0, Vec::len),
            )
            .field("exclude_patterns", &self.excludes.len())
            .field("features", &self.features);
    }

    fn debug_feature_fields(&self, debug: &mut std::fmt::DebugStruct<'_, '_>) {
        debug
            .field("extract_docstrings", &self.extract_docstrings())
            .field("track_call_sites", &self.track_call_sites())
            .field("index_submodules", &self.index_submodules())
            .field(
                "index_embedded_repositories",
                &self.index_embedded_repositories(),
            )
            .field("enable_centrality", &self.enable_centrality())
            .field("enable_betweenness", &self.enable_betweenness())
            .field("enable_churn", &self.enable_churn())
            .field("enable_co_change", &self.enable_co_change())
            .field("enable_biomarkers", &self.enable_biomarkers())
            .field("enable_issue_history", &self.enable_issue_history())
            .field("enable_config_refs", &self.enable_config_refs())
            .field("enable_sql_refs", &self.enable_sql_refs())
            .field(
                "enable_build_context_refs",
                &self.enable_build_context_refs(),
            )
            .field("enable_string_imports", &self.enable_string_imports())
            .field(
                "duplicate_code_partial_clones",
                &self.duplicate_code_partial_clones(),
            )
            .field(
                "duplicate_code_allowlist_patterns",
                &self.duplicate_code_allowlist.len(),
            );
    }
}

impl ProjectSourceSettings {
    #[must_use]
    /// Returns the maximum file bytes.
    pub const fn maximum_file_bytes(&self) -> Option<usize> {
        self.maximum_file_bytes
    }

    /// Effective defensive AST nesting ceiling.
    #[must_use]
    pub const fn maximum_ast_depth(&self) -> usize {
        match self.maximum_ast_depth {
            Some(value) => value,
            None => DEFAULT_PROJECT_AST_DEPTH,
        }
    }

    /// Returns the explicit retained canonical-generation byte ceiling.
    #[must_use]
    pub const fn maximum_generation_bytes(&self) -> Option<u64> {
        self.maximum_generation_bytes
    }

    /// Requested memory/PostgreSQL working-set strategy.
    #[must_use]
    pub const fn generation_storage(&self) -> ProjectGenerationStorage {
        self.generation_storage
    }

    /// Explicit logical PostgreSQL spill byte quota.
    #[must_use]
    pub const fn maximum_spill_bytes(&self) -> Option<u64> {
        self.maximum_spill_bytes
    }

    /// Explicit unordered PostgreSQL spill row quota.
    #[must_use]
    pub const fn maximum_spill_rows(&self) -> Option<u64> {
        self.maximum_spill_rows
    }

    /// Empty means every native language; otherwise this is the canonical
    /// v1-compatible `languages` allow-list.
    #[must_use]
    pub fn languages(&self) -> &[SourceLanguage] {
        &self.languages
    }

    /// `None` selects every native path; `Some` preserves the exact project
    /// include-glob allow-list, including an intentionally empty list.
    #[must_use]
    pub fn includes(&self) -> Option<&[String]> {
        self.includes.as_deref()
    }

    #[must_use]
    /// Returns the excludes.
    pub fn excludes(&self) -> &[String] {
        &self.excludes
    }

    #[must_use]
    /// Returns whether extracted docstrings are retained.
    pub const fn extract_docstrings(&self) -> bool {
        self.features.enabled(SourceFeature::ExtractDocstrings)
    }

    #[must_use]
    /// Returns whether call-site facts are indexed.
    pub const fn track_call_sites(&self) -> bool {
        self.features.enabled(SourceFeature::TrackCallSites)
    }

    #[must_use]
    /// Returns whether nested Git submodules are indexed.
    pub const fn index_submodules(&self) -> bool {
        self.features.enabled(SourceFeature::IndexSubmodules)
    }

    #[must_use]
    /// Returns whether embedded repositories are indexed.
    pub const fn index_embedded_repositories(&self) -> bool {
        self.features
            .enabled(SourceFeature::IndexEmbeddedRepositories)
    }

    /// Whether v1-compatible `PageRank` centrality is derived for each generation.
    #[must_use]
    pub const fn enable_centrality(&self) -> bool {
        self.features.enabled(SourceFeature::Centrality)
    }

    /// Whether bounded sampled Brandes betweenness is derived for each generation.
    #[must_use]
    pub const fn enable_betweenness(&self) -> bool {
        self.features.enabled(SourceFeature::Betweenness)
    }

    #[must_use]
    /// Whether Git churn facts are derived.
    pub const fn enable_churn(&self) -> bool {
        self.features.enabled(SourceFeature::Churn)
    }

    #[must_use]
    /// Whether Git co-change relationships are derived.
    pub const fn enable_co_change(&self) -> bool {
        self.features.enabled(SourceFeature::CoChange)
    }

    #[must_use]
    /// Whether static code-health biomarkers are derived.
    pub const fn enable_biomarkers(&self) -> bool {
        self.features.enabled(SourceFeature::Biomarkers)
    }

    #[must_use]
    /// Whether issue-tagged Git history is derived.
    pub const fn enable_issue_history(&self) -> bool {
        self.features.enabled(SourceFeature::IssueHistory)
    }

    #[must_use]
    /// Whether configuration-key references are extracted.
    pub const fn enable_config_refs(&self) -> bool {
        self.features.enabled(SourceFeature::ConfigReferences)
    }

    #[must_use]
    /// Whether SQL relation references are extracted.
    pub const fn enable_sql_refs(&self) -> bool {
        self.features.enabled(SourceFeature::SqlReferences)
    }

    #[must_use]
    /// Whether build-context file references are extracted.
    pub const fn enable_build_context_refs(&self) -> bool {
        self.features.enabled(SourceFeature::BuildContextReferences)
    }

    #[must_use]
    /// Whether import-shaped string literals contribute module edges.
    pub const fn enable_string_imports(&self) -> bool {
        self.features.enabled(SourceFeature::StringImports)
    }

    /// Whether the wider 0.80 Type-3 clone band is enabled in addition to 0.95.
    #[must_use]
    pub const fn duplicate_code_partial_clones(&self) -> bool {
        self.features
            .enabled(SourceFeature::DuplicateCodePartialClones)
    }

    /// Project-relative path globs exempted from every duplicate-code tier.
    #[must_use]
    pub fn duplicate_code_allowlist(&self) -> &[String] {
        &self.duplicate_code_allowlist
    }
}

/// Read bounded v1-compatible source discovery settings without exposing LLM credentials.
/// # Errors
///
/// Returns an error if config access/JSON shape is invalid or file-size,
/// language, include/exclude, nested-repository, or allow-list settings are malformed.
pub fn load_project_source_settings(
    project_root: &Path,
) -> Result<ProjectSourceSettings, ProjectConfigError> {
    let Some(value) = read_project_config(project_root)? else {
        return Ok(ProjectSourceSettings::default());
    };
    let root = value.as_object().ok_or(ProjectConfigError::InvalidConfig)?;
    parse_project_source_settings(root)
}

fn parse_project_source_settings(
    root: &Map<String, Value>,
) -> Result<ProjectSourceSettings, ProjectConfigError> {
    let maximum_file_bytes = optional_bounded_u64(
        root,
        "maxFileSize",
        BoundedU64Field {
            maximum: u64::try_from(MAXIMUM_PROJECT_SOURCE_BYTES)
                .map_err(|_| ProjectConfigError::InvalidConfig)?,
            invalid: ProjectConfigError::InvalidConfig,
        },
    )?
    .map(usize::try_from)
    .transpose()
    .map_err(|_| ProjectConfigError::InvalidConfig)?;
    let maximum_ast_depth = optional_bounded_u64(
        root,
        "maxAstDepth",
        BoundedU64Field {
            maximum: u64::try_from(MAXIMUM_PROJECT_AST_DEPTH)
                .map_err(|_| ProjectConfigError::InvalidConfig)?,
            invalid: ProjectConfigError::InvalidConfig,
        },
    )?
    .map(usize::try_from)
    .transpose()
    .map_err(|_| ProjectConfigError::InvalidConfig)?;
    if maximum_ast_depth.is_some_and(|value| value < MINIMUM_PROJECT_AST_DEPTH) {
        return Err(ProjectConfigError::InvalidConfig);
    }
    let maximum_generation_bytes =
        optional_named_bounded_u64(root, "maxGenerationBytes", MAXIMUM_PROJECT_GENERATION_BYTES)?;
    let (generation_storage, maximum_spill_bytes, maximum_spill_rows) =
        parse_generation_storage(root)?;
    let languages = root
        .get("languages")
        .map(parse_project_languages)
        .transpose()?
        .unwrap_or_default();
    let mut includes = root.get("include").map(parse_project_globs).transpose()?;
    if root
        .get("version")
        .and_then(Value::as_u64)
        .is_some_and(|version| version < 2)
        && let Some(includes) = includes.as_mut()
    {
        for additive in ["**/*.pyi", "**/*.toml"] {
            if !includes.iter().any(|pattern| pattern == additive) {
                includes.push(additive.to_owned());
            }
        }
    }
    let excludes = root
        .get("exclude")
        .map(parse_project_excludes)
        .transpose()?
        .unwrap_or_default();
    let features = parse_source_features(root)?;
    let duplicate_code_allowlist = root
        .get("duplicateCodeAllowlist")
        .map(parse_project_globs)
        .transpose()?
        .unwrap_or_default();
    Ok(ProjectSourceSettings {
        maximum_file_bytes,
        maximum_ast_depth,
        maximum_generation_bytes,
        generation_storage,
        maximum_spill_bytes,
        maximum_spill_rows,
        languages,
        includes,
        excludes,
        features,
        duplicate_code_allowlist,
    })
}

/// Every boolean source feature with its configuration key and v2 default.
///
/// v2 enables its bounded native implementations by default. A legacy `false`
/// override remains authoritative for projects that prefer v1's indexing-cost
/// tradeoff.
const SOURCE_FEATURE_DEFAULTS: [(SourceFeature, &str, bool); 15] = [
    (SourceFeature::ExtractDocstrings, "extractDocstrings", true),
    (SourceFeature::TrackCallSites, "trackCallSites", true),
    (SourceFeature::IndexSubmodules, "indexSubmodules", true),
    (
        SourceFeature::IndexEmbeddedRepositories,
        "indexEmbeddedRepos",
        true,
    ),
    (SourceFeature::Centrality, "enableCentrality", true),
    (SourceFeature::Betweenness, "enableBetweenness", true),
    (SourceFeature::Churn, "enableChurn", true),
    (SourceFeature::CoChange, "enableCoChange", true),
    (SourceFeature::Biomarkers, "enableBiomarkers", true),
    (SourceFeature::IssueHistory, "enableIssueHistory", true),
    (SourceFeature::ConfigReferences, "enableConfigRefs", true),
    (SourceFeature::SqlReferences, "enableSqlRefs", true),
    (
        SourceFeature::BuildContextReferences,
        "enableBuildContextRefs",
        true,
    ),
    (SourceFeature::StringImports, "enableStringImports", true),
    (
        SourceFeature::DuplicateCodePartialClones,
        "duplicateCodePartialClones",
        false,
    ),
];

fn parse_source_features(
    root: &Map<String, Value>,
) -> Result<SourceFeatureFlags, ProjectConfigError> {
    let mut features = SourceFeatureFlags::default();
    for (feature, key, default) in SOURCE_FEATURE_DEFAULTS {
        features.set(feature, optional_config_bool(root, key)?.unwrap_or(default));
    }
    // An embedded repository is only reachable through submodule indexing, so
    // the two flags are conjunctive rather than independent.
    if !features.enabled(SourceFeature::IndexSubmodules) {
        features.set(SourceFeature::IndexEmbeddedRepositories, false);
    }
    Ok(features)
}

fn parse_generation_storage(
    root: &Map<String, Value>,
) -> Result<(ProjectGenerationStorage, Option<u64>, Option<u64>), ProjectConfigError> {
    let storage = root
        .get("generationStorage")
        .map(|value| {
            value
                .as_str()
                .and_then(ProjectGenerationStorage::parse)
                .ok_or(ProjectConfigError::InvalidConfig)
        })
        .transpose()?
        .unwrap_or_default();
    let bytes = optional_bounded_u64(
        root,
        "maxSpillBytes",
        BoundedU64Field {
            maximum: MAXIMUM_PROJECT_SPILL_BYTES,
            invalid: ProjectConfigError::InvalidConfig,
        },
    )?;
    let rows = optional_bounded_u64(
        root,
        "maxSpillRows",
        BoundedU64Field {
            maximum: MAXIMUM_PROJECT_SPILL_ROWS,
            invalid: ProjectConfigError::InvalidConfig,
        },
    )?;
    Ok((storage, bytes, rows))
}

fn parse_project_languages(value: &Value) -> Result<Vec<SourceLanguage>, ProjectConfigError> {
    let values = value
        .as_array()
        .filter(|values| values.len() <= SourceLanguage::ALL.len())
        .ok_or(ProjectConfigError::InvalidConfig)?;
    let mut languages = Vec::new();
    languages
        .try_reserve_exact(values.len())
        .map_err(|_| ProjectConfigError::InvalidConfig)?;
    for value in values {
        let language = value
            .as_str()
            .and_then(SourceLanguage::from_stable_str)
            .ok_or(ProjectConfigError::InvalidConfig)?;
        if !languages.contains(&language) {
            languages.push(language);
        }
    }
    languages.sort_unstable();
    Ok(languages)
}

fn parse_project_excludes(value: &Value) -> Result<Vec<String>, ProjectConfigError> {
    parse_project_globs(value)
}

fn parse_project_globs(value: &Value) -> Result<Vec<String>, ProjectConfigError> {
    let values = value
        .as_array()
        .filter(|values| values.len() <= MAXIMUM_PROJECT_EXCLUDES)
        .ok_or(ProjectConfigError::InvalidConfig)?;
    let mut excludes = Vec::new();
    excludes
        .try_reserve_exact(values.len())
        .map_err(|_| ProjectConfigError::InvalidConfig)?;
    for value in values {
        let pattern = value
            .as_str()
            .filter(|pattern| {
                !pattern.is_empty()
                    && pattern.len() <= MAXIMUM_PROJECT_EXCLUDE_BYTES
                    && !pattern.contains('\0')
            })
            .ok_or(ProjectConfigError::InvalidConfig)?;
        excludes.push(pattern.to_owned());
    }
    Ok(excludes)
}

fn optional_config_bool(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, ProjectConfigError> {
    object
        .get(key)
        .map(|value| value.as_bool().ok_or(ProjectConfigError::InvalidConfig))
        .transpose()
}

/// Atomically preserve the rest of `.cartograph/config.json` while updating
/// the v1-compatible project-wide source-file ceiling.
/// # Errors
///
/// Returns an error if `max_file_size` is out of range, existing config is
/// unsafe/malformed/oversized, or the private atomic rewrite cannot complete.
pub fn write_project_max_file_size(
    project_root: &Path,
    max_file_size: usize,
) -> Result<(), ProjectConfigError> {
    if !(1..=MAXIMUM_PROJECT_SOURCE_BYTES).contains(&max_file_size) {
        return Err(ProjectConfigError::InvalidConfig);
    }
    update_project_config(project_root, |current| {
        let mut value = current.unwrap_or_else(|| json!({"version": 2}));
        let root = value
            .as_object_mut()
            .ok_or(ProjectConfigError::InvalidConfig)?;
        root.insert("maxFileSize".to_owned(), Value::from(max_file_size));
        Ok((value, ()))
    })
}

fn optional_named_bounded_u64(
    object: &Map<String, Value>,
    key: &'static str,
    maximum: u64,
) -> Result<Option<u64>, ProjectConfigError> {
    object
        .get(key)
        .map(|value| {
            let value = value.as_u64().ok_or(ProjectConfigError::InvalidConfig)?;
            if (1..=maximum).contains(&value) {
                Ok(value)
            } else {
                Err(ProjectConfigError::NumericFieldOutOfRange {
                    field: key,
                    minimum: 1,
                    maximum,
                })
            }
        })
        .transpose()
}

/// Bounded original config bytes and parsed value for compare-before-write operations.
/// Contains secret-bearing configuration and deliberately has no Debug/Serialize implementation.
pub struct ProjectConfigSnapshot {
    /// Parsed project configuration, including unrelated keys.
    pub value: Value,
    /// Exact original bytes, used only for concurrency validation.
    pub bytes: Vec<u8>,
}

struct ConfigWriteLock {
    file: File,
}

impl Drop for ConfigWriteLock {
    fn drop(&mut self) {
        let _ = File::unlock(&self.file);
    }
}

struct ConfigWriteTarget {
    directory: PathBuf,
    path: PathBuf,
    _lock: ConfigWriteLock,
}

/// Read a bounded JSON project configuration without rendering its contents.
/// # Errors
/// Returns an error for unsafe paths, excessive or invalid JSON, failed I/O, or concurrent edits.
pub fn read_project_config(project_root: &Path) -> Result<Option<Value>, ProjectConfigError> {
    read_project_config_snapshot(project_root)
        .map(|snapshot| snapshot.map(|snapshot| snapshot.value))
}

/// Read original bytes and parsed JSON for an optimistic concurrency check.
/// # Errors
/// Returns an error for unsafe paths, excessive or invalid JSON, failed I/O, or concurrent edits.
pub fn read_project_config_snapshot(
    project_root: &Path,
) -> Result<Option<ProjectConfigSnapshot>, ProjectConfigError> {
    let path = config_path(project_root)?;
    let Some(bytes) = read_config_bytes(&path)? else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&bytes).map_err(|_| ProjectConfigError::InvalidConfig)?;
    Ok(Some(ProjectConfigSnapshot { value, bytes }))
}

fn read_config_bytes(path: &Path) -> Result<Option<Vec<u8>>, ProjectConfigError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProjectConfigError::ProjectUnavailable),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ProjectConfigError::ProjectUnavailable);
    }
    if metadata.len() > MAXIMUM_CONFIG_BYTES {
        return Err(ProjectConfigError::ConfigTooLarge);
    }
    let file = File::open(path).map_err(|_| ProjectConfigError::ProjectUnavailable)?;
    let mut bytes = Vec::new();
    file.take(MAXIMUM_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ProjectConfigError::InvalidConfig)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAXIMUM_CONFIG_BYTES {
        return Err(ProjectConfigError::ConfigTooLarge);
    }
    Ok(Some(bytes))
}

/// Apply a typed mutation under the shared bounded lock and atomic rewrite.
/// # Errors
/// Returns an error for unsafe paths, excessive or invalid JSON, failed I/O, or concurrent edits.
pub fn update_project_config<ResultValue, UpdateError: From<ProjectConfigError>>(
    project_root: &Path,
    update: impl FnOnce(Option<Value>) -> Result<(Value, ResultValue), UpdateError>,
) -> Result<ResultValue, UpdateError> {
    update_config_value_after_missing_observed(project_root, update, || {})
}

fn update_config_value_after_missing_observed<
    ResultValue,
    UpdateError: From<ProjectConfigError>,
>(
    project_root: &Path,
    update: impl FnOnce(Option<Value>) -> Result<(Value, ResultValue), UpdateError>,
    observe_missing: impl FnOnce(),
) -> Result<ResultValue, UpdateError> {
    let target = acquire_config_write_target_after_missing_observed(project_root, observe_missing)?;
    let expected = read_config_bytes(&target.path)?;
    let current = expected
        .as_deref()
        .map(|bytes| serde_json::from_slice(bytes).map_err(|_| ProjectConfigError::InvalidConfig))
        .transpose()?;
    let (value, result) = update(current)?;
    if read_config_bytes(&target.path)? != expected {
        return Err(ProjectConfigError::ConcurrentModification.into());
    }
    write_config_value_at(&target.directory, &target.path, &value)?;
    Ok(result)
}

/// Atomically replace configuration only when the original bytes still match.
/// # Errors
/// Returns an error for unsafe paths, excessive or invalid JSON, failed I/O, or concurrent edits.
pub fn write_project_config_if_unchanged(
    project_root: &Path,
    value: &Value,
    expected: &[u8],
) -> Result<(), ProjectConfigError> {
    write_config_value_guarded(project_root, value, Some(expected))
}

fn write_config_value_guarded(
    project_root: &Path,
    value: &Value,
    expected: Option<&[u8]>,
) -> Result<(), ProjectConfigError> {
    let target = acquire_config_write_target(project_root)?;
    if let Some(expected) = expected {
        let current =
            read_config_bytes(&target.path)?.ok_or(ProjectConfigError::ConcurrentModification)?;
        if current != expected {
            return Err(ProjectConfigError::ConcurrentModification);
        }
    }
    write_config_value_at(&target.directory, &target.path, value)
}

fn acquire_config_write_target(
    project_root: &Path,
) -> Result<ConfigWriteTarget, ProjectConfigError> {
    acquire_config_write_target_after_missing_observed(project_root, || {})
}

fn acquire_config_write_target_after_missing_observed(
    project_root: &Path,
    observe_missing: impl FnOnce(),
) -> Result<ConfigWriteTarget, ProjectConfigError> {
    let root = canonical_project_root(project_root)?;
    let directory = root.join(CONFIG_DIRECTORY);
    match fs::symlink_metadata(&directory) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            observe_missing();
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(ProjectConfigError::WriteFailed),
            }
        }
        Err(_) => return Err(ProjectConfigError::ProjectUnavailable),
    }
    if !fs::symlink_metadata(&directory)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        return Err(ProjectConfigError::ProjectUnavailable);
    }
    let lock = acquire_config_write_lock(&directory)?;
    let path = directory.join(CONFIG_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProjectConfigError::ProjectUnavailable);
    }
    Ok(ConfigWriteTarget {
        directory,
        path,
        _lock: lock,
    })
}

fn write_config_value_at(
    directory: &Path,
    path: &Path,
    value: &Value,
) -> Result<(), ProjectConfigError> {
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|_| ProjectConfigError::InvalidConfig)?;
    bytes.push(b'\n');
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAXIMUM_CONFIG_BYTES {
        return Err(ProjectConfigError::ConfigTooLarge);
    }
    let mut temporary =
        NamedTempFile::new_in(directory).map_err(|_| ProjectConfigError::WriteFailed)?;
    #[cfg(unix)]
    set_private_permissions(temporary.as_file())?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| ProjectConfigError::WriteFailed)?;
    temporary
        .persist(path)
        .map_err(|_| ProjectConfigError::WriteFailed)?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ProjectConfigError::WriteFailed)
}

fn acquire_config_write_lock(directory: &Path) -> Result<ConfigWriteLock, ProjectConfigError> {
    let path = directory.join(CONFIG_LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => return Err(ProjectConfigError::ProjectUnavailable),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| ProjectConfigError::WriteFailed)?;
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return Err(ProjectConfigError::ProjectUnavailable);
    }
    #[cfg(unix)]
    set_private_permissions(&file)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(ConfigWriteLock { file }),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < CONFIG_LOCK_WAIT => {
                let remaining = CONFIG_LOCK_WAIT.saturating_sub(started.elapsed());
                thread::sleep(CONFIG_LOCK_RETRY.min(remaining));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ProjectConfigError::ConcurrentModification);
            }
            Err(_) => return Err(ProjectConfigError::WriteFailed),
        }
    }
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> Result<(), ProjectConfigError> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| ProjectConfigError::WriteFailed)
}

fn config_path(project_root: &Path) -> Result<PathBuf, ProjectConfigError> {
    Ok(canonical_project_root(project_root)?
        .join(CONFIG_DIRECTORY)
        .join(CONFIG_FILE))
}

fn canonical_project_root(project_root: &Path) -> Result<PathBuf, ProjectConfigError> {
    let root =
        fs::canonicalize(project_root).map_err(|_| ProjectConfigError::ProjectUnavailable)?;
    if root.is_dir() {
        Ok(root)
    } else {
        Err(ProjectConfigError::ProjectUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, mpsc};
    #[test]
    fn config_updates_wait_for_short_contention_and_read_after_lock() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let directory = root.path().join(CONFIG_DIRECTORY);
        fs::create_dir(&directory)
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let path = directory.join(CONFIG_FILE);
        fs::write(&path, r#"{"version":2,"languages":["rust"]}"#)
            .unwrap_or_else(|error| panic!("config fixture failed: {error}"));
        let held = acquire_config_write_lock(&directory)
            .unwrap_or_else(|error| panic!("fixture lock failed: {error}"));
        let project = root.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            started_tx
                .send(())
                .unwrap_or_else(|error| panic!("writer start signal failed: {error}"));
            write_project_max_file_size(&project, 8 * 1024 * 1024)
        });
        started_rx
            .recv()
            .unwrap_or_else(|error| panic!("writer start wait failed: {error}"));
        thread::sleep(Duration::from_millis(25));
        assert!(
            !writer.is_finished(),
            "a short-lived writer must wait for the config lock"
        );

        fs::write(
            &path,
            r#"{"version":2,"languages":["rust"],"concurrentEdit":true}"#,
        )
        .unwrap_or_else(|error| panic!("concurrent fixture write failed: {error}"));
        drop(held);
        writer
            .join()
            .unwrap_or_else(|_| panic!("config writer panicked"))
            .unwrap_or_else(|error| panic!("config writer failed: {error}"));

        let value = read_project_config(root.path())
            .unwrap_or_else(|error| panic!("config reread failed: {error}"))
            .unwrap_or_else(|| panic!("config missing"));
        assert_eq!(value["languages"], json!(["rust"]));
        assert_eq!(value["concurrentEdit"], true);
        assert_eq!(value["maxFileSize"], 8 * 1024 * 1024);
    }

    #[test]
    fn concurrent_first_config_updates_share_directory_creation_and_preserve_both_fields() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let project = Arc::new(root.path().to_path_buf());
        let missing_barrier = Arc::new(Barrier::new(2));

        let maximum_project = project.clone();
        let maximum_barrier = missing_barrier.clone();
        let maximum = thread::spawn(move || {
            update_config_value_after_missing_observed::<_, ProjectConfigError>(
                &maximum_project,
                |current| {
                    let mut value = current.unwrap_or_else(|| json!({"version": 2}));
                    value
                        .as_object_mut()
                        .ok_or(ProjectConfigError::InvalidConfig)?
                        .insert("maxFileSize".to_owned(), Value::from(8 * 1024 * 1024));
                    Ok((value, ()))
                },
                || {
                    maximum_barrier.wait();
                },
            )
        });
        let language_project = project.clone();
        let language_barrier = missing_barrier.clone();
        let language = thread::spawn(move || {
            update_config_value_after_missing_observed::<_, ProjectConfigError>(
                &language_project,
                |current| {
                    let mut value = current.unwrap_or_else(|| json!({"version": 2}));
                    value
                        .as_object_mut()
                        .ok_or(ProjectConfigError::InvalidConfig)?
                        .insert("languages".to_owned(), json!(["rust"]));
                    Ok((value, ()))
                },
                || {
                    language_barrier.wait();
                },
            )
        });

        maximum
            .join()
            .unwrap_or_else(|_| panic!("maximum writer panicked"))
            .unwrap_or_else(|error| panic!("maximum writer failed: {error}"));
        language
            .join()
            .unwrap_or_else(|_| panic!("language writer panicked"))
            .unwrap_or_else(|error| panic!("language writer failed: {error}"));

        let value = read_project_config(&project)
            .unwrap_or_else(|error| panic!("config reread failed: {error}"))
            .unwrap_or_else(|| panic!("config missing"));
        assert_eq!(value["maxFileSize"], 8 * 1024 * 1024);
        assert_eq!(value["languages"], json!(["rust"]));
    }

    #[test]
    fn config_updates_fail_after_bounded_lock_contention() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let directory = root.path().join(CONFIG_DIRECTORY);
        fs::create_dir(&directory)
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let _held = acquire_config_write_lock(&directory)
            .unwrap_or_else(|error| panic!("fixture lock failed: {error}"));

        let started = std::time::Instant::now();
        let result = write_project_max_file_size(root.path(), 8 * 1024 * 1024);
        let elapsed = started.elapsed();

        assert_eq!(result, Err(ProjectConfigError::ConcurrentModification));
        assert!(
            elapsed >= Duration::from_millis(100),
            "config contention failed immediately after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "config contention was not bounded: {elapsed:?}"
        );
    }

    #[test]
    fn config_updates_reject_an_uncooperative_edit_during_mutation() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let directory = root.path().join(CONFIG_DIRECTORY);
        fs::create_dir(&directory)
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        let path = directory.join(CONFIG_FILE);
        fs::write(&path, r#"{"version":2,"languages":["rust"]}"#)
            .unwrap_or_else(|error| panic!("config fixture failed: {error}"));
        let observed_path = path.clone();

        let result = update_project_config(root.path(), |current| {
            fs::write(
                &observed_path,
                r#"{"version":2,"languages":["rust"],"externalEdit":true}"#,
            )
            .unwrap_or_else(|error| panic!("external edit failed: {error}"));
            let mut value = current.ok_or(ProjectConfigError::InvalidConfig)?;
            value
                .as_object_mut()
                .ok_or(ProjectConfigError::InvalidConfig)?
                .insert("maxFileSize".to_owned(), Value::from(8 * 1024 * 1024));
            Ok((value, ()))
        });

        assert_eq!(result, Err(ProjectConfigError::ConcurrentModification));
        let value = read_project_config(root.path())
            .unwrap_or_else(|error| panic!("config reread failed: {error}"))
            .unwrap_or_else(|| panic!("config missing"));
        assert_eq!(value["externalEdit"], true);
        assert!(value.get("maxFileSize").is_none());
    }

    #[test]
    fn source_settings_are_bounded_v1_compatible_and_do_not_render_patterns() {
        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        fs::create_dir(root.path().join(CONFIG_DIRECTORY))
            .unwrap_or_else(|error| panic!("config directory failed: {error}"));
        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"maxFileSize":4096,"maxAstDepth":512,"maxGenerationBytes":8589934592,"generationStorage":"postgres","maxSpillBytes":137438953472,"maxSpillRows":1000000000,"languages":["typescript","rust","rust"],"exclude":["private/**"],"extractDocstrings":false,"trackCallSites":false,"indexSubmodules":false,"indexEmbeddedRepos":true,"enableCentrality":false,"enableBetweenness":false,"enableChurn":false,"enableCoChange":false,"enableBiomarkers":false,"enableIssueHistory":false,"enableConfigRefs":false,"enableSqlRefs":false,"enableBuildContextRefs":false,"enableStringImports":false,"duplicateCodePartialClones":true,"duplicateCodeAllowlist":["generated/**","vendor-copy/**"],"llm":{"apiKey":"do-not-render"}}"#,
        )
        .unwrap_or_else(|error| panic!("source config fixture failed: {error}"));
        let settings = load_project_source_settings(root.path())
            .unwrap_or_else(|error| panic!("source settings failed: {error}"));
        assert_eq!(settings.maximum_file_bytes(), Some(4096));
        assert_eq!(settings.maximum_ast_depth(), 512);
        assert_eq!(settings.maximum_generation_bytes(), Some(8_589_934_592));
        assert_eq!(
            settings.generation_storage(),
            ProjectGenerationStorage::Postgres
        );
        assert_eq!(settings.maximum_spill_bytes(), Some(137_438_953_472));
        assert_eq!(settings.maximum_spill_rows(), Some(1_000_000_000));
        assert_eq!(
            settings.languages(),
            [SourceLanguage::Rust, SourceLanguage::TypeScript]
        );
        assert_eq!(settings.excludes(), ["private/**"]);
        assert!(!settings.extract_docstrings());
        assert!(!settings.track_call_sites());
        assert!(!settings.index_submodules());
        assert!(!settings.index_embedded_repositories());
        assert!(!settings.enable_centrality());
        assert!(!settings.enable_betweenness());
        assert!(!settings.enable_churn());
        assert!(!settings.enable_co_change());
        assert!(!settings.enable_biomarkers());
        assert!(!settings.enable_issue_history());
        assert!(!settings.enable_config_refs());
        assert!(!settings.enable_sql_refs());
        assert!(!settings.enable_build_context_refs());
        assert!(!settings.enable_string_imports());
        assert!(settings.duplicate_code_partial_clones());
        assert_eq!(
            settings.duplicate_code_allowlist(),
            ["generated/**", "vendor-copy/**"]
        );
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("generated"));
        assert!(!rendered.contains("do-not-render"));

        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"exclude":"not-an-array"}"#,
        )
        .unwrap_or_else(|error| panic!("invalid source config fixture failed: {error}"));
        assert_eq!(
            load_project_source_settings(root.path()),
            Err(ProjectConfigError::InvalidConfig)
        );

        for invalid_depth in [63, 1025] {
            fs::write(
                root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
                format!(r#"{{"maxAstDepth":{invalid_depth}}}"#),
            )
            .unwrap_or_else(|error| panic!("invalid AST depth config fixture failed: {error}"));
            assert_eq!(
                load_project_source_settings(root.path()),
                Err(ProjectConfigError::InvalidConfig)
            );
        }

        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"generationStorage":"unbounded"}"#,
        )
        .unwrap_or_else(|error| panic!("invalid spill config fixture failed: {error}"));
        assert_eq!(
            load_project_source_settings(root.path()),
            Err(ProjectConfigError::InvalidConfig)
        );

        fs::write(
            root.path().join(CONFIG_DIRECTORY).join(CONFIG_FILE),
            r#"{"maxGenerationBytes":8589934593}"#,
        )
        .unwrap_or_else(|error| panic!("invalid generation config fixture failed: {error}"));
        assert_eq!(
            load_project_source_settings(root.path()),
            Err(ProjectConfigError::NumericFieldOutOfRange {
                field: "maxGenerationBytes",
                minimum: 1,
                maximum: 8_589_934_592,
            })
        );
        let error = load_project_source_settings(root.path())
            .err()
            .unwrap_or_else(|| panic!("out-of-range generation config unexpectedly loaded"));
        assert_eq!(
            error.to_string(),
            "Cartograph project configuration field `maxGenerationBytes` must be between 1 and 8589934592"
        );
    }
}
