use cartograph_domain::{DocumentId, FileId, GenerationId, SymbolId, SymbolKind};
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};

use crate::CartographDatabase;

use super::{
    common::{
        ModelLookup, begin_bounded, corrupt, load_model, parse_document_kind, read_string,
        require_active_selector, rollback_error, validate_normalization, vector_text,
    },
    readiness::{ReadinessTarget, evaluate_readiness},
    types::{
        SemanticReadinessState, SemanticStorageError, VectorSearchHit, VectorSearchRequest,
        database_error,
    },
};

const COSINE_DISTANCE_MAXIMUM: f64 = 2.0;
const COSINE_DISTANCE_TOLERANCE: f64 = 1.0e-9;
const MAXIMUM_RERANK_CODE_CHARACTERS: i32 = 4_096;
const MAXIMUM_RERANK_NATURAL_TEXT_CHARACTERS: i32 = 2_048;
const MAXIMUM_RERANK_TEXT_BYTES: usize = 32 * 1_024;

impl CartographDatabase {
    /// Execute current-generation cosine Top-K only after every semantic readiness proof passes.
    /// # Errors
    ///
    /// Returns an error if semantic readiness or the expected-generation fence
    /// fails, or the dimension-fixed cosine query/result decoding cannot complete.
    pub async fn vector_top_k(
        &self,
        request: VectorSearchRequest,
    ) -> Result<Vec<VectorSearchHit>, SemanticStorageError> {
        let mut transaction =
            begin_bounded(self, request.statement_timeout, "vector-search-begin").await?;
        let result = vector_search_transaction(&mut transaction, self, &request).await;
        match result {
            Ok(hits) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| database_error("vector-search-commit"))?;
                Ok(hits)
            }
            Err(error) => Err(rollback_error(transaction, error, "vector-search-rollback").await),
        }
    }
}

async fn vector_search_transaction(
    connection: &mut sqlx_postgres::PgConnection,
    database: &CartographDatabase,
    request: &VectorSearchRequest,
) -> Result<Vec<VectorSearchHit>, SemanticStorageError> {
    let readiness = evaluate_readiness(
        connection,
        ReadinessTarget::new(&database.schema, &request.project_id, &request.model),
    )
    .await?;
    if readiness.state != SemanticReadinessState::Ready {
        return Err(SemanticStorageError::NotReady {
            state: readiness.state,
        });
    }
    let model = load_model(
        connection,
        ModelLookup::new(&database.schema, request.model.model_id()),
    )
    .await?
    .ok_or(SemanticStorageError::ModelNotFound)?;
    let model = require_active_selector(model, &request.model)?;
    validate_normalization(&request.vector, model.normalization)?;
    let generation = readiness
        .generation_id()
        .ok_or(SemanticStorageError::CurrentGenerationUnavailable)?;
    if generation != &request.expected_generation_id {
        return Err(SemanticStorageError::CurrentGenerationChanged);
    }
    let input = VectorExecutionInput {
        database,
        request,
        generation,
    };
    let hits = execute_vector_search(connection, input).await?;
    let expected = u64::from(request.limit).min(readiness.documents());
    let expected = usize::try_from(expected).map_err(|_| corrupt("vector_search_cardinality"))?;
    if hits.len() == expected {
        return Ok(hits);
    }
    let exact = execute_exact_vector_search(connection, input).await?;
    if exact.len() == expected {
        Ok(exact)
    } else {
        Err(corrupt("vector_search_cardinality"))
    }
}

#[derive(Clone, Copy)]
struct VectorExecutionInput<'a> {
    database: &'a CartographDatabase,
    request: &'a VectorSearchRequest,
    generation: &'a GenerationId,
}

