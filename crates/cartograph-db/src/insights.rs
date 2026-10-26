use std::{collections::BTreeMap, time::Duration};

mod native;

use cartograph_domain::{ContentDigest, GenerationId, NormalizedPath, ProjectId, SymbolId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default native storage statement deadline for bounded insight reads.
pub const DEFAULT_INSIGHT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INSIGHT_LIMIT: u16 = 500;
const MAX_GROUPED_PATH_KEYS: usize = 52;
const MAX_GROUPED_PATHS_PER_KEY: usize = 500;
const MAX_GROUPED_PATHS: usize = 10_400;
const MAX_GROUP_KEY_BYTES: usize = 128;

/// One structurally unreferenced implementation candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeadCodeCandidate {
    symbol_id: String,
    path: String,
    language: String,
    symbol_kind: String,
    qualified_name: String,
    start_line: u32,
    incoming_edges: u64,
    outgoing_edges: u64,
    safe_code: String,
    interface_dispatch_risk: bool,
    reason: &'static str,
}

impl DeadCodeCandidate {
    #[must_use]
    /// Returns the symbol ID.
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the language.
    pub fn language(&self) -> &str {
        &self.language
    }

    #[must_use]
    /// Returns the symbol kind.
    pub fn symbol_kind(&self) -> &str {
        &self.symbol_kind
    }

    #[must_use]
    /// Returns the qualified name.
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    #[must_use]
    /// Returns the start line.
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    #[must_use]
    /// Returns the outgoing edges.
    pub const fn outgoing_edges(&self) -> u64 {
        self.outgoing_edges
    }

    #[must_use]
    /// Returns the safe code.
    pub fn safe_code(&self) -> &str {
        &self.safe_code
    }

    #[must_use]
    /// Returns the interface dispatch risk.
    pub const fn interface_dispatch_risk(&self) -> bool {
        self.interface_dispatch_risk
    }
}

/// Database-side orphan filters applied before the candidate cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadCodeQuery {
    limit: u16,
    include_tests: bool,
    exclude_fixtures: bool,
}

impl DeadCodeQuery {
    /// Creates a validated dead code query.
    ///
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the insight row maximum.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        validate_limit(limit)?;
        Ok(Self {
            limit,
            include_tests: false,
            exclude_fixtures: true,
        })
    }

    #[must_use]
    /// Sets the include tests and returns the updated value.
    pub const fn with_include_tests(mut self, include: bool) -> Self {
        self.include_tests = include;
        self
    }

    #[must_use]
    /// Sets the exclude fixtures and returns the updated value.
    pub const fn with_exclude_fixtures(mut self, exclude: bool) -> Self {
        self.exclude_fixtures = exclude;
        self
    }
}

/// One file ranked by current-generation structural pressure.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralHotspot {
    path: String,
    language: String,
    symbols: u64,
    incoming_edges: u64,
    outgoing_edges: u64,
    unresolved_references: u64,
    routes: u64,
    structural_risk: u64,
    commit_count: u64,
    author_count: u64,
    insertions: u64,
    deletions: u64,
    last_touched_at: Option<String>,
    last_touched_unix_seconds: Option<u64>,
    history_available: bool,
    composite_risk: u64,
    centrality: f64,
    structural_pressure: f64,
    churn_score: f64,
    risk_score: f64,
    maintenance_score: f64,
    brittle_score: f64,
    external_dependents: u64,
}

/// Maintenance lens applied before the database result cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StructuralHotspotCategory {
    #[default]
    /// Composite structural and change risk.
    Risk,
    /// High maintenance activity or cost.
    Maintenance,
    /// Structural fragility with elevated dependent impact.
    Brittle,
}

/// Stable database-side ordering for risk hotspots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StructuralHotspotSort {
    #[default]
    /// Order by composite risk score.
    Risk,
    /// Order by graph centrality.
    Centrality,
    /// Order by Git churn.
    Churn,
}

/// Validated hotspot filters that all execute before LIMIT.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StructuralHotspotQuery {
    category: StructuralHotspotCategory,
    sort: StructuralHotspotSort,
    minimum_commits: u64,
    minimum_centrality: f64,
    recency_days: Option<u32>,
    limit: u16,
}

