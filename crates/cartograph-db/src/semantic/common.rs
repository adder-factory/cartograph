use std::{fmt::Write as _, future::Future, time::Duration};

use cartograph_config::DatabaseSchema;
use cartograph_domain::{ContentDigest, DocumentId, DocumentKind, GenerationId, ProjectId};
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe, transaction::Transaction};
use sqlx_postgres::{PgConnection, Postgres};

use crate::CartographDatabase;

use super::types::{
    EmbeddingModelSelector, EmbeddingModelState, EmbeddingNormalization, RegisteredEmbeddingModel,
    SemanticStorageError, database_error,
};

const EMBEDDING_SOURCE_DOMAIN: &[u8] = b"cartograph-v2-embedding-source-v1";
const HNSW_EF_SEARCH: &str = "200";
const HNSW_MAX_SCAN_TUPLES: &str = "100000";
const HNSW_SCAN_MEM_MULTIPLIER: &str = "8";

pub(crate) async fn begin_bounded<'a>(
    database: &'a CartographDatabase,
    timeout: Duration,
    operation: &'static str,
) -> Result<Transaction<'a, Postgres>, SemanticStorageError> {
    let mut transaction = database
        .pool
        .begin()
        .await
        .map_err(|_| database_error(operation))?;
    crate::database::set_local_statement_timeout(&mut transaction, timeout)
        .await
        .map_err(|()| database_error("semantic-statement-timeout"))?;
    Ok(transaction)
}

pub(crate) async fn rollback_error(
    transaction: Transaction<'_, Postgres>,
    error: SemanticStorageError,
    operation: &'static str,
) -> SemanticStorageError {
    match transaction.rollback().await {
        Ok(()) => error,
        Err(_) => database_error(operation),
    }
}

pub(crate) async fn configure_filtered_hnsw_scan(
    connection: &mut PgConnection,
) -> Result<(), SemanticStorageError> {
    query(
        r"SELECT set_config('hnsw.iterative_scan', 'strict_order', true),
                  set_config('hnsw.ef_search', $1, true),
                  set_config('hnsw.max_scan_tuples', $2, true),
                  set_config('hnsw.scan_mem_multiplier', $3, true)",
    )
    .bind(HNSW_EF_SEARCH)
    .bind(HNSW_MAX_SCAN_TUPLES)
    .bind(HNSW_SCAN_MEM_MULTIPLIER)
    .execute(connection)
    .await
    .map_err(|_| database_error("semantic-hnsw-scan-bounds"))?;
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct TransactionCompletion {
    commit_operation: &'static str,
    rollback_operation: &'static str,
}

impl TransactionCompletion {
    pub(crate) const fn new(
        commit_operation: &'static str,
        rollback_operation: &'static str,
    ) -> Self {
        Self {
            commit_operation,
            rollback_operation,
        }
    }
}

async fn finish_bounded<T>(
    transaction: Transaction<'_, Postgres>,
    result: Result<T, SemanticStorageError>,
    completion: TransactionCompletion,
) -> Result<T, SemanticStorageError> {
    match result {
        Ok(value) => {
            transaction
                .commit()
                .await
                .map_err(|_| database_error(completion.commit_operation))?;
            Ok(value)
        }
        Err(error) => Err(rollback_error(transaction, error, completion.rollback_operation).await),
    }
}

pub(crate) trait BoundedSemanticOperation {
    type Output;

    const BEGIN_OPERATION: &'static str;
    const COMPLETION: TransactionCompletion;

    fn database(&self) -> &CartographDatabase;
    fn statement_timeout(&self) -> Duration;
    fn execute<'connection>(
        &'connection self,
        connection: &'connection mut PgConnection,
    ) -> impl Future<Output = Result<Self::Output, SemanticStorageError>> + 'connection;
}

pub(crate) async fn execute_bounded<Operation>(
    operation: Operation,
) -> Result<Operation::Output, SemanticStorageError>
where
    Operation: BoundedSemanticOperation,
{
    let mut transaction = begin_bounded(
        operation.database(),
        operation.statement_timeout(),
        Operation::BEGIN_OPERATION,
    )
    .await?;
    let result = operation.execute(&mut transaction).await;
    finish_bounded(transaction, result, Operation::COMPLETION).await
}

