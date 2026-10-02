use std::{
    fmt::Display,
    mem,
    time::{Duration, Instant},
};

use cartograph_config::DatabaseSchema;
use cartograph_domain::{GenerationId, ProjectId};
use sqlx_postgres::{PgConnection, PgCopyIn};

use crate::{StorageError, database::quoted_schema, generation::PrepareGenerationProgress};

use super::model::{
    CanonicalGenerationFacts, CanonicalSearchDocument, EdgeInput, FileInput, NumericalSiteInput,
    ReferenceInput, SymbolInput, ValidatedFactTables,
};

const COPY_CHUNK_BYTES: usize = 1024 * 1024;
const COPY_STATEMENT_BYTES: usize = 64 * 1024 * 1024;
const COPY_STATEMENT_ROWS: usize = 100_000;
const COPY_ABORT_MESSAGE: &str = "Cartograph v2 COPY stream aborted";
const ASCII_BACKSPACE: u8 = 0x08;
const ASCII_VERTICAL_TAB: u8 = 0x0b;
const ASCII_FORM_FEED: u8 = 0x0c;

/// One generation COPY attempt with its optional prepare-progress channel.
pub(crate) struct CopyGenerationRequest<'request> {
    pub(crate) context: CopyGenerationContext,
    pub(crate) prepare_progress: Option<&'request PrepareGenerationProgress>,
}

/// Validated schema and generation identity repeated in every copied row.
#[derive(Clone)]
pub(crate) struct CopyGenerationContext {
    pub(crate) schema: DatabaseSchema,
    pub(crate) project_id: ProjectId,
    pub(crate) generation_id: GenerationId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CopyTableDurations {
    pub(crate) files: Duration,
    pub(crate) symbols: Duration,
    pub(crate) edges: Duration,
    pub(crate) references: Duration,
    pub(crate) numerical_sites: Duration,
    pub(crate) documents: Duration,
}

pub(crate) struct CopyGenerationAttempt {
    pub(crate) durations: CopyTableDurations,
    pub(crate) result: Result<(), StorageError>,
}

#[derive(Clone, Copy)]
enum CopiedTable {
    Files,
    Symbols,
    Edges,
    References,
    NumericalSites,
    Documents,
}

struct CopyGenerationProgress<'a> {
    connection: &'a mut PgConnection,
    context: CopyGenerationContext,
    durations: CopyTableDurations,
    result: Result<(), StorageError>,
    prepare_progress: Option<&'a PrepareGenerationProgress>,
}

struct CopyTableRequest<T, Encode> {
    rows: Vec<T>,
    layout: CopyTableLayout,
    encode: Encode,
}

impl<T, Encode> CopyTableRequest<T, Encode> {
    fn new(rows: Vec<T>, layout: CopyTableLayout, encode: Encode) -> Self {
        Self {
            rows,
            layout,
            encode,
        }
    }
}

impl CopyTableDurations {
    fn record(&mut self, table: CopiedTable, duration: Duration) {
        match table {
            CopiedTable::Files => self.files = duration,
            CopiedTable::Symbols => self.symbols = duration,
            CopiedTable::Edges => self.edges = duration,
            CopiedTable::References => self.references = duration,
            CopiedTable::NumericalSites => self.numerical_sites = duration,
            CopiedTable::Documents => self.documents = duration,
        }
    }
}

impl<'a> CopyGenerationProgress<'a> {
    fn new(
        connection: &'a mut PgConnection,
        context: CopyGenerationContext,
        prepare_progress: Option<&'a PrepareGenerationProgress>,
    ) -> Self {
        Self {
            connection,
            context,
            durations: CopyTableDurations::default(),
            result: Ok(()),
            prepare_progress,
        }
    }

    async fn copy<T, Encode>(&mut self, request: CopyTableRequest<T, Encode>)
    where
        T: Send,
        Encode: Fn(&CopyGenerationContext, &T) -> Vec<u8> + Copy + Send,
    {
        if self.result.is_err() {
            return;
        }
        let CopyTableRequest {
            rows,
            layout,
            encode,
        } = request;
        let table = layout.copied_table;
        let plan = CopyTablePlan {
            batch: CopyBatch {
                context: self.context.clone(),
                rows,
            },
            layout,
            encode,
        };
        let (duration, result) =
            timed_copy_table(self.connection, plan, self.prepare_progress).await;
        self.durations.record(table, duration);
        if let Err(error) = result {
            self.result = Err(error);
        }
    }