impl StructuralHotspotQuery {
    /// Creates a validated structural hotspot query.
    ///
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the hotspot row maximum.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        validate_limit(limit)?;
        Ok(Self {
            category: StructuralHotspotCategory::Risk,
            sort: StructuralHotspotSort::Risk,
            minimum_commits: 0,
            minimum_centrality: 0.0,
            recency_days: None,
            limit,
        })
    }

    #[must_use]
    /// Sets the category and returns the updated value.
    pub const fn with_category(mut self, category: StructuralHotspotCategory) -> Self {
        self.category = category;
        self
    }

    #[must_use]
    /// Sets the sort and returns the updated value.
    pub const fn with_sort(mut self, sort: StructuralHotspotSort) -> Self {
        self.sort = sort;
        self
    }

    /// Sets the minimum commits and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if `commits` cannot be represented by native storage's
    /// signed `bigint` comparison.
    pub fn with_minimum_commits(mut self, commits: u64) -> Result<Self, StorageError> {
        if commits > i64::MAX as u64 {
            return Err(StorageError::InvalidInput {
                field: "hotspot_minimum_commits",
            });
        }
        self.minimum_commits = commits;
        Ok(self)
    }

    /// Sets the minimum centrality and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if `minimum` is non-finite or outside the inclusive
    /// normalized centrality range from zero to one.
    pub fn with_minimum_centrality(mut self, minimum: f64) -> Result<Self, StorageError> {
        if !minimum.is_finite() || !(0.0..=1.0).contains(&minimum) {
            return Err(StorageError::InvalidInput {
                field: "hotspot_minimum_centrality",
            });
        }
        self.minimum_centrality = minimum;
        Ok(self)
    }

    /// Sets the recency days and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the optional recency window is zero days.
    pub fn with_recency_days(mut self, days: Option<u32>) -> Result<Self, StorageError> {
        if days == Some(0) {
            return Err(StorageError::InvalidInput {
                field: "hotspot_recency_days",
            });
        }
        self.recency_days = days;
        Ok(self)
    }
}

impl StructuralHotspot {
    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the incoming edges.
    pub const fn incoming_edges(&self) -> u64 {
        self.incoming_edges
    }

    #[must_use]
    /// Returns the structural risk.
    pub const fn structural_risk(&self) -> u64 {
        self.structural_risk
    }

    #[must_use]
    /// Returns the commit count.
    pub const fn commit_count(&self) -> u64 {
        self.commit_count
    }

    #[must_use]
    /// Returns the last touched unix seconds.
    pub const fn last_touched_unix_seconds(&self) -> Option<u64> {
        self.last_touched_unix_seconds
    }

    #[must_use]
    /// Returns the composite risk.
    pub const fn composite_risk(&self) -> u64 {
        self.composite_risk
    }

    #[must_use]
    /// Returns the centrality.
    pub const fn centrality(&self) -> f64 {
        self.centrality
    }

    #[must_use]
    /// Returns the external dependents.
    pub const fn external_dependents(&self) -> u64 {
        self.external_dependents
    }

    #[must_use]
    /// Returns the risk score.
    pub const fn risk_score(&self) -> f64 {
        self.risk_score
    }
}

/// One caller-defined group of current project paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupedPathInput {
    key: String,
    paths: Vec<NormalizedPath>,
}

impl GroupedPathInput {
    /// Build one bounded, de-duplicated path group.
    /// # Errors
    ///
    /// Returns an error if the key is empty/oversized/NUL-containing or the
    /// de-duplicated path set is empty or exceeds its group cap.
    pub fn new(key: &str, paths: Vec<NormalizedPath>) -> Result<Self, StorageError> {
        if key.is_empty() || key.len() > MAX_GROUP_KEY_BYTES || key.contains('\0') {
            return Err(StorageError::InvalidInput {
                field: "grouped_path_key",
            });
        }
        let paths = paths.into_iter().collect::<std::collections::BTreeSet<_>>();
        if paths.is_empty() || paths.len() > MAX_GROUPED_PATHS_PER_KEY {
            return Err(StorageError::InvalidInput {
                field: "grouped_paths",
            });
        }
        Ok(Self {
            key: key.to_owned(),
            paths: paths.into_iter().collect(),
        })
    }
}