pub(crate) async fn load_model(
    connection: &mut PgConnection,
    lookup: ModelLookup<'_>,
) -> Result<Option<RegisteredEmbeddingModel>, SemanticStorageError> {
    let lock = if lookup.for_update { " FOR UPDATE" } else { "" };
    let sql = format!(
        r#"SELECT model_id::text, fingerprint, provider, model_name,
                  dimension, normalization, state
            FROM {}."embedding_models"
            WHERE model_id = CAST($1 AS uuid){lock}"#,
        crate::database::quoted_schema(lookup.schema)
    );
    query(AssertSqlSafe(sql))
        .bind(lookup.model_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("load-embedding-model"))?
        .as_ref()
        .map(decode_model)
        .transpose()
}

#[derive(Clone, Copy)]
pub(crate) struct ModelLookup<'a> {
    schema: &'a DatabaseSchema,
    model_id: &'a cartograph_domain::ModelId,
    for_update: bool,
}

impl<'a> ModelLookup<'a> {
    pub(crate) const fn new(
        schema: &'a DatabaseSchema,
        model_id: &'a cartograph_domain::ModelId,
    ) -> Self {
        Self {
            schema,
            model_id,
            for_update: false,
        }
    }

    pub(crate) const fn for_update(mut self) -> Self {
        self.for_update = true;
        self
    }
}

pub(crate) fn require_selector(
    model: RegisteredEmbeddingModel,
    selector: &EmbeddingModelSelector,
) -> Result<RegisteredEmbeddingModel, SemanticStorageError> {
    if model.selector == *selector {
        Ok(model)
    } else {
        Err(SemanticStorageError::ModelMismatch)
    }
}

pub(crate) fn require_active_selector(
    model: RegisteredEmbeddingModel,
    selector: &EmbeddingModelSelector,
) -> Result<RegisteredEmbeddingModel, SemanticStorageError> {
    let model = require_selector(model, selector)?;
    if model.state == EmbeddingModelState::Retired {
        Err(SemanticStorageError::ModelRetired)
    } else {
        Ok(model)
    }
}

pub(crate) async fn lock_current_generation(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    project_id: &ProjectId,
) -> Result<GenerationId, SemanticStorageError> {
    lock_optional_current_generation(connection, schema, project_id)
        .await?
        .ok_or(SemanticStorageError::CurrentGenerationUnavailable)
}

pub(crate) async fn lock_optional_current_generation(
    connection: &mut PgConnection,
    schema: &DatabaseSchema,
    project_id: &ProjectId,
) -> Result<Option<GenerationId>, SemanticStorageError> {
    let sql = format!(
        r#"SELECT current_generation_id::text
            FROM {}."projects"
            WHERE project_id = CAST($1 AS uuid)
            FOR SHARE"#,
        crate::database::quoted_schema(schema)
    );
    let Some(row) = query(AssertSqlSafe(sql))
        .bind(project_id.as_str())
        .fetch_optional(connection)
        .await
        .map_err(|_| database_error("lock-current-generation"))?
    else {
        return Ok(None);
    };
    let raw = row
        .try_get::<Option<String>, _>(0)
        .map_err(|_| corrupt("current_generation_id"))?;
    raw.map(|value| GenerationId::parse(&value).map_err(|_| corrupt("current_generation_id")))
        .transpose()
}

pub(crate) fn require_generation(
    actual: &GenerationId,
    expected: &GenerationId,
) -> Result<(), SemanticStorageError> {
    (actual == expected).ok_or(SemanticStorageError::CurrentGenerationChanged)
}

pub(crate) fn vector_text(vector: &[f32]) -> Result<String, SemanticStorageError> {
    let estimated = vector
        .len()
        .checked_mul(16)
        .and_then(|value| value.checked_add(2))
        .ok_or_else(|| database_error("encode-vector"))?;
    let mut encoded = String::new();
    encoded
        .try_reserve(estimated)
        .map_err(|_| database_error("encode-vector"))?;
    encoded.push('[');
    for (index, value) in vector.iter().enumerate() {
        if index > 0 {
            encoded.push(',');
        }
        write!(&mut encoded, "{value}").map_err(|_| database_error("encode-vector"))?;
    }
    encoded.push(']');
    Ok(encoded)
}

pub(crate) fn validate_normalization(
    vector: &[f32],
    normalization: EmbeddingNormalization,
) -> Result<(), SemanticStorageError> {
    if normalization == EmbeddingNormalization::None {
        return Ok(());
    }
    let magnitude = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    ((magnitude - 1.0).abs() <= 0.001).ok_or(super::types::invalid("embedding_normalization"))
}

pub(crate) struct DocumentSource {
    pub(crate) row_id: u64,
    pub(crate) document_id: DocumentId,
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) kind: DocumentKind,
    pub(crate) qualified_name: String,
    pub(crate) code: String,
    pub(crate) natural_text: String,
    pub(crate) summary: String,
}