    fn finish(self) -> CopyGenerationAttempt {
        CopyGenerationAttempt {
            durations: self.durations,
            result: self.result,
        }
    }
}

struct CopyTableSpec {
    statement: String,
    operation: &'static str,
}

struct CopyStatementBuffer {
    rows: Vec<Vec<u8>>,
    bytes: usize,
    maximum_rows: usize,
    maximum_bytes: usize,
}

impl CopyStatementBuffer {
    fn new(maximum_rows: usize, maximum_bytes: usize) -> Self {
        Self {
            rows: Vec::new(),
            bytes: 0,
            maximum_rows,
            maximum_bytes,
        }
    }

    fn push(&mut self, row: Vec<u8>) -> Option<Vec<Vec<u8>>> {
        let completed = if !self.rows.is_empty()
            && (self.rows.len() >= self.maximum_rows
                || self.bytes.saturating_add(row.len()) > self.maximum_bytes)
        {
            self.take()
        } else {
            None
        };
        self.bytes = self.bytes.saturating_add(row.len());
        self.rows.push(row);
        completed
    }

    fn finish(&mut self) -> Option<Vec<Vec<u8>>> {
        self.take()
    }

    fn take(&mut self) -> Option<Vec<Vec<u8>>> {
        if self.rows.is_empty() {
            return None;
        }
        self.bytes = 0;
        Some(mem::take(&mut self.rows))
    }
}

#[derive(Clone, Copy)]
struct CopyTableLayout {
    copied_table: CopiedTable,
    table: &'static str,
    columns: &'static str,
    operation: &'static str,
}

const FILES_LAYOUT: CopyTableLayout = CopyTableLayout {
    copied_table: CopiedTable::Files,
    table: "files",
    columns: "project_id, generation_id, file_id, normalized_path, language, content_hash, byte_size, parse_status",
    operation: "copy-files",
};
const SYMBOLS_LAYOUT: CopyTableLayout = CopyTableLayout {
    copied_table: CopiedTable::Symbols,
    table: "symbols",
    columns: "project_id, generation_id, symbol_id, file_id, symbol_kind, qualified_name, signature, start_byte, end_byte, start_line, end_line, structural_digest, visibility, exported, default_export, async_symbol, static_member, declaration_only, betweenness, pagerank",
    operation: "copy-symbols",
};
const EDGES_LAYOUT: CopyTableLayout = CopyTableLayout {
    copied_table: CopiedTable::Edges,
    table: "edges",
    columns: "project_id, generation_id, source_symbol_id, target_symbol_id, edge_kind, confidence, provenance, site_count",
    operation: "copy-edges",
};
const REFERENCES_LAYOUT: CopyTableLayout = CopyTableLayout {
    copied_table: CopiedTable::References,
    table: "references",
    columns: "project_id, generation_id, file_id, owner_symbol_id, target_symbol_id, reference_name, reference_kind, start_byte, end_byte, confidence, resolution_provenance, site_count, span_precision",
    operation: "copy-references",
};
const NUMERICAL_SITES_LAYOUT: CopyTableLayout = CopyTableLayout {
    copied_table: CopiedTable::NumericalSites,
    table: "numerical_sites",
    columns: "project_id, generation_id, numerical_site_id, file_id, owner_symbol_id, start_byte, end_byte, start_line, end_line, operation, hazard, precision, expression_digest, confidence_ppm, provenance, evidence_level, unknowns",
    operation: "copy-numerical-sites",
};
const DOCUMENTS_LAYOUT: CopyTableLayout = CopyTableLayout {
    copied_table: CopiedTable::Documents,
    table: "search_documents",
    columns: "project_id, generation_id, document_id, file_id, symbol_id, path, language, document_kind, qualified_name, code, natural_text, metadata, metadata_json",
    operation: "copy-search-documents",
};

struct CopyBatch<T> {
    context: CopyGenerationContext,
    rows: Vec<T>,
}

struct CopyTablePlan<T, Encode> {
    batch: CopyBatch<T>,
    layout: CopyTableLayout,
    encode: Encode,
}

