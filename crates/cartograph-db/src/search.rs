use std::{collections::HashSet, time::Duration};

use cartograph_domain::{DocumentId, FileId, GenerationId, ProjectId, SymbolId, SymbolKind};
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

use crate::{
    CartographDatabase, CurrentGenerationLookup, StorageError,
    database::{parse_stored_generation_id, read_stored_string, stored_value_error},
    search_relation::require_generation_search_relation,
};

const MAX_QUERY_BYTES: usize = 1_024;
const MAX_RESULT_LIMIT: u16 = 100;
const DEFAULT_INTERACTIVE_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const HIT_SYMBOL_ID_COLUMN: usize = 3;
const HIT_PATH_COLUMN: usize = 4;
const HIT_LANGUAGE_COLUMN: usize = 5;
const HIT_DOCUMENT_KIND_COLUMN: usize = 6;
const HIT_QUALIFIED_NAME_COLUMN: usize = 7;
const HIT_SYMBOL_KIND_COLUMN: usize = 8;
const HIT_SCORE_COLUMN: usize = 9;
const HIT_ROW_ID_COLUMN: usize = 10;

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

/// Deterministically ordered `ParadeDB` evidence from the current generation.
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

/// Deterministically ordered `ParadeDB` evidence from the current generation.
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

    /// Native `ParadeDB` BM25 score; only ordering within this candidate channel
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

impl CartographDatabase {
    /// Search code-aware names/source and natural-language evidence from only
    /// the generation referenced by the project's current pointer.
    /// # Errors
    ///
    /// Returns an error if query/result bounds are invalid, the generation or
    /// BM25 relation is unavailable, or a ranked code hit cannot be decoded.
    pub async fn search_current_code(
        &self,
        input: SearchQuery,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.search_current_code_bounded(input, DEFAULT_INTERACTIVE_SEARCH_TIMEOUT)
            .await
    }

    /// Search current BM25 evidence under an explicit PostgreSQL deadline.
    /// # Errors
    ///
    /// Returns an error if the explicit deadline or query bounds are invalid,
    /// the generation fence fails, or `ParadeDB` cannot complete the search.
    pub async fn search_current_code_bounded(
        &self,
        input: SearchQuery,
        statement_timeout: Duration,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.search_current_flavor(input, statement_timeout, SearchFlavor::All)
            .await
    }

    /// Search only code-aware qualified names, excluding source/natural text hits.
    /// # Errors
    ///
    /// Returns an error if query/result bounds are invalid, name-tokenized BM25
    /// is unavailable, or a qualified-name hit cannot be decoded.
    pub async fn search_current_names(
        &self,
        input: SearchQuery,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.search_current_flavor(
            input,
            DEFAULT_INTERACTIVE_SEARCH_TIMEOUT,
            SearchFlavor::Name,
        )
        .await
    }

    /// Search only summaries, docstrings, and test-derived natural language.
    /// # Errors
    ///
    /// Returns an error if query/result bounds are invalid, intent fields are
    /// unavailable in BM25, or a natural-language hit cannot be decoded.
    pub async fn search_current_intent(
        &self,
        input: SearchQuery,
    ) -> Result<Vec<SearchHit>, StorageError> {
        self.search_current_flavor(
            input,
            DEFAULT_INTERACTIVE_SEARCH_TIMEOUT,
            SearchFlavor::Intent,
        )
        .await
    }

    /// Search only qualified source-code names with `ParadeDB` typo tolerance.
    /// The edit distance is deliberately limited to Tantivy's efficient 1..=2 range.
    /// # Errors
    ///
    /// Returns an error if `edit_distance` is outside 1..=2, query/result
    /// bounds are invalid, or the fuzzy `ParadeDB` name search fails.
    pub async fn search_current_names_fuzzy(
        &self,
        input: SearchQuery,
        edit_distance: u8,
    ) -> Result<Vec<SearchHit>, StorageError> {
        if !(1..=2).contains(&edit_distance) {
            return Err(StorageError::InvalidInput {
                field: "fuzzy_edit_distance",
            });
        }
        self.search_current_flavor(
            input,
            DEFAULT_INTERACTIVE_SEARCH_TIMEOUT,
            SearchFlavor::FuzzyName(edit_distance),
        )
        .await
    }