/// Validated grouped-path symbol lookup with one independent cap per group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupedSymbolQuery {
    groups: Vec<GroupedPathInput>,
    excluded_symbol_id: Option<SymbolId>,
    per_group_limit: u16,
}

impl GroupedSymbolQuery {
    /// Build a grouped current-symbol query executed in one native storage statement.
    /// # Errors
    ///
    /// Returns an error if groups are empty/too numerous, keys repeat, total
    /// path arithmetic/caps fail, or `per_group_limit` is invalid.
    pub fn new(groups: Vec<GroupedPathInput>, per_group_limit: u16) -> Result<Self, StorageError> {
        validate_limit(per_group_limit)?;
        let total_paths = groups
            .iter()
            .try_fold(0_usize, |total, group| total.checked_add(group.paths.len()))
            .ok_or(StorageError::InvalidInput {
                field: "grouped_paths",
            })?;
        let unique_keys = groups
            .iter()
            .map(|group| group.key.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        if groups.is_empty()
            || groups.len() > MAX_GROUPED_PATH_KEYS
            || unique_keys.len() != groups.len()
            || total_paths > MAX_GROUPED_PATHS
        {
            return Err(StorageError::InvalidInput {
                field: "grouped_paths",
            });
        }
        Ok(Self {
            groups,
            excluded_symbol_id: None,
            per_group_limit,
        })
    }

    /// Exclude the blamed/root symbol itself from every peer group.
    #[must_use]
    pub fn excluding_symbol(mut self, symbol_id: SymbolId) -> Self {
        self.excluded_symbol_id = Some(symbol_id);
        self
    }
}

/// One current-generation symbol attributed through a caller path group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedSymbolPeer {
    symbol_id: String,
    path: String,
    language: String,
    symbol_kind: String,
    qualified_name: String,
    start_line: u32,
    end_line: u32,
}

/// Exact pre-limit count and bounded peers for one input group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedSymbolPeers {
    key: String,
    total: u64,
    peers: Vec<GroupedSymbolPeer>,
    truncated: bool,
}

impl GroupedSymbolPeers {
    /// Caller-supplied stable group key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// One current-generation import with exact resolution evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportInsight {
    path: String,
    language: String,
    owner_symbol_id: Option<String>,
    module_specifier: String,
    target_symbol_id: Option<String>,
    target_path: Option<String>,
    start_byte: u64,
    end_byte: u64,
    confidence: f32,
    provenance: String,
    site_count: u64,
    source_file_symbol_id: String,
}

impl ImportInsight {
    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the language.
    pub fn language(&self) -> &str {
        &self.language
    }

    #[must_use]
    /// Returns the owner symbol ID.
    pub fn owner_symbol_id(&self) -> Option<&str> {
        self.owner_symbol_id.as_deref()
    }

    #[must_use]
    /// Returns the module specifier.
    pub fn module_specifier(&self) -> &str {
        &self.module_specifier
    }

    #[must_use]
    /// Returns the target path.
    pub fn target_path(&self) -> Option<&str> {
        self.target_path.as_deref()
    }

    #[must_use]
    /// Returns the target symbol ID.
    pub fn target_symbol_id(&self) -> Option<&str> {
        self.target_symbol_id.as_deref()
    }

    #[must_use]
    /// Returns the evidence symbol ID.
    pub fn evidence_symbol_id(&self) -> &str {
        self.owner_symbol_id
            .as_deref()
            .unwrap_or(&self.source_file_symbol_id)
    }

    #[must_use]
    /// Returns the start byte.
    pub const fn start_byte(&self) -> u64 {
        self.start_byte
    }

    #[must_use]
    /// Returns the end byte.
    pub const fn end_byte(&self) -> u64 {
        self.end_byte
    }

    #[must_use]
    /// Returns the confidence.
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    /// Returns the provenance.
    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    #[must_use]
    /// Returns the site count.
    pub const fn site_count(&self) -> u64 {
        self.site_count
    }
}

/// One JavaScript-family external import used by package-manifest auditing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalImportRecord {
    path: String,
    language: String,
    module_specifier: String,
    site_count: u64,
}

impl ExternalImportRecord {
    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the module specifier.
    pub fn module_specifier(&self) -> &str {
        &self.module_specifier
    }

    #[must_use]
    /// Returns the site count.
    pub const fn site_count(&self) -> u64 {
        self.site_count
    }
}

