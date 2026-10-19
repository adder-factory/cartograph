use std::{collections::BTreeMap, time::Duration};

use cartograph_domain::{NormalizedPath, ProjectId, SourceLanguage};
use serde::Serialize;

const FILE_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FILE_ROWS: u16 = 2_000;
const MAX_FILE_DEPENDENCY_ROWS: u16 = 500;
const MAX_PATH_REGEX_BYTES: usize = 8_192;

mod native;

/// Database-side file inventory filters applied before the response cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSurfaceQuery {
    directory: Option<NormalizedPath>,
    language: Option<SourceLanguage>,
    path_regex: Option<String>,
    limit: u16,
}

impl FileSurfaceQuery {
    /// Build a bounded inventory query.
    /// # Errors
    ///
    /// Returns an error if `limit` is zero or exceeds the file-surface row cap.
    pub fn new(limit: u16) -> Result<Self, StorageError> {
        validate_limit(limit, MAX_FILE_ROWS, "file_surface_limit")?;
        Ok(Self {
            directory: None,
            language: None,
            path_regex: None,
            limit,
        })
    }

    /// Restrict rows to one exact directory subtree.
    #[must_use]
    pub fn within_directory(mut self, directory: NormalizedPath) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Restrict rows to one supported language.
    #[must_use]
    pub const fn with_language(mut self, language: SourceLanguage) -> Self {
        self.language = Some(language);
        self
    }

    /// Apply an anchored regular expression generated from a bounded glob.
    /// # Errors
    ///
    /// Returns an error if the optional expression is empty, contains a NUL
    /// byte, or exceeds the path-regex byte bound.
    pub fn with_path_regex(mut self, regex: Option<&str>) -> Result<Self, StorageError> {
        if regex.is_some_and(|value| {
            value.is_empty() || value.len() > MAX_PATH_REGEX_BYTES || value.contains('\0')
        }) {
            return Err(StorageError::InvalidInput {
                field: "file_path_regex",
            });
        }
        self.path_regex = regex.map(ToOwned::to_owned);
        Ok(self)
    }
}

/// One current file with exact structural counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSurfaceRow {
    path: String,
    language: String,
    byte_size: u64,
    parse_status: String,
    symbol_count: u64,
}

impl FileSurfaceRow {
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
    /// Returns the byte size.
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    #[must_use]
    /// Returns the symbol count.
    pub const fn symbol_count(&self) -> u64 {
        self.symbol_count
    }
}

/// Exact pre-limit inventory totals plus a bounded path-ordered page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSurfaceResult {
    files: Vec<FileSurfaceRow>,
    total_files: u64,
    total_bytes: u64,
    total_symbols: u64,
    truncated: bool,
}

/// Exact full-filter language rollup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLanguageAggregate {
    language: String,
    files: u64,
    symbols: u64,
    bytes: u64,
}

/// Recursive directory rollup; each file contributes to every ancestor directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDirectoryAggregate {
    path: String,
    files: u64,
    symbols: u64,
    bytes: u64,
}

/// Complete language totals plus a bounded recursive directory page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileAggregateResult {
    languages: Vec<FileLanguageAggregate>,
    directories: Vec<FileDirectoryAggregate>,
    total_directories: u64,
    directories_truncated: bool,
}

impl FileAggregateResult {
    #[must_use]
    /// Returns the languages.
    pub fn languages(&self) -> &[FileLanguageAggregate] {
        &self.languages
    }

    #[must_use]
    /// Returns the directories.
    pub fn directories(&self) -> &[FileDirectoryAggregate] {
        &self.directories
    }

    #[must_use]
    /// Returns whether additional matching directories were omitted.
    pub const fn directories_truncated(&self) -> bool {
        self.directories_truncated
    }
}

impl FileSurfaceResult {
    #[must_use]
    /// Returns the files.
    pub fn files(&self) -> &[FileSurfaceRow] {
        &self.files
    }

    #[must_use]
    /// Returns the total files.
    pub const fn total_files(&self) -> u64 {
        self.total_files
    }

    #[must_use]
    /// Returns the total bytes.
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    #[must_use]
    /// Returns the total symbols.
    pub const fn total_symbols(&self) -> u64 {
        self.total_symbols
    }

    #[must_use]
    /// Whether the file limit omitted additional matching rows.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Direction for one file's cross-file graph relationships.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDependencyDirection {
    /// Return files referenced by the anchor file.
    Dependencies,
    /// Return files that reference the anchor file.
    Dependents,
    #[default]
    /// Return both dependencies and dependents.
    Both,
}

impl FileDependencyDirection {}

/// Independently bounded dependency/dependent request for one exact indexed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDependencyQuery {
    path: NormalizedPath,
    direction: FileDependencyDirection,
    per_direction_limit: u16,
}

impl FileDependencyQuery {
    /// Creates a validated file dependency query.
    ///
    /// # Errors
    ///
    /// Returns an error if `per_direction_limit` is zero or exceeds the
    /// bounded adjacent-file row maximum.
    pub fn new(path: NormalizedPath, per_direction_limit: u16) -> Result<Self, StorageError> {
        validate_limit(
            per_direction_limit,
            MAX_FILE_DEPENDENCY_ROWS,
            "file_dependency_limit",
        )?;
        Ok(Self {
            path,
            direction: FileDependencyDirection::Both,
            per_direction_limit,
        })
    }

    #[must_use]
    /// Sets the direction and returns the updated value.
    pub const fn with_direction(mut self, direction: FileDependencyDirection) -> Self {
        self.direction = direction;
        self
    }
}

/// One adjacent file with aggregated edge kinds and exact represented sites.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDependencyRow {
    direction: String,
    path: String,
    language: String,
    edge_count: u64,
    site_count: u64,
    edge_kinds: Vec<String>,
}

/// Exact pre-limit counts and independently capped adjacent-file groups.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDependencyResult {
    rows: Vec<FileDependencyRow>,
    counts: BTreeMap<String, u64>,
    truncated: BTreeMap<String, bool>,
}

impl FileDependencyResult {
    #[must_use]
    /// Returns the rows.
    pub fn rows(&self) -> &[FileDependencyRow] {
        &self.rows
    }
}

fn validate_limit(limit: u16, maximum: u16, field: &'static str) -> Result<(), StorageError> {
    if limit == 0 || limit > maximum {
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
    fn file_queries_reject_unbounded_inputs() {
        assert!(FileSurfaceQuery::new(0).is_err());
        assert!(FileSurfaceQuery::new(MAX_FILE_ROWS + 1).is_err());
        assert!(FileSurfaceQuery::new(MAX_FILE_ROWS).is_ok());
        assert!(
            FileSurfaceQuery::new(1)
                .and_then(|query| query.with_path_regex(Some(&"x".repeat(MAX_PATH_REGEX_BYTES + 1))))
                .is_err()
        );
        let path = NormalizedPath::parse("src/lib.rs")
            .unwrap_or_else(|error| panic!("fixture path failed: {error}"));
        assert!(FileDependencyQuery::new(path, MAX_FILE_DEPENDENCY_ROWS + 1).is_err());
    }
}