    async fn search_current_flavor(
        &self,
        input: SearchQuery,
        statement_timeout: Duration,
        flavor: SearchFlavor,
    ) -> Result<Vec<SearchHit>, StorageError> {
        validate_query(&input)?;
        let generation =
            CurrentGenerationLookup::new(&input.project_id, &input.expected_generation_id);
        let mut transaction = crate::retrieval::begin_bounded_read(self, statement_timeout).await?;
        crate::retrieval::require_expected_current_generation(
            &mut transaction,
            &self.schema,
            generation,
        )
        .await?;
        let relation =
            require_generation_search_relation(&mut transaction, &self.schema, generation).await?;
        let (matches, fixed_components, operation): (String, Option<&[SearchComponent]>, _) =
            match flavor {
                SearchFlavor::All => (
                    "(documents.qualified_name ||| $2 OR documents.code ||| $2 OR documents.natural_text ||| $2)".to_owned(),
                    None,
                    "bm25-search",
                ),
                SearchFlavor::Name => (
                    "documents.symbol_id IS NOT NULL AND documents.qualified_name ||| $2".to_owned(),
                    Some(&[SearchComponent::QualifiedName]),
                    "name-search",
                ),
                SearchFlavor::Intent => (
                    "documents.natural_text ||| $2".to_owned(),
                    Some(&[SearchComponent::NaturalText]),
                    "intent-search",
                ),
                SearchFlavor::FuzzyName(distance) => (
                    format!("documents.symbol_id IS NOT NULL AND documents.qualified_name ||| $2::pdb.fuzzy({distance})"),
                    Some(&[SearchComponent::QualifiedName]),
                    "fuzzy-name-search",
                ),
            };
        let table = relation.qualified_table(&self.schema);
        // Field-match flags are deliberately not select-list `|||` expressions:
        // ParadeDB cannot evaluate a match outside its index scan and falls back
        // to a sequential per-row evaluation that is hundreds of times slower
        // than the top-K search itself. They are probed for the returned rows only.
        let sql = format!(
            r#"SELECT
                    documents.document_id::text,
                    documents.generation_id::text,
                    documents.file_id::text,
                    documents.symbol_id::text,
                    documents.path,
                    documents.language,
                    documents.document_kind,
                    documents.qualified_name,
                    (
                        SELECT symbols.symbol_kind
                        FROM {}."symbols" AS symbols
                        WHERE symbols.project_id = documents.project_id
                          AND symbols.generation_id = documents.generation_id
                          AND symbols.symbol_id = documents.symbol_id
                    ) AS symbol_kind,
                    pdb.score(documents.id)::double precision,
                    documents.id
                FROM {table} AS documents
                WHERE documents.project_id = CAST($1 AS uuid)
                  AND documents.generation_id = CAST($4 AS uuid)
                  AND {matches}
                ORDER BY pdb.score(documents.id) DESC, documents.id ASC
                LIMIT $3"#,
            crate::database::quoted_schema(&self.schema),
        );
        let rows = query(AssertSqlSafe(sql))
            .bind(input.project_id.as_str())
            .bind(&input.query)
            .bind(i64::from(input.limit))
            .bind(input.expected_generation_id.as_str())
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| StorageError::DatabaseOperation { operation })?;
        let probed = match fixed_components {
            Some(_) => None,
            None => Some(
                probe_field_matches(
                    &mut transaction,
                    FieldProbeInput {
                        table: &table,
                        search: &input,
                        rows: &rows,
                    },
                )
                .await
                .map_err(|()| StorageError::DatabaseOperation { operation })?,
            ),
        };
        crate::retrieval::commit_bounded_read(transaction, operation).await?;
        rows.iter()
            .map(|row| {
                let components = match (fixed_components, &probed) {
                    (Some(fixed), _) => fixed.to_vec(),
                    (None, Some(probed)) => probed.components(read_row_id(row)?),
                    (None, None) => Vec::new(),
                };
                decode_hit(row, components)
            })
            .collect()
    }
}

/// Exact per-field match sets for the admitted rows of one all-field search.
struct FieldMatches {
    qualified_name: HashSet<i64>,
    code: HashSet<i64>,
    natural_text: HashSet<i64>,
}

impl FieldMatches {
    fn components(&self, row_id: i64) -> Vec<SearchComponent> {
        [
            (&self.qualified_name, SearchComponent::QualifiedName),
            (&self.code, SearchComponent::Code),
            (&self.natural_text, SearchComponent::NaturalText),
        ]
        .into_iter()
        .filter(|(rows, _)| rows.contains(&row_id))
        .map(|(_, component)| component)
        .collect()
    }
}

/// The returned rows of one all-field search and the relation they came from.
struct FieldProbeInput<'probe> {
    table: &'probe str,
    search: &'probe SearchQuery,
    rows: &'probe [sqlx_postgres::PgRow],
}