/// Resolution coverage for one current-generation language/reference family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyCoverageRow {
    language: String,
    reference_kind: String,
    references: u64,
    resolved: u64,
    unresolved: u64,
    represented_sites: u64,
}

/// One exact reference site participating in a rename plan.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameReferenceSite {
    path: String,
    owner_symbol_id: Option<String>,
    start_byte: u64,
    end_byte: u64,
    reference_kind: String,
    confidence: f32,
    provenance: String,
}

impl RenameReferenceSite {
    #[must_use]
    /// Returns the path.
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    /// Returns the owner symbol ID.
    pub fn owner_symbol_id(&self) -> Option<&str> {
        self.owner_symbol_id.as_deref()
    }

    #[must_use]
    /// Returns the start byte.
    pub const fn start_byte(&self) -> u64 {
        self.start_byte
    }

    #[must_use]
    /// Returns the end byte.
    pub const fn end_byte(&self) -> u64 {
        self.end_byte
    }

    #[must_use]
    /// Returns the reference kind.
    pub fn reference_kind(&self) -> &str {
        &self.reference_kind
    }

    #[must_use]
    /// Returns the confidence.
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    /// Returns the provenance.
    pub fn provenance(&self) -> &str {
        &self.provenance
    }
}

/// One graph-derived test-coverage row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralCoverageRow {
    symbol_id: String,
    path: String,
    qualified_name: String,
    symbol_kind: String,
    direct_test_files: u64,
    incoming_edges: u64,
}

/// One test file reached from changed files through the current dependency graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTestImpact {
    path: String,
    distance: u8,
    reason: &'static str,
}

impl FileTestImpact {
    /// Canonical project-relative test path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Minimum cross-symbol graph distance from any input file.
    #[must_use]
    pub const fn distance(&self) -> u8 {
        self.distance
    }
}

/// Native changed-file to affected-test traversal with bounded counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTestImpactResult {
    generation_id: GenerationId,
    matched_inputs: Vec<String>,
    dependent_file_count: u64,
    affected_test_file_count: u64,
    reached_barrel_count: u64,
    reached_barrels: Vec<String>,
    nodes_truncated: bool,
    barrels_truncated: bool,
    tests: Vec<FileTestImpact>,
    tests_truncated: bool,
}

/// Bounded reverse dependency traversal from exact current-generation files.
pub struct FileTestImpactQuery<'a> {
    /// Stable project ID for this record.
    pub project_id: &'a ProjectId,
    /// Project-relative paths for this record.
    pub paths: &'a [NormalizedPath],
    /// Maximum reverse-dependency traversal depth.
    pub max_depth: u8,
    /// Maximum number of distinct symbols admitted to the bounded traversal result.
    pub max_nodes: u16,
    /// Maximum number of entries returned by this bounded request.
    pub limit: u16,
    /// Optional test path regex, when available.
    pub test_path_regex: Option<&'a str>,
}

/// One current-generation path and exact indexed source digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedFileFingerprint {
    path: NormalizedPath,
    content_hash: ContentDigest,
}

impl IndexedFileFingerprint {
    /// Canonical project-relative path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Digest of the exact source bytes indexed in the current generation.
    #[must_use]
    pub const fn content_hash(&self) -> &ContentDigest {
        &self.content_hash
    }
}

impl FileTestImpactResult {
    /// Immutable generation traversed by the query.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Canonical input paths that existed in this generation.
    #[must_use]
    pub fn matched_inputs(&self) -> &[String] {
        &self.matched_inputs
    }

    /// Distinct non-input files reached within the admitted node budget.
    #[must_use]
    pub const fn dependent_file_count(&self) -> u64 {
        self.dependent_file_count
    }

    /// Affected test files found within the admitted node budget before output limiting.
    #[must_use]
    pub const fn affected_test_file_count(&self) -> u64 {
        self.affected_test_file_count
    }

    /// Whether file counts cover every node within the requested depth.
    #[must_use]
    pub const fn counts_complete(&self) -> bool {
        !self.nodes_truncated
    }

    /// Whether more distinct symbols existed than the requested traversal node budget.
    #[must_use]
    pub const fn nodes_truncated(&self) -> bool {
        self.nodes_truncated
    }

