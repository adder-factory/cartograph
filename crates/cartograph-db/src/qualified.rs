//! Generation-fenced native storage execution for field-qualified symbol search.

use std::time::Duration;

use cartograph_domain::{GenerationId, ProjectId};
use serde::Serialize;

const MAXIMUM_QUERY_BYTES: usize = 1_024;
mod native;
const MAXIMUM_FILTER_BYTES: usize = 4_096;
const MAXIMUM_FILTER_VALUES: usize = 64;
const MAXIMUM_RESULT_LIMIT: u16 = 100;
const DEFAULT_QUALIFIED_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Generation-scoped centrality comparison with v1-compatible unranked-symbol semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualifiedCentralityComparator {
    /// Require a non-null score strictly above the threshold.
    GreaterThan,
    /// Require a non-null score at or above the threshold.
    GreaterThanOrEqual,
    /// Require a score below the threshold; unranked symbols are retained.
    LessThan,
    /// Require a score at or below the threshold; unranked symbols are retained.
    LessThanOrEqual,
}

impl QualifiedCentralityComparator {}

/// Stable qualified-query ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QualifiedSymbolSort {
    /// Deterministic declaration name, path, and stable identity order.
    #[default]
    Relevance,
    /// Directed `PageRank` descending, with unranked symbols last.
    Centrality,
}

/// One bounded current-generation field-qualified query.
#[derive(Clone, Copy, Debug)]
pub struct QualifiedSymbolQuery<'a> {
    project_id: &'a ProjectId,
    expected_generation_id: &'a GenerationId,
    text: &'a str,
    kinds: &'a [String],
    languages: &'a [String],
    path_prefixes: &'a [String],
    path_filters: &'a [String],
    name_filters: &'a [String],
    signature_filters: &'a [String],
    callers_of: &'a [String],
    callees_of: &'a [String],
    depends_on: &'a [String],
    centrality: Option<(QualifiedCentralityComparator, f64)>,
    sort: QualifiedSymbolSort,
    scan_all: bool,
    limit: u16,
}

impl<'a> QualifiedSymbolQuery<'a> {
    /// Bind a parsed query to one immutable published generation.
    #[must_use]
    pub const fn new(generation: CurrentGenerationLookup<'a>, text: &'a str, limit: u16) -> Self {
        Self {
            project_id: generation.project_id(),
            expected_generation_id: generation.expected_generation_id(),
            text,
            kinds: &[],
            languages: &[],
            path_prefixes: &[],
            path_filters: &[],
            name_filters: &[],
            signature_filters: &[],
            callers_of: &[],
            callees_of: &[],
            depends_on: &[],
            centrality: None,
            sort: QualifiedSymbolSort::Relevance,
            scan_all: false,
            limit,
        }
    }

    /// Add OR-composed symbol kinds.
    #[must_use]
    pub const fn with_kinds(mut self, values: &'a [String]) -> Self {
        self.kinds = values;
        self
    }

    /// Add OR-composed source languages.
    #[must_use]
    pub const fn with_languages(mut self, values: &'a [String]) -> Self {
        self.languages = values;
        self
    }

    /// Add explicit normalized path prefixes.
    #[must_use]
    pub const fn with_path_prefixes(mut self, values: &'a [String]) -> Self {
        self.path_prefixes = values;
        self
    }

    /// Add case-insensitive path substring filters.
    #[must_use]
    pub const fn with_path_filters(mut self, values: &'a [String]) -> Self {
        self.path_filters = values;
        self
    }

    /// Add case-insensitive simple-name substring filters.
    #[must_use]
    pub const fn with_name_filters(mut self, values: &'a [String]) -> Self {
        self.name_filters = values;
        self
    }

    /// Add case-insensitive signature substring filters.
    #[must_use]
    pub const fn with_signature_filters(mut self, values: &'a [String]) -> Self {
        self.signature_filters = values;
        self
    }

    /// Add OR-composed call targets; this category intersects with other categories.
    #[must_use]
    pub const fn with_callers_of(mut self, values: &'a [String]) -> Self {
        self.callers_of = values;
        self
    }

    /// Add OR-composed call sources; this category intersects with other categories.
    #[must_use]
    pub const fn with_callees_of(mut self, values: &'a [String]) -> Self {
        self.callees_of = values;
        self
    }

    /// Add OR-composed structural dependency targets.
    #[must_use]
    pub const fn with_depends_on(mut self, values: &'a [String]) -> Self {
        self.depends_on = values;
        self
    }

    /// Add one centrality predicate.
    #[must_use]
    pub const fn with_centrality(
        mut self,
        comparator: QualifiedCentralityComparator,
        threshold: f64,
    ) -> Self {
        self.centrality = Some((comparator, threshold));
        self
    }

    /// Select relevance or graph-centrality ordering.
    #[must_use]
    pub const fn with_sort(mut self, sort: QualifiedSymbolSort) -> Self {
        self.sort = sort;
        self
    }

    /// Permit an explicitly requested bounded all-symbol scan, such as `sort:centrality` alone.
    #[must_use]
    pub const fn with_scan_all(mut self, scan_all: bool) -> Self {
        self.scan_all = scan_all;
        self
    }
}

/// One qualified-query hit with lexical and graph ranking evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QualifiedSymbolHit {
    symbol: CurrentSymbolRecord,
    centrality: Option<f64>,
    lexical_score: Option<f64>,
}

impl QualifiedSymbolHit {
    /// Hydrated current-generation declaration.
    #[must_use]
    pub const fn symbol(&self) -> &CurrentSymbolRecord {
        &self.symbol
    }

    /// Directed calls/references `PageRank`, when computed.
    #[must_use]
    pub const fn centrality(&self) -> Option<f64> {
        self.centrality
    }