impl DocumentSource {
    pub(crate) fn encoded_length(&self) -> Result<u64, SemanticStorageError> {
        [
            &self.path,
            &self.language,
            self.kind.as_str(),
            &self.qualified_name,
            &self.code,
            &self.natural_text,
            &self.summary,
        ]
        .into_iter()
        .try_fold(0_u64, |total, value| {
            let length = u64::try_from(value.len()).map_err(|_| corrupt("document_source"))?;
            total
                .checked_add(length)
                .and_then(|sum| sum.checked_add(32))
                .ok_or_else(|| corrupt("document_source"))
        })
    }

    pub(crate) fn render(self) -> Result<(String, ContentDigest), SemanticStorageError> {
        let mut text = String::new();
        let capacity = usize::try_from(self.encoded_length()?)
            .map_err(|_| database_error("render-document-source"))?;
        text.try_reserve(capacity)
            .map_err(|_| database_error("render-document-source"))?;
        append_field(&mut text, "path", &self.path);
        append_field(&mut text, "language", &self.language);
        append_field(&mut text, "kind", self.kind.as_str());
        append_field(&mut text, "name", &self.qualified_name);
        append_field(&mut text, "code", &self.code);
        append_field(&mut text, "natural_text", &self.natural_text);
        append_field(&mut text, "summary", &self.summary);
        let mut hasher = blake3::Hasher::new();
        hasher.update(EMBEDDING_SOURCE_DOMAIN);
        hasher.update(&u64::try_from(text.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(text.as_bytes());
        Ok((
            text,
            ContentDigest::from_bytes(*hasher.finalize().as_bytes()),
        ))
    }
}

pub(crate) fn decode_document_source(
    row: &sqlx_postgres::PgRow,
) -> Result<DocumentSource, SemanticStorageError> {
    let row_id = read_u64(row, "id")?;
    let document_id =
        DocumentId::parse(&read_string(row, "document_id")?).map_err(|_| corrupt("document_id"))?;
    Ok(DocumentSource {
        row_id,
        document_id,
        path: read_string(row, "path")?,
        language: read_string(row, "language")?,
        kind: parse_document_kind(&read_string(row, "document_kind")?)?,
        qualified_name: read_string(row, "qualified_name")?,
        code: read_string(row, "code")?,
        natural_text: read_string(row, "natural_text")?,
        summary: read_string(row, "summary")?,
    })
}

fn read_u64(row: &sqlx_postgres::PgRow, column: &'static str) -> Result<u64, SemanticStorageError> {
    let value = row.try_get::<i64, _>(column).map_err(|_| corrupt(column))?;
    u64::try_from(value).map_err(|_| corrupt(column))
}

pub(crate) fn read_string(
    row: &sqlx_postgres::PgRow,
    column: &'static str,
) -> Result<String, SemanticStorageError> {
    row.try_get::<String, _>(column)
        .map_err(|_| corrupt(column))
}

fn decode_model(
    row: &sqlx_postgres::PgRow,
) -> Result<RegisteredEmbeddingModel, SemanticStorageError> {
    let model_id = cartograph_domain::ModelId::parse(&read_string(row, "model_id")?)
        .map_err(|_| corrupt("model_id"))?;
    let fingerprint = ContentDigest::parse(&read_string(row, "fingerprint")?)
        .map_err(|_| corrupt("fingerprint"))?;
    let dimension = row
        .try_get::<i32, _>("dimension")
        .ok()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| corrupt("dimension"))?;
    Ok(RegisteredEmbeddingModel {
        selector: EmbeddingModelSelector {
            model_id,
            fingerprint,
            dimension,
        },
        provider: read_string(row, "provider")?,
        model_name: read_string(row, "model_name")?,
        normalization: EmbeddingNormalization::parse(&read_string(row, "normalization")?)?,
        state: EmbeddingModelState::parse(&read_string(row, "state")?)?,
    })
}

fn append_field(output: &mut String, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(label);
    output.push_str(":\n");
    output.push_str(value);
}

pub(crate) fn parse_document_kind(value: &str) -> Result<DocumentKind, SemanticStorageError> {
    match value {
        "symbol" => Ok(DocumentKind::Symbol),
        "file" => Ok(DocumentKind::File),
        "documentation" => Ok(DocumentKind::Documentation),
        "test" => Ok(DocumentKind::Test),
        "configuration" => Ok(DocumentKind::Configuration),
        _ => Err(corrupt("document_kind")),
    }
}

pub(crate) const fn corrupt(field: &'static str) -> SemanticStorageError {
    SemanticStorageError::CorruptStoredValue { field }
}