    /// Complete number of public API barrels reached after the input layer.
    #[must_use]
    pub const fn reached_barrel_count(&self) -> u64 {
        self.reached_barrel_count
    }

    /// Bounded canonical paths for reached public API barrels.
    #[must_use]
    pub fn reached_barrels(&self) -> &[String] {
        &self.reached_barrels
    }

    /// Ordered affected test files retained under the response limit.
    #[must_use]
    pub fn tests(&self) -> &[FileTestImpact] {
        &self.tests
    }
}

/// One deterministic code-health signal derived from current graph facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralFinding {
    symbol_id: String,
    path: String,
    qualified_name: String,
    finding: String,
    severity: String,
    start_line: u32,
    end_line: u32,
    metric_name: String,
    metric: f64,
    degree_centrality: f64,
    outgoing_edges: u64,
    unresolved_references: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<Value>,
}

/// Minimum severity accepted by a structural-finding query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StructuralFindingSeverity {
    /// Include informational, warning, and error findings.
    Info,
    /// Include warning and error findings.
    #[default]
    Warning,
    /// Include only error findings.
    Error,
}

impl StructuralFindingSeverity {
    const fn rank(self) -> i16 {
        match self {
            Self::Info => 1,
            Self::Warning => 2,
            Self::Error => 3,
        }
    }
}

/// Fully bounded database-side filters for ranked structural findings.
#[derive(Clone, Debug, PartialEq)]
pub struct StructuralFindingQuery {
    limit: u16,
    finding: Option<String>,
    minimum_severity: StructuralFindingSeverity,
    minimum_metric: Option<f64>,
    maximum_metric: Option<f64>,
    minimum_centrality: Option<f64>,
    excluded_path_prefix: Option<String>,
    path_prefix: Option<String>,
    symbol_ids: Vec<SymbolId>,
    exclude_fixtures: bool,
}

/// Per-detector bounds for an audit that must not let one noisy rule starve others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuralFindingGroupQuery {
    findings: Vec<String>,
    minimum_severity: StructuralFindingSeverity,
    per_finding_limit: u16,
    exclude_fixtures: bool,
    path_prefix: Option<String>,
}

impl StructuralFindingGroupQuery {
    /// Build a query for up to 64 detector names with an independent row cap.
    /// # Errors
    ///
    /// Returns an error if the detector list is empty/over 64, a name is
    /// invalid, or `per_finding_limit` is outside the insight bounds.
    pub fn new(findings: Vec<String>, per_finding_limit: u16) -> Result<Self, StorageError> {
        if findings.is_empty() || findings.len() > 64 {
            return Err(StorageError::InvalidInput {
                field: "finding_group",
            });
        }
        validate_limit(per_finding_limit)?;
        let mut unique = Vec::with_capacity(findings.len());
        for finding in findings {
            validate_finding_text(Some(&finding), "finding_group")?;
            if !unique.contains(&finding) {
                unique.push(finding);
            }
        }
        Ok(Self {
            findings: unique,
            minimum_severity: StructuralFindingSeverity::default(),
            per_finding_limit,
            exclude_fixtures: false,
            path_prefix: None,
        })
    }

    /// Select the minimum accepted severity.
    #[must_use]
    pub const fn with_minimum_severity(mut self, severity: StructuralFindingSeverity) -> Self {
        self.minimum_severity = severity;
        self
    }

    /// Drop conventional test, fixture, mock, example, script, and benchmark paths.
    #[must_use]
    pub const fn with_exclude_fixtures(mut self, exclude: bool) -> Self {
        self.exclude_fixtures = exclude;
        self
    }

    /// Restrict detector findings and pre-limit counts to a canonical path prefix.
    /// # Errors
    /// Rejects an invalid project-relative path prefix.
    pub fn with_path_prefix(mut self, prefix: Option<&str>) -> Result<Self, StorageError> {
        validate_finding_path(prefix, "finding_path_prefix")?;
        self.path_prefix = prefix
            .map(|prefix| {
                NormalizedPath::parse(prefix.trim_end_matches('/'))
                    .map(|path| path.as_str().to_owned())
                    .map_err(|_| StorageError::InvalidInput {
                        field: "finding_path_prefix",
                    })
            })
            .transpose()?;
        Ok(self)
    }
}