pub(crate) async fn copy_generation_facts(
    connection: &mut PgConnection,
    request: CopyGenerationRequest<'_>,
    facts: CanonicalGenerationFacts,
) -> CopyGenerationAttempt {
    let CopyGenerationRequest {
        context,
        prepare_progress,
    } = request;
    let ValidatedFactTables {
        files,
        symbols,
        edges,
        references,
        numerical_sites,
        documents,
    } = facts.tables;
    let mut progress = CopyGenerationProgress::new(connection, context, prepare_progress);
    progress
        .copy(CopyTableRequest::new(files, FILES_LAYOUT, encode_file))
        .await;
    progress
        .copy(CopyTableRequest::new(
            symbols,
            SYMBOLS_LAYOUT,
            encode_symbol,
        ))
        .await;
    progress
        .copy(CopyTableRequest::new(edges, EDGES_LAYOUT, encode_edge))
        .await;
    progress
        .copy(CopyTableRequest::new(
            references,
            REFERENCES_LAYOUT,
            encode_reference,
        ))
        .await;
    progress
        .copy(CopyTableRequest::new(
            numerical_sites,
            NUMERICAL_SITES_LAYOUT,
            encode_numerical_site,
        ))
        .await;
    progress
        .copy(CopyTableRequest::new(
            documents,
            DOCUMENTS_LAYOUT,
            encode_document,
        ))
        .await;
    progress.finish()
}

async fn timed_copy_table<T, Encode>(
    connection: &mut PgConnection,
    plan: CopyTablePlan<T, Encode>,
    prepare_progress: Option<&PrepareGenerationProgress>,
) -> (Duration, Result<(), StorageError>)
where
    T: Send,
    Encode: Fn(&CopyGenerationContext, &T) -> Vec<u8> + Copy + Send,
{
    let started = Instant::now();
    let result = copy_table(connection, plan, prepare_progress).await;
    (started.elapsed(), result)
}

async fn copy_table<T, Encode>(
    connection: &mut PgConnection,
    plan: CopyTablePlan<T, Encode>,
    prepare_progress: Option<&PrepareGenerationProgress>,
) -> Result<(), StorageError>
where
    T: Send,
    Encode: Fn(&CopyGenerationContext, &T) -> Vec<u8> + Copy + Send,
{
    let CopyTablePlan {
        batch,
        layout,
        encode,
    } = plan;
    let schema = quoted_schema(&batch.context.schema);
    let context = batch.context;
    let table = layout.table;
    let columns = layout.columns;
    let spec = CopyTableSpec {
        statement: format!(r#"COPY {schema}."{table}" ({columns}) FROM STDIN WITH (FORMAT text)"#),
        operation: layout.operation,
    };
    let mut statements = CopyStatementBuffer::new(COPY_STATEMENT_ROWS, COPY_STATEMENT_BYTES);
    for row in batch.rows {
        if let Some(rows) = statements.push(encode(&context, &row)) {
            copy_text_rows(connection, &spec, rows).await?;
            advance_prepare_progress(prepare_progress)?;
        }
    }
    if let Some(rows) = statements.finish() {
        copy_text_rows(connection, &spec, rows).await?;
        advance_prepare_progress(prepare_progress)?;
    }
    Ok(())
}

fn advance_prepare_progress(
    progress: Option<&PrepareGenerationProgress>,
) -> Result<(), StorageError> {
    progress.map_or(Ok(()), PrepareGenerationProgress::advance)
}

async fn copy_text_rows<I>(
    connection: &mut PgConnection,
    spec: &CopyTableSpec,
    rows: I,
) -> Result<(), StorageError>
where
    I: IntoIterator<Item = Vec<u8>> + Send,
    I::IntoIter: Send,
{
    let rows = rows.into_iter();
    let (minimum_rows, maximum_rows) = rows.size_hint();
    let expected_rows = maximum_rows.filter(|upper| *upper == minimum_rows);
    let Some(expected_rows) = expected_rows else {
        return Err(database_error("copy-row-count-unknown"));
    };
    if expected_rows == 0 {
        return Ok(());
    }
    let expected_rows =
        u64::try_from(expected_rows).map_err(|_| database_error("copy-row-count-overflow"))?;
    copy_text_rows_counted(
        connection,
        CountedCopyRequest {
            statement: &spec.statement,
            operation: spec.operation,
            expected_rows,
        },
        rows.map(Ok),
    )
    .await
}

/// Exact statement, audit label, and admitted row count for one COPY stream.
#[derive(Clone, Copy)]
pub(crate) struct CountedCopyRequest<'request> {
    pub(crate) statement: &'request str,
    pub(crate) operation: &'static str,
    pub(crate) expected_rows: u64,
}