    /// Reserved lexical score. Exact-name identity queries do not use a tokenizer.
    #[must_use]
    pub const fn lexical_score(&self) -> Option<f64> {
        self.lexical_score
    }
}

/// Exact pre-limit cardinality plus one deterministic result page.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QualifiedSymbolPage {
    total: u64,
    hits: Vec<QualifiedSymbolHit>,
}

impl QualifiedSymbolPage {
    /// Exact number of declarations matching all hard filters before `LIMIT`.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Deterministically ranked bounded result rows.
    #[must_use]
    pub fn hits(&self) -> &[QualifiedSymbolHit] {
        &self.hits
    }

    /// Whether more matching declarations exist beyond this page.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.total > u64::try_from(self.hits.len()).unwrap_or(u64::MAX)
    }
}

fn validate_query(input: &QualifiedSymbolQuery<'_>) -> Result<(), StorageError> {
    validate_query_text(input.text)?;
    validate_query_limit(input.limit)?;
    validate_primary_filters(input)?;
    validate_text_filters(input)?;
    validate_graph_filters(input)?;
    validate_centrality(input.centrality)?;
    if !query_has_constraint(input) {
        return Err(invalid("query"));
    }
    Ok(())
}

fn validate_query_text(text: &str) -> Result<(), StorageError> {
    if text.len() > MAXIMUM_QUERY_BYTES || text.contains('\0') {
        return Err(invalid("query"));
    }
    Ok(())
}

fn validate_query_limit(limit: u16) -> Result<(), StorageError> {
    if !(1..=MAXIMUM_RESULT_LIMIT).contains(&limit) {
        return Err(invalid("limit"));
    }
    Ok(())
}

fn validate_primary_filters(input: &QualifiedSymbolQuery<'_>) -> Result<(), StorageError> {
    validate_filter_group([
        ("kinds", input.kinds),
        ("languages", input.languages),
        ("path_prefixes", input.path_prefixes),
    ])
}

fn validate_text_filters(input: &QualifiedSymbolQuery<'_>) -> Result<(), StorageError> {
    validate_filter_group([
        ("path_filters", input.path_filters),
        ("name_filters", input.name_filters),
        ("signature_filters", input.signature_filters),
    ])
}

fn validate_graph_filters(input: &QualifiedSymbolQuery<'_>) -> Result<(), StorageError> {
    validate_filter_group([
        ("callers_of", input.callers_of),
        ("callees_of", input.callees_of),
        ("depends_on", input.depends_on),
    ])
}

fn validate_centrality(
    centrality: Option<(QualifiedCentralityComparator, f64)>,
) -> Result<(), StorageError> {
    if centrality.is_some_and(|(_, threshold)| !threshold.is_finite() || threshold < 0.0) {
        return Err(invalid("centrality"));
    }
    Ok(())
}

fn validate_filter_group<const COUNT: usize>(
    filters: [(&'static str, &[String]); COUNT],
) -> Result<(), StorageError> {
    for (field, values) in filters {
        if invalid_filter_values(values) {
            return Err(invalid(field));
        }
    }
    Ok(())
}

fn invalid_filter_values(values: &[String]) -> bool {
    if values.len() > MAXIMUM_FILTER_VALUES {
        return true;
    }
    values
        .iter()
        .any(|value| value.is_empty() || value.len() > MAXIMUM_FILTER_BYTES || value.contains('\0'))
}

fn query_has_constraint(input: &QualifiedSymbolQuery<'_>) -> bool {
    has_primary_query_constraint(input)
        || has_text_filter_constraint(input)
        || has_graph_filter_constraint(input)
}

fn has_primary_query_constraint(input: &QualifiedSymbolQuery<'_>) -> bool {
    [
        !input.text.is_empty(),
        !input.kinds.is_empty(),
        !input.languages.is_empty(),
        !input.path_prefixes.is_empty(),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn has_text_filter_constraint(input: &QualifiedSymbolQuery<'_>) -> bool {
    [
        !input.path_filters.is_empty(),
        !input.name_filters.is_empty(),
        !input.signature_filters.is_empty(),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn has_graph_filter_constraint(input: &QualifiedSymbolQuery<'_>) -> bool {
    [
        !input.callers_of.is_empty(),
        !input.callees_of.is_empty(),
        !input.depends_on.is_empty(),
        input.centrality.is_some(),
        input.scan_all,
    ]
    .into_iter()
    .any(std::convert::identity)
}

const fn invalid(field: &'static str) -> StorageError {
    StorageError::InvalidInput { field }
}

use crate::StorageError;

use crate::{CurrentGenerationLookup, CurrentSymbolRecord};

#[cfg(test)]
mod tests {
    use super::*;

    fn project_id() -> ProjectId {
        ProjectId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap_or_else(|error| panic!("project fixture is invalid: {error}"))
    }

    fn generation_id() -> GenerationId {
        GenerationId::parse("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .unwrap_or_else(|error| panic!("generation fixture is invalid: {error}"))
    }

    #[test]
    fn query_validation_requires_bounded_text_or_a_real_filter() {
        let project = project_id();
        let generation = generation_id();
        let generation = CurrentGenerationLookup::new(&project, &generation);
        assert_eq!(
            validate_query(&QualifiedSymbolQuery::new(generation, "", 10)),
            Err(StorageError::InvalidInput { field: "query" })
        );
        let kinds = vec!["function".to_owned()];
        assert!(
            validate_query(&QualifiedSymbolQuery::new(generation, "", 10).with_kinds(&kinds))
                .is_ok()
        );
        assert_eq!(
            validate_query(&QualifiedSymbolQuery::new(
                generation,
                &"x".repeat(MAXIMUM_QUERY_BYTES + 1),
                10,
            )),
            Err(StorageError::InvalidInput { field: "query" })
        );
    }
}
