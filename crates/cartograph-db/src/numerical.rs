//! Generation-fenced, read-only access to privacy-safe numerical source evidence.

use std::{collections::BTreeMap, time::Duration};

use cartograph_domain::{
    ContentDigest, FileId, GenerationId, NormalizedPath, NumericalSiteId, SymbolId,
};
use serde::Serialize;

const MAXIMUM_SITE_LIMIT: u16 = 100;
const MAXIMUM_CATEGORY_BYTES: usize = 64;
const DEFAULT_NUMERICAL_READ_TIMEOUT: Duration = Duration::from_secs(30);

mod native;

/// Validated filters for one bounded current-generation numerical-site page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumericalSiteQuery {
    limit: u16,
    path_prefix: Option<NormalizedPath>,
    owner_symbol_id: Option<SymbolId>,
    operation: Option<String>,
    hazard: Option<String>,
    precision: Option<String>,
    evidence_level: Option<String>,
    minimum_confidence_ppm: u32,
    hazards_only: bool,
}

impl NumericalSiteQuery {
    /// Create one bounded page request.
    /// # Errors
    ///
    /// Returns an error when `limit` is zero or exceeds the public result ceiling.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        if limit == 0 || limit > MAXIMUM_SITE_LIMIT {
            return Err(invalid("numerical_site_limit"));
        }
        Ok(Self {
            limit,
            path_prefix: None,
            owner_symbol_id: None,
            operation: None,
            hazard: None,
            precision: None,
            evidence_level: None,
            minimum_confidence_ppm: 0,
            hazards_only: false,
        })
    }

    /// Restrict the page to one exact path or directory prefix.
    /// # Errors
    ///
    /// Returns an error when the path is not a validated project-relative path.
    pub fn with_path_prefix(mut self, value: Option<&str>) -> Result<Self, StorageError> {
        self.path_prefix = value
            .map(NormalizedPath::parse)
            .transpose()
            .map_err(|_| invalid("numerical_path_prefix"))?;
        Ok(self)
    }

    /// Restrict the page to one exact owning symbol.
    #[must_use]
    pub fn with_owner_symbol_id(mut self, value: Option<SymbolId>) -> Self {
        self.owner_symbol_id = value;
        self
    }

    /// Restrict the page to one stable operation category.
    /// # Errors
    ///
    /// Returns an error when the category is not a bounded machine token.
    pub fn with_operation(mut self, value: Option<&str>) -> Result<Self, StorageError> {
        self.operation = validated_filter(value, "numerical_operation")?;
        Ok(self)
    }

    /// Restrict the page to one stable hazard category.
    /// # Errors
    ///
    /// Returns an error when the category is not a bounded machine token.
    pub fn with_hazard(mut self, value: Option<&str>) -> Result<Self, StorageError> {
        self.hazard = validated_filter(value, "numerical_hazard")?;
        Ok(self)
    }

    /// Restrict the page to one stable precision category.
    /// # Errors
    ///
    /// Returns an error when the category is not a bounded machine token.
    pub fn with_precision(mut self, value: Option<&str>) -> Result<Self, StorageError> {
        self.precision = validated_filter(value, "numerical_precision")?;
        Ok(self)
    }

    /// Restrict the page to one explicit evidence level.
    /// # Errors
    ///
    /// Returns an error when the value is not a source-derived evidence level.
    pub fn with_evidence_level(mut self, value: Option<&str>) -> Result<Self, StorageError> {
        if value.is_some_and(|value| !matches!(value, "proven" | "heuristic" | "coverage_gap")) {
            return Err(invalid("numerical_evidence_level"));
        }
        self.evidence_level = value.map(ToOwned::to_owned);
        Ok(self)
    }

    /// Require at least the supplied deterministic confidence in parts per million.
    /// # Errors
    ///
    /// Returns an error when the value is above one million.
    pub fn with_minimum_confidence_ppm(mut self, value: u32) -> Result<Self, StorageError> {
        if value > 1_000_000 {
            return Err(invalid("numerical_minimum_confidence_ppm"));
        }
        self.minimum_confidence_ppm = value;
        Ok(self)
    }

    /// Omit sites whose current static pass observed no named hazard.
    #[must_use]
    pub const fn with_hazards_only(mut self, value: bool) -> Self {
        self.hazards_only = value;
        self
    }
}

/// One exact, privacy-safe current-generation numerical source site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumericalSiteRecord {
    generation_id: GenerationId,
    numerical_site_id: NumericalSiteId,
    file_id: FileId,
    path: NormalizedPath,
    language: String,
    owner_symbol_id: Option<SymbolId>,
    owner_qualified_name: Option<String>,
    start_byte: u64,
    end_byte: u64,
    start_line: u32,
    end_line: u32,
    operation: String,
    hazard: String,
    precision: String,
    expression_digest: ContentDigest,
    confidence_ppm: u32,
    provenance: String,
    evidence_level: String,
    unknowns: Vec<String>,
}

impl NumericalSiteRecord {
    /// Immutable generation containing this site.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Stable numerical-site identity.
    #[must_use]
    pub const fn numerical_site_id(&self) -> &NumericalSiteId {
        &self.numerical_site_id
    }

    /// Source file containing this site.
    #[must_use]
    pub const fn file_id(&self) -> &FileId {
        &self.file_id
    }

