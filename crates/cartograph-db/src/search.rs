use std::time::Duration;

use cartograph_domain::{DocumentId, FileId, GenerationId, ProjectId, SymbolId, SymbolKind};
use serde::Serialize;

pub(crate) mod native;

const MAX_QUERY_BYTES: usize = 1_024;
const MAX_RESULT_LIMIT: u16 = 100;
const DEFAULT_INTERACTIVE_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum SearchFlavor {
    All,
    Name,
    Intent,
    FuzzyName(u8),
}

/// Bounded BM25 query against one project's atomically published generation.
pub struct SearchQuery {
    project_id: ProjectId,
    expected_generation_id: GenerationId,
    query: String,
    limit: u16,
}

impl SearchQuery {
    /// Build a query request. It is validated immediately before execution.
    #[must_use]
    pub fn new(
        generation: CurrentGenerationLookup<'_>,
        query: impl Into<String>,
        limit: u16,
    ) -> Self {
        Self {
            project_id: generation.project_id().clone(),
            expected_generation_id: generation.expected_generation_id().clone(),
            query: query.into().trim().to_owned(),
            limit,
        }
    }
}

/// Fields contributing to native lexical evidence from the current generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchComponent {
    /// Qualified declaration names contributed to the match.
    QualifiedName,
    /// Indexed source code contributed to the match.
    Code,
    /// Documentation or other natural language contributed to the match.
    NaturalText,
}

/// Deterministically ordered native BM25 evidence from the current generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    document_id: DocumentId,
    generation_id: GenerationId,
    file_id: Option<FileId>,
    symbol_id: Option<SymbolId>,
    path: String,
    language: String,
    document_kind: String,
    qualified_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<SymbolKind>,
    score: f64,
    components: Vec<SearchComponent>,
}

impl SearchHit {
    /// Stable logical document identity.
    #[must_use]
    pub const fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    /// Published generation that produced this evidence.
    #[must_use]
    pub const fn generation_id(&self) -> &GenerationId {
        &self.generation_id
    }

    /// File represented by the document, when the document is file-backed.
    #[must_use]
    pub const fn file_id(&self) -> Option<&FileId> {
        self.file_id.as_ref()
    }

    /// Symbol represented by the document, when the document is symbol-backed.
    #[must_use]
    pub const fn symbol_id(&self) -> Option<&SymbolId> {
        self.symbol_id.as_ref()
    }

    /// Normalized project path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stable indexed language name.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Stable search-document kind name.
    #[must_use]
    pub fn document_kind(&self) -> &str {
        &self.document_kind
    }

    /// Qualified symbol/declaration name when present.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Exact extracted symbol category when retained in current search metadata.
    #[must_use]
    pub const fn symbol_kind(&self) -> Option<SymbolKind> {
        self.symbol_kind
    }

    /// Native field-weighted BM25 score; only ordering within this candidate channel
    /// is meaningful.
    #[must_use]
    pub const fn score(&self) -> f64 {
        self.score
    }

    /// Ordered fields that contributed to the BM25 candidate match. This is
    /// provenance, not a cross-field score decomposition.
    #[must_use]
    pub fn components(&self) -> &[SearchComponent] {
        &self.components
    }
}

fn validate_query(input: &SearchQuery) -> Result<(), StorageError> {
    if input.query.is_empty() || input.query.len() > MAX_QUERY_BYTES || input.query.contains('\0') {
        return Err(StorageError::InvalidInput { field: "query" });
    }
    if !(1..=MAX_RESULT_LIMIT).contains(&input.limit) {
        return Err(StorageError::InvalidInput { field: "limit" });
    }
    Ok(())
}

use crate::StorageError;

use crate::CurrentGenerationLookup;

#[cfg(test)]
mod tests {
    use super::*;

    fn project_id() -> ProjectId {
        match ProjectId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa") {
            Ok(id) => id,
            Err(error) => panic!("fixture UUID is invalid: {error}"),
        }
    }

    fn generation_id() -> GenerationId {
        GenerationId::parse("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .unwrap_or_else(|error| panic!("generation fixture UUID is invalid: {error}"))
    }

    fn search_query(query: impl Into<String>, limit: u16) -> SearchQuery {
        let project = project_id();
        let generation = generation_id();
        SearchQuery::new(
            CurrentGenerationLookup::new(&project, &generation),
            query,
            limit,
        )
    }

    #[test]
    fn query_bounds_reject_empty_unbounded_and_zero_limit_requests() {
        assert_eq!(
            validate_query(&search_query("   ", 10)),
            Err(StorageError::InvalidInput { field: "query" })
        );
        assert_eq!(
            validate_query(&search_query("x".repeat(MAX_QUERY_BYTES + 1), 10)),
            Err(StorageError::InvalidInput { field: "query" })
        );
        assert_eq!(
            validate_query(&search_query("parser", 0)),
            Err(StorageError::InvalidInput { field: "limit" })
        );
    }

    #[test]
    fn component_provenance_has_a_stable_priority_order() {
        let components = [
            SearchComponent::QualifiedName,
            SearchComponent::Code,
            SearchComponent::NaturalText,
        ];
        assert!(components.is_sorted_by(|previous, next| previous < next));
    }

    #[test]
    fn query_validation_errors_never_render_query_text() {
        let untrusted_input = "opaque-caller-query\0";
        let error = match validate_query(&search_query(untrusted_input, 10)) {
            Ok(()) => panic!("nul-bearing query unexpectedly passed validation"),
            Err(error) => error,
        };
        let rendered = error.to_string();
        assert!(!rendered.contains("opaque-caller-query"));
        assert!(rendered.contains("query"));
    }
}