/// Independently bounded detector groups plus complete pre-limit counts.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralFindingGroup {
    findings: Vec<StructuralFinding>,
    counts: BTreeMap<String, u64>,
    computation: Option<StructuralFindingComputation>,
}

impl StructuralFindingGroup {
    /// Consume the group into its deterministically ordered findings.
    #[must_use]
    pub fn into_findings(self) -> Vec<StructuralFinding> {
        self.findings
    }

    /// Complete count for each requested detector before its independent cap.
    #[must_use]
    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.counts
    }

    /// Complete computation provenance read in the same snapshot as these rows.
    /// Absent when no complete relation exists for the snapshot's inputs.
    #[must_use]
    pub const fn computation(&self) -> Option<&StructuralFindingComputation> {
        self.computation.as_ref()
    }
}

/// One complete detector relation and its exact generation/input identity.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralFindingComputation {
    generation_id: GenerationId,
    input_fingerprint: ContentDigest,
    stats: StructuralFindingStats,
}

impl StructuralFindingComputation {
    /// Generation whose source and auxiliary inputs produced this relation.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Fingerprint including the generation and auxiliary detector inputs.
    #[must_use]
    pub const fn input_fingerprint(&self) -> &ContentDigest {
        &self.input_fingerprint
    }

    /// Complete unfiltered project totals from this same relation.
    #[must_use]
    pub const fn stats(&self) -> &StructuralFindingStats {
        &self.stats
    }
}

impl StructuralFindingQuery {
    /// Build a warning-or-worse ranked query with an explicit result bound.
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the structural-finding cap.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        validate_limit(limit)?;
        Ok(Self {
            limit,
            finding: None,
            minimum_severity: StructuralFindingSeverity::default(),
            minimum_metric: None,
            maximum_metric: None,
            minimum_centrality: None,
            excluded_path_prefix: None,
            path_prefix: None,
            symbol_ids: Vec::new(),
            exclude_fixtures: false,
        })
    }

    /// Restrict results to one detector name.
    /// # Errors
    ///
    /// Returns an error if the optional detector name is empty, oversized, or
    /// contains a NUL byte.
    pub fn with_finding(mut self, finding: Option<&str>) -> Result<Self, StorageError> {
        validate_finding_text(finding, "finding")?;
        self.finding = finding.map(ToOwned::to_owned);
        Ok(self)
    }

    /// Select the minimum accepted severity.
    #[must_use]
    pub const fn with_minimum_severity(mut self, severity: StructuralFindingSeverity) -> Self {
        self.minimum_severity = severity;
        self
    }

    /// Restrict detector metrics to an inclusive range.
    /// # Errors
    ///
    /// Returns an error if either bound is non-finite or the minimum is greater
    /// than the maximum.
    pub fn with_metric_bounds(
        mut self,
        minimum: Option<f64>,
        maximum: Option<f64>,
    ) -> Result<Self, StorageError> {
        if minimum.is_some_and(|value| !value.is_finite())
            || maximum.is_some_and(|value| !value.is_finite())
            || minimum.zip(maximum).is_some_and(|(low, high)| low > high)
        {
            return Err(StorageError::InvalidInput {
                field: "finding_metric",
            });
        }
        self.minimum_metric = minimum;
        self.maximum_metric = maximum;
        Ok(self)
    }

    /// Restrict results by normalized incoming-edge-site degree.
    /// # Errors
    ///
    /// Returns an error if the optional minimum is non-finite or outside the
    /// inclusive normalized centrality range.
    pub fn with_minimum_centrality(mut self, minimum: Option<f64>) -> Result<Self, StorageError> {
        if minimum.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
            return Err(StorageError::InvalidInput {
                field: "finding_centrality",
            });
        }
        self.minimum_centrality = minimum;
        Ok(self)
    }

    /// Exclude one literal normalized-path prefix.
    /// # Errors
    ///
    /// Returns an error if the optional excluded prefix is empty, absolute,
    /// oversized, or contains a NUL byte.
    pub fn with_excluded_path_prefix(mut self, prefix: Option<&str>) -> Result<Self, StorageError> {
        validate_finding_path(prefix, "finding_excluded_path")?;
        self.excluded_path_prefix = prefix.map(ToOwned::to_owned);
        Ok(self)
    }

    /// Restrict results to one literal normalized-path prefix.
    /// # Errors
    ///
    /// Returns an error if the optional required prefix is empty, absolute,
    /// oversized, or contains a NUL byte.
    pub fn with_path_prefix(mut self, prefix: Option<&str>) -> Result<Self, StorageError> {
        validate_finding_path(prefix, "finding_path_prefix")?;
        self.path_prefix = prefix.map(ToOwned::to_owned);
        Ok(self)
    }

    /// Restrict findings to a non-empty, bounded set of exact symbol identities.
    /// # Errors
    ///
    /// Returns an error if the symbol set is empty or contains more than 20 identities.
    pub fn with_symbol_ids(mut self, symbol_ids: Vec<SymbolId>) -> Result<Self, StorageError> {
        if symbol_ids.is_empty() || symbol_ids.len() > 20 {
            return Err(StorageError::InvalidInput {
                field: "finding_symbol_ids",
            });
        }
        for symbol_id in symbol_ids {
            if !self.symbol_ids.contains(&symbol_id) {
                self.symbol_ids.push(symbol_id);
            }
        }
        Ok(self)
    }

    /// Drop conventional test, fixture, mock, example, and benchmark paths.
    #[must_use]
    pub const fn with_exclude_fixtures(mut self, exclude: bool) -> Self {
        self.exclude_fixtures = exclude;
        self
    }
}