    /// Project-relative source path.
    #[must_use]
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    /// Closest containing declaration, when extraction proved one.
    #[must_use]
    pub const fn owner_symbol_id(&self) -> Option<&SymbolId> {
        self.owner_symbol_id.as_ref()
    }

    /// Stable operation category.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// Potential-hazard category, or `none_observed`.
    #[must_use]
    pub fn hazard(&self) -> &str {
        &self.hazard
    }

    /// Best statically visible precision category.
    #[must_use]
    pub fn precision(&self) -> &str {
        &self.precision
    }

    /// Evidence tier, independent of confidence.
    #[must_use]
    pub fn evidence_level(&self) -> &str {
        &self.evidence_level
    }

    /// Facts that this static analyzer could not prove.
    #[must_use]
    pub fn unknowns(&self) -> &[String] {
        &self.unknowns
    }
}

/// Exact pre-limit cardinality plus one deterministic numerical-site page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumericalSitePage {
    generation_id: GenerationId,
    total: u64,
    sites: Vec<NumericalSiteRecord>,
    truncated: bool,
}

impl NumericalSitePage {
    /// Immutable generation queried by this page.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Complete count before the page cap.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Deterministically ordered page rows.
    #[must_use]
    pub fn sites(&self) -> &[NumericalSiteRecord] {
        &self.sites
    }

    /// Whether additional matching sites were omitted.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Complete current-generation static numerical readiness rollup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumericalSiteStats {
    generation_id: GenerationId,
    total_sites: u64,
    hazard_sites: u64,
    unknown_precision_sites: u64,
    sites_with_unknowns: u64,
    supported_files: u64,
    analyzed_files: u64,
    unanalyzed_files: u64,
    files_with_sites: u64,
    symbols_with_sites: u64,
    by_operation: BTreeMap<String, u64>,
    by_hazard: BTreeMap<String, u64>,
    by_precision: BTreeMap<String, u64>,
    by_evidence_level: BTreeMap<String, u64>,
    by_provenance: BTreeMap<String, u64>,
    by_language: BTreeMap<String, u64>,
    breakdowns_truncated: bool,
}

impl NumericalSiteStats {
    /// Immutable generation summarized by this rollup.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// Complete number of persisted static numerical sites.
    #[must_use]
    pub const fn total_sites(&self) -> u64 {
        self.total_sites
    }

    /// Sites whose static pass named a potential hazard.
    #[must_use]
    pub const fn hazard_sites(&self) -> u64 {
        self.hazard_sites
    }

    /// Whether at least one static site is queryable in this generation.
    #[must_use]
    pub const fn has_sites(&self) -> bool {
        self.total_sites > 0
    }

    /// Number of current-generation Rust files covered by the MVP analyzer contract.
    #[must_use]
    pub const fn supported_files(&self) -> u64 {
        self.supported_files
    }

    /// Number of supported files whose parsed or partial syntax tree was analyzed.
    #[must_use]
    pub const fn analyzed_files(&self) -> u64 {
        self.analyzed_files
    }

    /// Number of supported files skipped or failed before numerical analysis.
    #[must_use]
    pub const fn unanalyzed_files(&self) -> u64 {
        self.unanalyzed_files
    }

    /// Supported files containing at least one numerical site.
    #[must_use]
    pub const fn files_with_sites(&self) -> u64 {
        self.files_with_sites
    }
}

fn validated_filter(
    value: Option<&str>,
    field: &'static str,
) -> Result<Option<String>, StorageError> {
    value
        .map(|value| {
            if value.is_empty()
                || value.len() > MAXIMUM_CATEGORY_BYTES
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                Err(invalid(field))
            } else {
                Ok(value.to_owned())
            }
        })
        .transpose()
}

const fn invalid(field: &'static str) -> StorageError {
    StorageError::InvalidInput { field }
}

use crate::StorageError;

use crate::CurrentGenerationLookup;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numerical_query_filters_are_bounded_machine_tokens() {
        assert!(NumericalSiteQuery::new(0).is_err());
        assert!(NumericalSiteQuery::new(MAXIMUM_SITE_LIMIT + 1).is_err());
        assert!(NumericalSiteQuery::new(1).is_ok());

        let query = NumericalSiteQuery::new(10)
            .and_then(|query| query.with_path_prefix(Some("src/numerical")))
            .and_then(|query| query.with_operation(Some("tolerance_comparison")))
            .and_then(|query| query.with_hazard(Some("absolute_only_tolerance")))
            .and_then(|query| query.with_precision(Some("f32")))
            .and_then(|query| query.with_evidence_level(Some("heuristic")))
            .and_then(|query| query.with_minimum_confidence_ppm(900_000));
        assert!(query.is_ok());

        for value in ["", "Uppercase", "contains-hyphen", "contains space"] {
            assert!(
                NumericalSiteQuery::new(1)
                    .and_then(|query| query.with_operation(Some(value)))
                    .is_err()
            );
        }
        assert!(
            NumericalSiteQuery::new(1)
                .and_then(|query| query.with_evidence_level(Some("observed")))
                .is_err()
        );
        assert!(
            NumericalSiteQuery::new(1)
                .and_then(|query| query.with_minimum_confidence_ppm(1_000_001))
                .is_err()
        );
    }
}
