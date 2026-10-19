use std::time::Duration;

use cartograph_domain::{ContentDigest, GenerationId, NormalizedPath, ProjectId, SymbolId};
use serde::{Deserialize, Serialize};
pub(crate) mod native;

const COVERAGE_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_COVERAGE_FACTS: usize = 1_000_000;
const MAX_COVERAGE_LIMIT: u16 = 500;

/// Current-generation symbol span used to join LCOV line observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageTarget {
    generation_id: GenerationId,
    symbol_id: SymbolId,
    path: NormalizedPath,
    symbol_kind: String,
    qualified_name: String,
    start_line: u32,
    end_line: u32,
}

impl CoverageTarget {
    #[must_use]
    /// Returns the generation ID.
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    #[must_use]
    /// Returns the symbol ID.
    pub const fn symbol_id(&self) -> &SymbolId {
        &self.symbol_id
    }

    #[must_use]
    /// Returns the path.
    pub const fn path(&self) -> &NormalizedPath {
        &self.path
    }

    #[must_use]
    /// Returns the start line.
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    #[must_use]
    /// Returns the end line.
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }
}

/// Validated found/hit counts for one coverage dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageCount {
    found: u64,
    hit: u64,
}

impl CoverageCount {
    /// Public constant defining the zero.
    pub const ZERO: Self = Self { found: 0, hit: 0 };

    /// Creates a validated coverage count.
    ///
    /// # Errors
    ///
    /// Returns an error if `hit` exceeds `found`.
    pub fn new(found: u64, hit: u64) -> Result<Self, StorageError> {
        if hit > found {
            Err(StorageError::InvalidInput {
                field: "coverage_counts",
            })
        } else {
            Ok(Self { found, hit })
        }
    }
}

/// One exact symbol-level coverage observation ready for bulk insertion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolCoverageFact {
    symbol_id: SymbolId,
    lines_found: u64,
    lines_hit: u64,
    functions_found: u64,
    functions_hit: u64,
}

impl SymbolCoverageFact {
    #[must_use]
    /// Creates a validated symbol coverage fact.
    pub fn new(symbol_id: SymbolId, lines: CoverageCount, functions: CoverageCount) -> Self {
        Self {
            symbol_id,
            lines_found: lines.found,
            lines_hit: lines.hit,
            functions_found: functions.found,
            functions_hit: functions.hit,
        }
    }
}

/// Identity and provenance for one current-generation coverage replacement.
pub struct CoverageLoadInput {
    /// Stable project ID for this record.
    pub project_id: ProjectId,
    /// Stable generation ID for this record.
    pub generation_id: GenerationId,
    /// Source label for this record.
    pub source_label: String,
    /// Digest-fenced report digest for this record.
    pub report_digest: ContentDigest,
}

/// Validated replacement of one source/current-generation coverage snapshot.
pub struct CoverageLoadRequest {
    project_id: ProjectId,
    generation_id: GenerationId,
    source_label: String,
    report_digest: ContentDigest,
    report_metadata: serde_json::Value,
    facts: Vec<SymbolCoverageFact>,
}

impl CoverageLoadRequest {
    /// Creates a validated coverage load request.
    ///
    /// # Errors
    ///
    /// Returns an error if the source label is empty, oversized, or contains a
    /// NUL byte, or the fact count exceeds the atomic load bound.
    pub fn new(
        input: CoverageLoadInput,
        facts: Vec<SymbolCoverageFact>,
    ) -> Result<Self, StorageError> {
        let CoverageLoadInput {
            project_id,
            generation_id,
            source_label,
            report_digest,
        } = input;
        if source_label.is_empty()
            || source_label.len() > 256
            || source_label.contains('\0')
            || facts.len() > MAX_COVERAGE_FACTS
        {
            return Err(StorageError::InvalidInput {
                field: "coverage_load",
            });
        }
        Ok(Self {
            project_id,
            generation_id,
            source_label,
            report_digest,
            report_metadata: serde_json::json!({}),
            facts,
        })
    }

    /// Sets the metadata and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata is not a JSON object, cannot be encoded, or
    /// exceeds the coverage metadata byte limit.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Result<Self, StorageError> {
        let bytes = serde_json::to_vec(&metadata).map_err(|_| StorageError::InvalidInput {
            field: "coverage_metadata",
        })?;
        if !metadata.is_object() || bytes.len() > 65_536 {
            return Err(StorageError::InvalidInput {
                field: "coverage_metadata",
            });
        }
        self.report_metadata = metadata;
        Ok(self)
    }
}

/// Atomic LCOV replacement result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageLoadReport {
    generation_id: GenerationId,
    source_id: String,
    source_label: String,
    symbols_written: u64,
}

/// One graph-composed LCOV row.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolCoverageRecord {
    symbol_id: String,
    path: String,
    language: String,
    symbol_kind: String,
    qualified_name: String,
    source: String,
    lines_found: u64,
    lines_hit: u64,
    coverage_fraction: Option<f64>,
    incoming_edges: u64,
    direct_test_files: u64,
    degree_centrality: f64,
}