/// Project-wide deterministic finding totals without a row-limit blind spot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralFindingComputationState {
    /// No complete detector relation exists for the selected generation.
    #[default]
    NotComputed,
    /// Every detector completed for the exact current input fingerprint.
    Ready,
}

/// Project-wide deterministic finding totals without a row-limit blind spot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralFindingStats {
    state: StructuralFindingComputationState,
    analyzed_symbols: u64,
    total_findings: u64,
    info_findings: u64,
    warning_findings: u64,
    error_findings: u64,
    very_long_symbols: u64,
    high_fan_out_symbols: u64,
    unresolved_reference_pressure: u64,
    code_health_score: f32,
    by_finding: BTreeMap<String, u64>,
}

impl StructuralFindingStats {
    /// Whether the complete detector relation was actually computed.
    #[must_use]
    pub const fn state(&self) -> StructuralFindingComputationState {
        self.state
    }
    /// Current-generation symbols evaluated by the complete detector relation.
    #[must_use]
    pub const fn analyzed_symbols(&self) -> u64 {
        self.analyzed_symbols
    }

    /// Exact number of findings across every detector before any response limit.
    #[must_use]
    pub const fn total_findings(&self) -> u64 {
        self.total_findings
    }

    /// Exact number of error-severity findings.
    #[must_use]
    pub const fn error_findings(&self) -> u64 {
        self.error_findings
    }
}

impl StructuralFinding {
    /// Stable symbol identity for composing findings with exact lookups.
    #[must_use]
    pub fn symbol_id(&self) -> &str {
        &self.symbol_id
    }

    /// Project-relative source path carrying the finding.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stable qualified symbol name carrying the finding.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Stable detector name.
    #[must_use]
    pub fn finding(&self) -> &str {
        &self.finding
    }

    /// Normalized incoming-edge-site degree used for impact filtering.
    #[must_use]
    pub const fn degree_centrality(&self) -> f64 {
        self.degree_centrality
    }
}

fn validate_limit(limit: u16) -> Result<(), StorageError> {
    if limit == 0 || limit > MAX_INSIGHT_LIMIT {
        Err(StorageError::InvalidInput { field: "limit" })
    } else {
        Ok(())
    }
}

fn validate_finding_text(value: Option<&str>, field: &'static str) -> Result<(), StorageError> {
    if value.is_some_and(|value| value.is_empty() || value.len() > 128 || value.contains('\0')) {
        Err(StorageError::InvalidInput { field })
    } else {
        Ok(())
    }
}

fn validate_finding_path(value: Option<&str>, field: &'static str) -> Result<(), StorageError> {
    if value.is_some_and(|value| {
        value.is_empty() || value.len() > 4_096 || value.contains('\0') || value.starts_with('/')
    }) {
        Err(StorageError::InvalidInput { field })
    } else {
        Ok(())
    }
}

use crate::StorageError;