async fn execute_exact_vector_search(
    connection: &mut sqlx_postgres::PgConnection,
    input: VectorExecutionInput<'_>,
) -> Result<Vec<VectorSearchHit>, SemanticStorageError> {
    let schema = crate::database::quoted_schema(&input.database.schema);
    let model_id = input.request.model.model_id().as_str();
    let dimension = input.request.model.dimension();
    let sql = format!(
        r#"WITH current_embeddings AS MATERIALIZED (
                SELECT embeddings.project_id, embeddings.generation_id,
                       embeddings.document_id,
                       embeddings.embedding::vector({dimension}) AS embedding
                FROM {schema}."document_embeddings" AS embeddings
                WHERE embeddings.project_id = CAST($1 AS uuid)
                  AND embeddings.generation_id = CAST($2 AS uuid)
                  AND embeddings.model_id = '{model_id}'::uuid
            )
            SELECT documents.generation_id::text, documents.document_id::text,
                  documents.file_id::text, documents.symbol_id::text,
                  documents.path, documents.language, documents.document_kind,
                  documents.qualified_name,
                  (
                      SELECT symbols.symbol_kind
                      FROM {schema}."symbols" AS symbols
                      WHERE symbols.project_id = documents.project_id
                        AND symbols.generation_id = documents.generation_id
                        AND symbols.symbol_id = documents.symbol_id
                  ) AS symbol_kind,
                  left(documents.code, $5) AS rerank_code,
                  left(documents.natural_text, $6) AS rerank_natural_text,
                  (current_embeddings.embedding
                      <=> CAST($3 AS vector({dimension})))::float8 AS distance
            FROM current_embeddings
            INNER JOIN {schema}."search_documents" AS documents
              ON documents.project_id = current_embeddings.project_id
             AND documents.generation_id = current_embeddings.generation_id
             AND documents.document_id = current_embeddings.document_id
            ORDER BY current_embeddings.embedding <=> CAST($3 AS vector({dimension})),
                     documents.id
            LIMIT $4"#
    );
    let rows = query(AssertSqlSafe(sql))
        .bind(input.request.project_id.as_str())
        .bind(input.generation.as_str())
        .bind(vector_text(&input.request.vector)?)
        .bind(i64::from(input.request.limit))
        .bind(MAXIMUM_RERANK_CODE_CHARACTERS)
        .bind(MAXIMUM_RERANK_NATURAL_TEXT_CHARACTERS)
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("vector-search-exact-fallback"))?;
    rows.iter().map(decode_rerankable_hit).collect()
}

async fn execute_vector_search(
    connection: &mut sqlx_postgres::PgConnection,
    input: VectorExecutionInput<'_>,
) -> Result<Vec<VectorSearchHit>, SemanticStorageError> {
    let schema = crate::database::quoted_schema(&input.database.schema);
    let model_id = input.request.model.model_id().as_str();
    let dimension = input.request.model.dimension();
    let sql = format!(
        r#"WITH nearest AS MATERIALIZED (
                SELECT embeddings.project_id, embeddings.generation_id,
                       embeddings.document_id,
                       (embeddings.embedding::vector({dimension})
                           <=> CAST($3 AS vector({dimension})))::float8 AS distance
                FROM {schema}."document_embeddings" AS embeddings
                WHERE embeddings.project_id = CAST($1 AS uuid)
                  AND embeddings.generation_id = CAST($2 AS uuid)
                  AND embeddings.model_id = '{model_id}'::uuid
                ORDER BY embeddings.embedding::vector({dimension})
                             <=> CAST($3 AS vector({dimension}))
                LIMIT $4
            )
            SELECT documents.generation_id::text, documents.document_id::text,
                  documents.file_id::text, documents.symbol_id::text,
                  documents.path, documents.language, documents.document_kind,
                  documents.qualified_name,
                  (
                      SELECT symbols.symbol_kind
                      FROM {schema}."symbols" AS symbols
                      WHERE symbols.project_id = documents.project_id
                        AND symbols.generation_id = documents.generation_id
                        AND symbols.symbol_id = documents.symbol_id
                  ) AS symbol_kind,
                  left(documents.code, $5) AS rerank_code,
                  left(documents.natural_text, $6) AS rerank_natural_text,
                  nearest.distance
            FROM nearest
            INNER JOIN {schema}."search_documents" AS documents
              ON documents.project_id = nearest.project_id
             AND documents.generation_id = nearest.generation_id
             AND documents.document_id = nearest.document_id
            ORDER BY nearest.distance, documents.id"#
    );
    let rows = query(AssertSqlSafe(sql))
        .bind(input.request.project_id.as_str())
        .bind(input.generation.as_str())
        .bind(vector_text(&input.request.vector)?)
        .bind(i64::from(input.request.limit))
        .bind(MAXIMUM_RERANK_CODE_CHARACTERS)
        .bind(MAXIMUM_RERANK_NATURAL_TEXT_CHARACTERS)
        .fetch_all(connection)
        .await
        .map_err(|_| database_error("vector-search-query"))?;
    rows.iter().map(decode_rerankable_hit).collect()
}