/// Validated filters for one current-generation coverage ranking.
#[derive(Clone, Debug, PartialEq)]
pub struct SymbolCoverageQuery {
    source: Option<String>,
    symbol_id: Option<SymbolId>,
    limit: u16,
    include_tests: bool,
    maximum_fraction: Option<f64>,
    minimum_centrality: Option<f64>,
    symbol_kinds: Vec<String>,
    path_prefix: Option<String>,
}

impl SymbolCoverageQuery {
    /// Creates a validated symbol coverage query.
    ///
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the bounded coverage row maximum.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        validate_source_and_limit(None, limit)?;
        Ok(Self {
            source: None,
            symbol_id: None,
            limit,
            include_tests: true,
            maximum_fraction: None,
            minimum_centrality: None,
            symbol_kinds: Vec::new(),
            path_prefix: None,
        })
    }

    /// Sets the source and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the optional source label is empty, oversized, or
    /// contains a NUL byte.
    pub fn with_source(mut self, source: Option<&str>) -> Result<Self, StorageError> {
        validate_source(source)?;
        self.source = source.map(ToOwned::to_owned);
        Ok(self)
    }

    #[must_use]
    /// Sets the symbol and returns the updated value.
    pub fn with_symbol(mut self, symbol_id: Option<SymbolId>) -> Self {
        self.symbol_id = symbol_id;
        self
    }

    #[must_use]
    /// Sets the include tests and returns the updated value.
    pub const fn with_include_tests(mut self, include_tests: bool) -> Self {
        self.include_tests = include_tests;
        self
    }

    /// Sets the maximum fraction and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the optional maximum is not finite or lies outside
    /// the inclusive coverage-fraction range from zero to one.
    pub fn with_maximum_fraction(mut self, value: Option<f64>) -> Result<Self, StorageError> {
        validate_fraction(value, "coverage_fraction")?;
        self.maximum_fraction = value;
        Ok(self)
    }

    /// Sets the minimum centrality and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the optional minimum is not finite or lies outside
    /// the inclusive centrality range from zero to one.
    pub fn with_minimum_centrality(mut self, value: Option<f64>) -> Result<Self, StorageError> {
        validate_fraction(value, "coverage_centrality")?;
        self.minimum_centrality = value;
        Ok(self)
    }

    /// Sets the symbol kinds and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if there are too many kinds or any kind is empty,
    /// oversized, or contains a NUL byte.
    pub fn with_symbol_kinds(mut self, kinds: Vec<String>) -> Result<Self, StorageError> {
        if kinds.len() > 64
            || kinds
                .iter()
                .any(|kind| kind.is_empty() || kind.len() > 64 || kind.contains('\0'))
        {
            return Err(StorageError::InvalidInput {
                field: "coverage_kinds",
            });
        }
        for kind in kinds {
            if !self.symbol_kinds.contains(&kind) {
                self.symbol_kinds.push(kind);
            }
        }
        Ok(self)
    }

    /// Sets the path prefix and returns the updated value.
    ///
    /// # Errors
    ///
    /// Returns an error if the optional prefix is empty, absolute, oversized,
    /// or contains a NUL byte.
    pub fn with_path_prefix(mut self, prefix: Option<&str>) -> Result<Self, StorageError> {
        if prefix.is_some_and(|value| {
            value.is_empty()
                || value.len() > 4_096
                || value.contains('\0')
                || value.starts_with('/')
        }) {
            return Err(StorageError::InvalidInput {
                field: "coverage_path_prefix",
            });
        }
        self.path_prefix = prefix.map(ToOwned::to_owned);
        Ok(self)
    }
}

/// One loaded coverage source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageSourceRecord {
    source_id: String,
    label: String,
    report_digest: String,
    loaded_at: String,
    current_symbols: u64,
}

/// Complete project coverage aggregate.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageStats {
    symbols: u64,
    lines_found: u64,
    lines_hit: u64,
    coverage_fraction: Option<f64>,
    sources: u64,
}

fn validate_source_and_limit(source: Option<&str>, limit: u16) -> Result<(), StorageError> {
    validate_source(source)?;
    if limit == 0 || limit > MAX_COVERAGE_LIMIT {
        Err(StorageError::InvalidInput { field: "limit" })
    } else {
        Ok(())
    }
}

fn validate_source(source: Option<&str>) -> Result<(), StorageError> {
    if source.is_some_and(|value| value.is_empty() || value.len() > 256 || value.contains('\0')) {
        Err(StorageError::InvalidInput {
            field: "coverage_source",
        })
    } else {
        Ok(())
    }
}

fn validate_fraction(value: Option<f64>, field: &'static str) -> Result<(), StorageError> {
    if value.is_some_and(|number| !number.is_finite() || !(0.0..=1.0).contains(&number)) {
        Err(StorageError::InvalidInput { field })
    } else {
        Ok(())
    }
}

use crate::StorageError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_facts_reject_impossible_hit_counts() {
        let symbol = SymbolId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap_or_else(|error| panic!("fixture symbol failed: {error}"));
        let lines =
            CoverageCount::new(10, 8).unwrap_or_else(|error| panic!("line counts failed: {error}"));
        let functions = CoverageCount::new(2, 1)
            .unwrap_or_else(|error| panic!("function counts failed: {error}"));
        let fact = SymbolCoverageFact::new(symbol, lines, functions);
        assert_eq!(fact.lines_hit, 8);
        assert!(CoverageCount::new(10, 11).is_err());
    }
}