async fn copy_text_rows_counted<I>(
    connection: &mut PgConnection,
    request: CountedCopyRequest<'_>,
    rows: I,
) -> Result<(), StorageError>
where
    I: IntoIterator<Item = Result<Vec<u8>, StorageError>> + Send,
    I::IntoIter: Send,
{
    if request.expected_rows == 0 {
        return Ok(());
    }
    let mut copy = CountedTextCopy::begin(connection, request).await?;
    for row in rows {
        copy.push(row).await?;
    }
    copy.finish().await
}

pub(crate) struct CountedTextCopy<'connection> {
    copy: Option<PgCopyIn<&'connection mut PgConnection>>,
    operation: &'static str,
    expected_rows: u64,
    streamed_rows: u64,
    chunk: Vec<u8>,
}

impl<'connection> CountedTextCopy<'connection> {
    pub(crate) async fn begin(
        connection: &'connection mut PgConnection,
        request: CountedCopyRequest<'_>,
    ) -> Result<Self, StorageError> {
        let CountedCopyRequest {
            statement,
            operation,
            expected_rows,
        } = request;
        if expected_rows == 0 {
            return Err(StorageError::InvalidInput {
                field: "copy_expected_rows",
            });
        }
        // The only dynamic identifier is a validated, double-quoted
        // DatabaseSchema; table names and column lists are compile-time literals.
        let copy = connection
            .copy_in_raw(statement)
            .await
            .map_err(|_| database_error(operation))?;
        Ok(Self {
            copy: Some(copy),
            operation,
            expected_rows,
            streamed_rows: 0,
            chunk: Vec::with_capacity(COPY_CHUNK_BYTES),
        })
    }

    pub(crate) async fn push(
        &mut self,
        row: Result<Vec<u8>, StorageError>,
    ) -> Result<(), StorageError> {
        let row = match row {
            Ok(row) => row,
            Err(error) => {
                self.abort().await;
                return Err(error);
            }
        };
        if self.streamed_rows >= self.expected_rows {
            self.abort().await;
            return Err(database_error("copy-row-count-mismatch"));
        }
        if !self.chunk.is_empty() && self.chunk.len().saturating_add(row.len()) > COPY_CHUNK_BYTES {
            self.flush_chunk().await?;
        }
        if row.len() > COPY_CHUNK_BYTES {
            self.send(row.as_slice()).await?;
        } else {
            self.chunk.extend_from_slice(&row);
        }
        self.streamed_rows = self.streamed_rows.saturating_add(1);
        Ok(())
    }

    pub(crate) async fn finish(mut self) -> Result<(), StorageError> {
        if self.streamed_rows != self.expected_rows {
            self.abort().await;
            return Err(database_error("copy-row-count-mismatch"));
        }
        self.flush_chunk().await?;
        let copy = self
            .copy
            .take()
            .ok_or_else(|| database_error(self.operation))?;
        let copied = copy
            .finish()
            .await
            .map_err(|_| database_error(self.operation))?;
        if copied != self.expected_rows {
            return Err(database_error("copy-row-count-mismatch"));
        }
        Ok(())
    }

    async fn flush_chunk(&mut self) -> Result<(), StorageError> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let mut chunk = mem::take(&mut self.chunk);
        let result = self.send(chunk.as_slice()).await;
        chunk.clear();
        self.chunk = chunk;
        result
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        let result = match self.copy.as_mut() {
            Some(copy) => copy
                .send(bytes)
                .await
                .map(|_| ())
                .map_err(|_| database_error(self.operation)),
            None => Err(database_error(self.operation)),
        };
        if result.is_err() {
            self.abort().await;
        }
        result
    }

    async fn abort(&mut self) {
        if let Some(copy) = self.copy.take() {
            let _ = copy.abort(COPY_ABORT_MESSAGE).await;
        }
    }
}