/// Re-evaluate each field's match predicate inside the BM25 index, restricted
/// to the returned rows by an identity term set, within the same snapshot.
async fn probe_field_matches(
    transaction: &mut sqlx_postgres::PgConnection,
    input: FieldProbeInput<'_>,
) -> Result<FieldMatches, ()> {
    let FieldProbeInput {
        table,
        search,
        rows,
    } = input;
    if rows.is_empty() {
        return Ok(FieldMatches {
            qualified_name: HashSet::new(),
            code: HashSet::new(),
            natural_text: HashSet::new(),
        });
    }
    let ids = rows
        .iter()
        .map(read_row_id)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    let probe = |field: &str| {
        format!(
            "ARRAY(SELECT probe.id FROM {table} AS probe
                WHERE probe.project_id = CAST($1 AS uuid)
                  AND probe.generation_id = CAST($3 AS uuid)
                  AND probe.id = ANY($4::bigint[])
                  AND probe.{field} ||| $2)"
        )
    };
    let sql = format!(
        "SELECT {}, {}, {}",
        probe("qualified_name"),
        probe("code"),
        probe("natural_text")
    );
    let row = query(AssertSqlSafe(sql))
        .bind(search.project_id.as_str())
        .bind(&search.query)
        .bind(search.expected_generation_id.as_str())
        .bind(&ids)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| ())?;
    let set = |index: usize| {
        row.try_get::<Vec<i64>, _>(index)
            .map(|ids| ids.into_iter().collect::<HashSet<_>>())
            .map_err(|_| ())
    };
    Ok(FieldMatches {
        qualified_name: set(0)?,
        code: set(1)?,
        natural_text: set(2)?,
    })
}

fn read_row_id(row: &sqlx_postgres::PgRow) -> Result<i64, StorageError> {
    row.try_get::<i64, _>(HIT_ROW_ID_COLUMN)
        .map_err(|_| corrupt("search_row_id"))
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

fn decode_hit(
    row: &sqlx_postgres::PgRow,
    components: Vec<SearchComponent>,
) -> Result<SearchHit, StorageError> {
    let document_id = parse_document_id(row, 0)?;
    let generation_id = parse_stored_generation_id(row, 1)?;
    let file_id = parse_optional_file_id(row, 2)?;
    let symbol_id = parse_optional_symbol_id(row, HIT_SYMBOL_ID_COLUMN)?;
    let path = read_stored_string(row, HIT_PATH_COLUMN, "path")?;
    let language = read_stored_string(row, HIT_LANGUAGE_COLUMN, "language")?;
    let document_kind = read_stored_string(row, HIT_DOCUMENT_KIND_COLUMN, "document_kind")?;
    let qualified_name = read_stored_string(row, HIT_QUALIFIED_NAME_COLUMN, "qualified_name")?;
    let symbol_kind = row
        .try_get::<Option<String>, _>(HIT_SYMBOL_KIND_COLUMN)
        .map_err(|_| corrupt("symbol_kind"))?
        .map(|value| SymbolKind::from_stable_str(&value).ok_or_else(|| corrupt("symbol_kind")))
        .transpose()?;
    let score = row
        .try_get::<f64, _>(HIT_SCORE_COLUMN)
        .map_err(|_| corrupt("score"))?;
    if !score.is_finite() || score < 0.0 {
        return Err(corrupt("score"));
    }
    if components.is_empty() {
        return Err(corrupt("search_components"));
    }
    Ok(SearchHit {
        document_id,
        generation_id,
        file_id,
        symbol_id,
        path,
        language,
        document_kind,
        qualified_name,
        symbol_kind,
        score,
        components,
    })
}

fn parse_optional_file_id(
    row: &sqlx_postgres::PgRow,
    index: usize,
) -> Result<Option<FileId>, StorageError> {
    let raw = row
        .try_get::<Option<String>, _>(index)
        .map_err(|_| corrupt("file_id"))?;
    raw.map(|value| FileId::parse(&value).map_err(|_| corrupt("file_id")))
        .transpose()
}

fn parse_optional_symbol_id(
    row: &sqlx_postgres::PgRow,
    index: usize,
) -> Result<Option<SymbolId>, StorageError> {
    let raw = row
        .try_get::<Option<String>, _>(index)
        .map_err(|_| corrupt("symbol_id"))?;
    raw.map(|value| SymbolId::parse(&value).map_err(|_| corrupt("symbol_id")))
        .transpose()
}

fn parse_document_id(row: &sqlx_postgres::PgRow, index: usize) -> Result<DocumentId, StorageError> {
    let raw = read_stored_string(row, index, "document_id")?;
    DocumentId::parse(&raw).map_err(|_| corrupt("document_id"))
}

const fn corrupt(field: &'static str) -> StorageError {
    stored_value_error(field)
}

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
        assert!(components.windows(2).all(|pair| pair[0] < pair[1]));
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