fn decode_rerankable_hit(
    row: &sqlx_postgres::PgRow,
) -> Result<VectorSearchHit, SemanticStorageError> {
    let mut hit = decode_hit(row)?;
    let code = read_string(row, "rerank_code")?;
    let natural_text = read_string(row, "rerank_natural_text")?;
    hit.rerank_text = Some(render_rerank_text(&hit, &code, &natural_text)?);
    Ok(hit)
}

pub(crate) fn decode_hit(
    row: &sqlx_postgres::PgRow,
) -> Result<VectorSearchHit, SemanticStorageError> {
    let generation_id = GenerationId::parse(&read_string(row, "generation_id")?)
        .map_err(|_| corrupt("generation_id"))?;
    let document_id =
        DocumentId::parse(&read_string(row, "document_id")?).map_err(|_| corrupt("document_id"))?;
    let file_id = parse_optional_id(
        row.try_get::<Option<String>, _>("file_id")
            .map_err(|_| corrupt("file_id"))?,
        FileId::parse,
        "file_id",
    )?;
    let symbol_id = parse_optional_id(
        row.try_get::<Option<String>, _>("symbol_id")
            .map_err(|_| corrupt("symbol_id"))?,
        SymbolId::parse,
        "symbol_id",
    )?;
    let distance = row
        .try_get::<f64, _>("distance")
        .map_err(|_| corrupt("distance"))?;
    if !distance.is_finite()
        || !(-COSINE_DISTANCE_TOLERANCE..=COSINE_DISTANCE_MAXIMUM + COSINE_DISTANCE_TOLERANCE)
            .contains(&distance)
    {
        return Err(corrupt("distance"));
    }
    let distance = distance.clamp(0.0, COSINE_DISTANCE_MAXIMUM);
    Ok(VectorSearchHit {
        generation_id,
        document_id,
        file_id,
        symbol_id,
        path: read_string(row, "path")?,
        language: read_string(row, "language")?,
        document_kind: parse_document_kind(&read_string(row, "document_kind")?)?,
        qualified_name: read_string(row, "qualified_name")?,
        symbol_kind: row
            .try_get::<Option<String>, _>("symbol_kind")
            .map_err(|_| corrupt("symbol_kind"))?
            .map(|value| SymbolKind::from_stable_str(&value).ok_or_else(|| corrupt("symbol_kind")))
            .transpose()?,
        distance,
        rerank_text: None,
    })
}

fn render_rerank_text(
    hit: &VectorSearchHit,
    code: &str,
    natural_text: &str,
) -> Result<String, SemanticStorageError> {
    let mut text = String::new();
    text.try_reserve(MAXIMUM_RERANK_TEXT_BYTES)
        .map_err(|_| database_error("rerank-text-reserve"))?;
    for (label, value) in [
        ("path", hit.path.as_str()),
        ("language", hit.language.as_str()),
        ("kind", hit.document_kind.as_str()),
        ("name", hit.qualified_name.as_str()),
        ("code", code),
        ("natural_text", natural_text),
    ] {
        append_rerank_field(&mut text, label, value);
        if text.len() == MAXIMUM_RERANK_TEXT_BYTES {
            break;
        }
    }
    Ok(text)
}

fn append_rerank_field(text: &mut String, label: &str, value: &str) {
    if value.is_empty() || text.len() == MAXIMUM_RERANK_TEXT_BYTES {
        return;
    }
    let separator = if text.is_empty() { "" } else { "\n" };
    let prefix = format!("{separator}{label}:\n");
    let prefix = bounded_utf8(
        &prefix,
        MAXIMUM_RERANK_TEXT_BYTES.saturating_sub(text.len()),
    );
    text.push_str(prefix);
    let remaining = MAXIMUM_RERANK_TEXT_BYTES.saturating_sub(text.len());
    text.push_str(bounded_utf8(value, remaining));
}

fn bounded_utf8(value: &str, maximum_bytes: usize) -> &str {
    &value[..value.floor_char_boundary(maximum_bytes)]
}

fn parse_optional_id<T>(
    raw: Option<String>,
    parse: impl FnOnce(&str) -> Result<T, cartograph_domain::InvalidId>,
    field: &'static str,
) -> Result<Option<T>, SemanticStorageError> {
    raw.map(|value| parse(&value).map_err(|_| corrupt(field)))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_utf8_never_splits_a_scalar() {
        assert_eq!(bounded_utf8("a😀b", 3), "a");
        assert_eq!(bounded_utf8("a😀b", 5), "a😀");
        assert_eq!(bounded_utf8("small", 8), "small");
    }
}