fn encode_file(context: &CopyGenerationContext, file: &FileInput) -> Vec<u8> {
    let mut row = TextRow::new();
    row.text(context.project_id.as_str());
    row.text(context.generation_id.as_str());
    row.text(file.file_id.as_str());
    row.text(&file.normalized_path);
    row.text(&file.language);
    row.text(file.content_hash.as_str());
    row.number(file.byte_size);
    row.text(file.parse_status.as_str());
    row.finish()
}

fn encode_symbol(context: &CopyGenerationContext, symbol: &SymbolInput) -> Vec<u8> {
    let mut row = TextRow::new();
    row.text(context.project_id.as_str());
    row.text(context.generation_id.as_str());
    row.text(symbol.symbol_id.as_str());
    row.text(symbol.file_id.as_str());
    row.text(&symbol.symbol_kind);
    row.text(&symbol.qualified_name);
    row.text(&symbol.signature);
    row.number(symbol.start_byte);
    row.number(symbol.end_byte);
    row.number(symbol.start_line);
    row.number(symbol.end_line);
    row.text(symbol.structural_digest.as_str());
    row.optional_text(symbol.visibility.map(cartograph_domain::Visibility::as_str));
    row.number(symbol.export.exported);
    row.number(symbol.export.default_export);
    row.number(symbol.execution.async_symbol);
    row.number(symbol.execution.static_member);
    row.number(symbol.declaration_only);
    row.optional_number(
        symbol
            .betweenness_ppb
            .map(|score| f64::from(score) / 1_000_000_000.0),
    );
    row.optional_number(
        symbol
            .pagerank_ppb
            .map(|score| f64::from(score) / 1_000_000_000.0),
    );
    row.finish()
}

fn encode_edge(context: &CopyGenerationContext, edge: &EdgeInput) -> Vec<u8> {
    let mut row = TextRow::new();
    row.text(context.project_id.as_str());
    row.text(context.generation_id.as_str());
    row.text(edge.source_symbol_id.as_str());
    row.text(edge.target_symbol_id.as_str());
    row.text(edge.kind.as_str());
    row.number(edge.confidence);
    row.text(&edge.provenance);
    row.number(edge.site_count);
    row.finish()
}

fn encode_reference(context: &CopyGenerationContext, reference: &ReferenceInput) -> Vec<u8> {
    let mut row = TextRow::new();
    row.text(context.project_id.as_str());
    row.text(context.generation_id.as_str());
    row.text(reference.file_id.as_str());
    row.optional_text(
        reference
            .owner_symbol_id
            .as_ref()
            .map(cartograph_domain::SymbolId::as_str),
    );
    row.optional_text(
        reference
            .target_symbol_id
            .as_ref()
            .map(cartograph_domain::SymbolId::as_str),
    );
    row.text(&reference.reference_name);
    row.text(&reference.reference_kind);
    row.number(reference.start_byte);
    row.number(reference.end_byte);
    row.number(reference.confidence);
    row.text(&reference.resolution_provenance);
    row.number(reference.site_count);
    row.text(reference.span_precision.as_str());
    row.finish()
}

fn encode_numerical_site(context: &CopyGenerationContext, site: &NumericalSiteInput) -> Vec<u8> {
    let mut row = TextRow::new();
    row.text(context.project_id.as_str());
    row.text(context.generation_id.as_str());
    row.text(site.site_id.as_str());
    row.text(site.file_id.as_str());
    row.optional_text(
        site.owner_symbol_id
            .as_ref()
            .map(cartograph_domain::SymbolId::as_str),
    );
    row.number(site.start_byte);
    row.number(site.end_byte);
    row.number(site.start_line);
    row.number(site.end_line);
    row.text(&site.operation);
    row.text(&site.hazard);
    row.text(&site.precision);
    row.text(site.expression_digest.as_str());
    row.number(site.confidence_ppm);
    row.text(&site.provenance);
    row.text(&site.evidence_level);
    row.text(&site.unknowns);
    row.finish()
}

fn encode_document(context: &CopyGenerationContext, document: &CanonicalSearchDocument) -> Vec<u8> {
    let mut row = TextRow::new();
    row.text(context.project_id.as_str());
    row.text(context.generation_id.as_str());
    row.text(document.document_id.as_str());
    row.optional_text(
        document
            .file_id
            .as_ref()
            .map(cartograph_domain::FileId::as_str),
    );
    row.optional_text(
        document
            .symbol_id
            .as_ref()
            .map(cartograph_domain::SymbolId::as_str),
    );
    row.text(&document.path);
    row.text(&document.language);
    row.text(document.kind.as_str());
    row.text(&document.qualified_name);
    row.text(&document.code);
    row.text(&document.natural_text);
    row.text(&document.metadata_json);
    row.text(&document.metadata_json);
    row.finish()
}

pub(crate) struct TextRow {
    bytes: Vec<u8>,
    needs_delimiter: bool,
}

impl TextRow {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            needs_delimiter: false,
        }
    }

    pub(crate) fn text(&mut self, value: &str) {
        self.delimiter();
        escape_text(value, &mut self.bytes);
    }

    pub(crate) fn optional_text(&mut self, value: Option<&str>) {
        if let Some(value) = value {
            self.text(value);
        } else {
            self.delimiter();
            self.bytes.extend_from_slice(br"\N");
        }
    }

    pub(crate) fn number(&mut self, value: impl Display) {
        self.text(&value.to_string());
    }

    pub(crate) fn optional_number(&mut self, value: Option<impl Display>) {
        if let Some(value) = value {
            self.number(value);
        } else {
            self.delimiter();
            self.bytes.extend_from_slice(br"\N");
        }
    }

    fn delimiter(&mut self) {
        if self.needs_delimiter {
            self.bytes.push(b'\t');
        }
        self.needs_delimiter = true;
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.bytes.push(b'\n');
        self.bytes
    }
}

fn escape_text(value: &str, output: &mut Vec<u8>) {
    for byte in value.bytes() {
        match byte {
            b'\\' => output.extend_from_slice(br"\\"),
            b'\t' => output.extend_from_slice(br"\t"),
            b'\n' => output.extend_from_slice(br"\n"),
            b'\r' => output.extend_from_slice(br"\r"),
            ASCII_BACKSPACE => output.extend_from_slice(br"\b"),
            ASCII_VERTICAL_TAB => output.extend_from_slice(br"\v"),
            ASCII_FORM_FEED => output.extend_from_slice(br"\f"),
            _ => output.push(byte),
        }
    }
}

const fn database_error(operation: &'static str) -> StorageError {
    StorageError::DatabaseOperation { operation }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_copy_encoding_distinguishes_null_and_escaped_data() {
        let mut row = TextRow::new();
        row.text("plain");
        row.text("tab\tline\nreturn\rslash\\null-marker\\N");
        row.optional_text(None);
        row.optional_text(Some(""));

        assert_eq!(
            row.finish(),
            b"plain\ttab\\tline\\nreturn\\rslash\\\\null-marker\\\\N\t\\N\t\n"
        );
    }

    #[test]
    fn copy_statement_buffer_batches_by_rows_and_encoded_bytes_without_reordering() {
        let mut buffer = CopyStatementBuffer::new(3, 8);
        let mut completed = Vec::new();
        for row in [
            b"aa".to_vec(),
            b"bbb".to_vec(),
            b"c".to_vec(),
            b"dddd".to_vec(),
        ] {
            if let Some(rows) = buffer.push(row) {
                completed.push(rows);
            }
        }
        if let Some(rows) = buffer.finish() {
            completed.push(rows);
        }

        assert_eq!(
            completed,
            vec![
                vec![b"aa".to_vec(), b"bbb".to_vec(), b"c".to_vec()],
                vec![b"dddd".to_vec()]
            ]
        );

        let mut byte_bounded = CopyStatementBuffer::new(10, 5);
        assert!(byte_bounded.push(b"1234".to_vec()).is_none());
        assert_eq!(
            byte_bounded.push(b"56".to_vec()),
            Some(vec![b"1234".to_vec()])
        );
        assert_eq!(byte_bounded.finish(), Some(vec![b"56".to_vec()]));

        let mut indivisible = CopyStatementBuffer::new(10, 5);
        assert!(indivisible.push(b"123456".to_vec()).is_none());
        assert_eq!(indivisible.finish(), Some(vec![b"123456".to_vec()]));
    }
}
