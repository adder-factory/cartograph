use std::mem::size_of;

use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, FileParseStatus,
    GenerationDigestVersion, NumericalSiteId, SymbolExecutionFlags, SymbolExportFlags, SymbolId,
    Visibility,
};
use serde::Serialize;
use serde_json::Value;

/// One source-file fact staged into an immutable generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileInput {
    /// Stable identity derived independently of worker scheduling.
    pub file_id: FileId,
    /// Project-normalized source path.
    pub normalized_path: String,
    /// Normalized language identifier.
    pub language: String,
    /// Digest of the complete source bytes.
    pub content_hash: ContentDigest,
    /// Original source size in bytes.
    pub byte_size: u64,
    /// Parser/extractor outcome.
    pub parse_status: FileParseStatus,
}

/// One code-symbol fact tied to a staged file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SymbolInput {
    /// Stable structural symbol identity.
    pub symbol_id: SymbolId,
    /// File containing the symbol.
    pub file_id: FileId,
    /// Extractor-defined bounded symbol category.
    pub symbol_kind: String,
    /// Fully qualified display/search name.
    pub qualified_name: String,
    /// Deterministic declaration signature.
    pub signature: String,
    /// Inclusive start byte in the source file.
    pub start_byte: u64,
    /// Exclusive end byte in the source file.
    pub end_byte: u64,
    /// One-based inclusive start line.
    pub start_line: u32,
    /// One-based inclusive end line.
    pub end_line: u32,
    /// Digest of the normalized structural representation.
    pub structural_digest: ContentDigest,
    /// Language-level declaration visibility when the extractor can prove it.
    pub visibility: Option<Visibility>,
    /// Module export state proven by the extractor.
    pub export: SymbolExportFlags,
    /// Async and static execution modifiers proven by the extractor.
    pub execution: SymbolExecutionFlags,
    /// Whether the source contains a declaration without an implementation body.
    pub declaration_only: bool,
    /// Sampled directed Brandes betweenness, normalized to parts per billion.
    ///
    /// The value is derived from this generation's structural graph and is
    /// therefore intentionally excluded from the logical generation digest.
    pub betweenness_ppb: Option<u32>,
    /// Directed `PageRank` over calls and references, normalized to parts per billion.
    ///
    /// Like betweenness, this is derived from the generation graph and excluded
    /// from the logical generation digest.
    pub pagerank_ppb: Option<u32>,
}

/// One resolved structural graph edge.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EdgeInput {
    /// Origin symbol.
    pub source_symbol_id: SymbolId,
    /// Destination symbol.
    pub target_symbol_id: SymbolId,
    /// Typed relationship.
    pub kind: EdgeKind,
    /// Calibrated confidence in the inclusive range 0 through 1.
    pub confidence: f32,
    /// Bounded resolver/extractor provenance.
    pub provenance: String,
    /// Exact number of relation sites represented by this canonical edge.
    pub site_count: u32,
}

/// Whether a persisted reference span is an exact token or a bounded legacy anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceSpanPrecision {
    /// Start/end identify the exact retained source token.
    Exact,
    /// Start/end identify a validated point or line anchor, not the normalized name itself.
    CoarsePoint,
    /// Start/end conservatively reuse the validated lexical owner span.
    CoarseOwner,
}

impl ReferenceSpanPrecision {
    /// Stable PostgreSQL representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::CoarsePoint => "coarse_point",
            Self::CoarseOwner => "coarse_owner",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, ()> {
        match value {
            "exact" => Ok(Self::Exact),
            "coarse_point" => Ok(Self::CoarsePoint),
            "coarse_owner" => Ok(Self::CoarseOwner),
            _ => Err(()),
        }
    }
}

/// One source span that refers to an optional resolved symbol.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReferenceInput {
    /// File containing the reference.
    pub file_id: FileId,
    /// Closest extracted symbol that lexically owns the reference.
    pub owner_symbol_id: Option<SymbolId>,
    /// Resolved target when available.
    pub target_symbol_id: Option<SymbolId>,
    /// Bounded normalized name retained even when resolution is unavailable.
    pub reference_name: String,
    /// Extractor-defined bounded reference category.
    pub reference_kind: String,
    /// Inclusive start byte in the source file.
    pub start_byte: u64,
    /// Exclusive end byte in the source file.
    pub end_byte: u64,
    /// Calibrated confidence in the inclusive range 0 through 1.
    pub confidence: f32,
    /// Bounded resolver provenance, including explicit unresolved outcomes.
    pub resolution_provenance: String,
    /// Exact number of source sites represented by this retained anchor row.
    pub site_count: u32,
    /// Machine-readable precision of the retained source span.
    pub span_precision: ReferenceSpanPrecision,
}

/// One immutable, privacy-safe static numerical evidence site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NumericalSiteInput {
    /// Stable site identity derived independently of worker scheduling.
    pub site_id: NumericalSiteId,
    /// File containing the expression.
    pub file_id: FileId,
    /// Closest extracted declaration containing the expression.
    pub owner_symbol_id: Option<SymbolId>,
    /// Inclusive start byte in the source file.
    pub start_byte: u64,
    /// Exclusive end byte in the source file.
    pub end_byte: u64,
    /// One-based inclusive start line.
    pub start_line: u32,
    /// One-based inclusive end line.
    pub end_line: u32,
    /// Stable numerical operation category.
    pub operation: String,
    /// Stable potential-hazard category, or `none_observed`.
    pub hazard: String,
    /// Best statically visible precision category.
    pub precision: String,
    /// Source-version-fenced, privacy-safe expression identity digest.
    pub expression_digest: ContentDigest,
    /// Deterministic heuristic confidence in parts per million.
    pub confidence_ppm: u32,
    /// Bounded extractor or observer provenance.
    pub provenance: String,
    /// Evidence tier such as `heuristic`, never inferred from confidence.
    pub evidence_level: String,
    /// Stable comma-separated list of facts static analysis could not prove.
    pub unknowns: String,
}

/// Search document staged as part of one immutable generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchDocumentInput {
    /// Stable logical identity, independent of `ParadeDB`'s bigint key field.
    pub document_id: DocumentId,
    /// Owning file when the structural file row is available.
    pub file_id: Option<FileId>,
    /// Owning symbol for symbol-level evidence.
    pub symbol_id: Option<SymbolId>,
    /// Project-normalized source path.
    pub path: String,
    /// Normalized language identifier.
    pub language: String,
    /// Intent-routing document category.
    pub kind: DocumentKind,
    /// Code-aware symbol or declaration name.
    pub qualified_name: String,
    /// Source text indexed with `pdb.source_code`.
    pub code: String,
    /// Documentation and summaries indexed as natural language.
    pub natural_text: String,
    /// Bounded structured ranking/filter metadata.
    pub metadata: Value,
}

/// Complete unordered worker output for one logical generation.
///
/// Validation sorts, canonicalizes, and deduplicates these facts before COPY.
#[derive(Clone, Debug, Default)]
pub struct GenerationFacts {
    /// Source-file facts.
    pub files: Vec<FileInput>,
    /// Symbol facts.
    pub symbols: Vec<SymbolInput>,
    /// Graph-edge facts.
    pub edges: Vec<EdgeInput>,
    /// Reference-span facts.
    pub references: Vec<ReferenceInput>,
    /// Static numerical evidence facts.
    pub numerical_sites: Vec<NumericalSiteInput>,
    /// BM25 search documents.
    pub documents: Vec<SearchDocumentInput>,
}

impl GenerationFacts {
    /// Cooperatively measure the complete unordered payload, including spare capacities.
    /// # Errors
    ///
    /// Returns an error if measurement is cancelled, retained-size arithmetic
    /// overflows, or the payload exceeds `maximum_retained_bytes`.
    pub fn measure_retained_bytes<Cancel>(
        &self,
        maximum_retained_bytes: u64,
        cancelled: Cancel,
    ) -> Result<GenerationMemoryMeasurement, GenerationMemoryModelError>
    where
        Cancel: FnMut() -> bool,
    {
        model_raw_facts(self, maximum_retained_bytes, cancelled)
    }
}

/// Fixed-size result of a cooperative retained-memory traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationMemoryMeasurement {
    retained_bytes: u64,
    transient_bytes: u64,
}

impl GenerationMemoryMeasurement {
    /// Rust-owned bytes retained by the measured generation payload.
    #[must_use]
    pub const fn retained_bytes(self) -> u64 {
        self.retained_bytes
    }

    /// Cumulative temporary traversal storage allocated while measuring metadata.
    #[must_use]
    pub const fn transient_bytes(self) -> u64 {
        self.transient_bytes
    }
}

/// Cooperative retained-memory traversal failed before validation admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationMemoryModelError {
    /// The caller requested cancellation during traversal.
    Cancelled,
    /// Retained bytes overflowed or exceeded the supplied ceiling.
    RetainedLimit,
    /// Structured search metadata exceeded the storage depth contract.
    MetadataDepth,
}

/// Validated, canonical search document ready for deterministic persistence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanonicalSearchDocument {
    pub(crate) document_id: DocumentId,
    pub(crate) file_id: Option<FileId>,
    pub(crate) symbol_id: Option<SymbolId>,
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) kind: DocumentKind,
    pub(crate) qualified_name: String,
    pub(crate) code: String,
    pub(crate) natural_text: String,
    pub(crate) metadata_json: String,
}

pub(crate) struct ValidatedFactTables {
    pub(crate) files: Vec<FileInput>,
    pub(crate) symbols: Vec<SymbolInput>,
    pub(crate) edges: Vec<EdgeInput>,
    pub(crate) references: Vec<ReferenceInput>,
    pub(crate) numerical_sites: Vec<NumericalSiteInput>,
    pub(crate) documents: Vec<CanonicalSearchDocument>,
}

/// Storage-validated, deterministically reduced generation payload.
///
/// Construction is the only boundary that accepts unordered raw facts; COPY accepts this
/// immutable capability so database tasks never perform an unbounded synchronous reduction.
pub struct CanonicalGenerationFacts {
    pub(super) tables: ValidatedFactTables,
    pub(crate) digest: ContentDigest,
    pub(crate) digest_version: GenerationDigestVersion,
}

impl CanonicalGenerationFacts {
    /// Exact canonical row counts persisted with the ready generation.
    pub(crate) fn fact_counts(&self) -> crate::NativeGenerationSpillFactCounts {
        let count = |rows: usize| u64::try_from(rows).unwrap_or(u64::MAX);
        crate::NativeGenerationSpillFactCounts {
            files: count(self.tables.files.len()),
            symbols: count(self.tables.symbols.len()),
            edges: count(self.tables.edges.len()),
            references: count(self.tables.references.len()),
            numerical_sites: count(self.tables.numerical_sites.len()),
            documents: count(self.tables.documents.len()),
        }
    }

    /// Canonical logical digest covering every persisted field.
    #[must_use]
    pub const fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    /// Versioned contract under which the logical digest was produced.
    #[must_use]
    pub const fn digest_version(&self) -> GenerationDigestVersion {
        self.digest_version
    }

    pub(super) fn measure_retained_bytes<Cancel>(
        &self,
        maximum_retained_bytes: u64,
        cancelled: Cancel,
    ) -> Result<GenerationMemoryMeasurement, GenerationMemoryModelError>
    where
        Cancel: FnMut() -> bool,
    {
        model_canonical_facts(self, maximum_retained_bytes, cancelled)
    }

    /// Canonically ordered file rows.
    #[must_use]
    pub fn files(&self) -> &[FileInput] {
        &self.tables.files
    }

    /// Canonically ordered symbol rows.
    #[must_use]
    pub fn symbols(&self) -> &[SymbolInput] {
        &self.tables.symbols
    }

    /// Canonically ordered graph edges.
    #[must_use]
    pub fn edges(&self) -> &[EdgeInput] {
        &self.tables.edges
    }

    /// Canonically ordered resolved and unresolved source references.
    #[must_use]
    pub fn references(&self) -> &[ReferenceInput] {
        &self.tables.references
    }

    /// Canonically ordered static numerical evidence sites.
    #[must_use]
    pub fn numerical_sites(&self) -> &[NumericalSiteInput] {
        &self.tables.numerical_sites
    }

    /// Canonically ordered, metadata-normalized search documents.
    #[must_use]
    pub fn documents(&self) -> &[CanonicalSearchDocument] {
        &self.tables.documents
    }
}

const MAX_MODELED_METADATA_DEPTH: usize = 64;
const JSON_OBJECT_NODE_ALLOWANCE: u64 = 128;

fn model_raw_facts<Cancel>(
    facts: &GenerationFacts,
    maximum_retained_bytes: u64,
    cancelled: Cancel,
) -> Result<GenerationMemoryMeasurement, GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    let mut model = MemoryModel::new(maximum_retained_bytes, cancelled);
    model.add_retained(usize_to_u64(size_of::<GenerationFacts>()))?;
    model.add_vector(&facts.files)?;
    model.add_vector(&facts.symbols)?;
    model.add_vector(&facts.edges)?;
    model.add_vector(&facts.references)?;
    model.add_vector(&facts.numerical_sites)?;
    model.add_vector(&facts.documents)?;
    model_file_fields(&mut model, &facts.files)?;
    model_symbol_fields(&mut model, &facts.symbols)?;
    model_edge_fields(&mut model, &facts.edges)?;
    model_reference_fields(&mut model, &facts.references)?;
    model_numerical_site_fields(&mut model, &facts.numerical_sites)?;
    for document in &facts.documents {
        model.poll()?;
        model_document_fields(
            &mut model,
            &DocumentModel {
                document_id: &document.document_id,
                file_id: document.file_id.as_ref(),
                symbol_id: document.symbol_id.as_ref(),
                path: &document.path,
                language: &document.language,
                qualified_name: &document.qualified_name,
                code: &document.code,
                natural_text: &document.natural_text,
            },
        )?;
        model_json(&document.metadata, &mut model)?;
    }
    Ok(model.finish())
}

fn model_canonical_facts<Cancel>(
    facts: &CanonicalGenerationFacts,
    maximum_retained_bytes: u64,
    cancelled: Cancel,
) -> Result<GenerationMemoryMeasurement, GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    let mut model = MemoryModel::new(maximum_retained_bytes, cancelled);
    model.add_retained(usize_to_u64(size_of::<CanonicalGenerationFacts>()))?;
    model.add_id(facts.digest.as_str())?;
    model.add_vector(&facts.tables.files)?;
    model.add_vector(&facts.tables.symbols)?;
    model.add_vector(&facts.tables.edges)?;
    model.add_vector(&facts.tables.references)?;
    model.add_vector(&facts.tables.numerical_sites)?;
    model.add_vector(&facts.tables.documents)?;
    model_file_fields(&mut model, &facts.tables.files)?;
    model_symbol_fields(&mut model, &facts.tables.symbols)?;
    model_edge_fields(&mut model, &facts.tables.edges)?;
    model_reference_fields(&mut model, &facts.tables.references)?;
    model_numerical_site_fields(&mut model, &facts.tables.numerical_sites)?;
    for document in &facts.tables.documents {
        model.poll()?;
        model_document_fields(
            &mut model,
            &DocumentModel {
                document_id: &document.document_id,
                file_id: document.file_id.as_ref(),
                symbol_id: document.symbol_id.as_ref(),
                path: &document.path,
                language: &document.language,
                qualified_name: &document.qualified_name,
                code: &document.code,
                natural_text: &document.natural_text,
            },
        )?;
        model.add_string(&document.metadata_json)?;
    }
    Ok(model.finish())
}

fn model_file_fields<Cancel>(
    model: &mut MemoryModel<Cancel>,
    files: &[FileInput],
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    for file in files {
        model.poll()?;
        model.add_id(file.file_id.as_str())?;
        model.add_string(&file.normalized_path)?;
        model.add_string(&file.language)?;
        model.add_id(file.content_hash.as_str())?;
    }
    Ok(())
}

fn model_symbol_fields<Cancel>(
    model: &mut MemoryModel<Cancel>,
    symbols: &[SymbolInput],
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    for symbol in symbols {
        model.poll()?;
        model.add_id(symbol.symbol_id.as_str())?;
        model.add_id(symbol.file_id.as_str())?;
        model.add_string(&symbol.symbol_kind)?;
        model.add_string(&symbol.qualified_name)?;
        model.add_string(&symbol.signature)?;
        model.add_id(symbol.structural_digest.as_str())?;
    }
    Ok(())
}

fn model_edge_fields<Cancel>(
    model: &mut MemoryModel<Cancel>,
    edges: &[EdgeInput],
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    for edge in edges {
        model.poll()?;
        model.add_id(edge.source_symbol_id.as_str())?;
        model.add_id(edge.target_symbol_id.as_str())?;
        model.add_string(&edge.provenance)?;
    }
    Ok(())
}

fn model_reference_fields<Cancel>(
    model: &mut MemoryModel<Cancel>,
    references: &[ReferenceInput],
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    for reference in references {
        model.poll()?;
        model.add_id(reference.file_id.as_str())?;
        model.add_optional_id(reference.owner_symbol_id.as_ref().map(SymbolId::as_str))?;
        model.add_optional_id(reference.target_symbol_id.as_ref().map(SymbolId::as_str))?;
        model.add_string(&reference.reference_name)?;
        model.add_string(&reference.reference_kind)?;
        model.add_string(&reference.resolution_provenance)?;
    }
    Ok(())
}

fn model_numerical_site_fields<Cancel>(
    model: &mut MemoryModel<Cancel>,
    sites: &[NumericalSiteInput],
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    for site in sites {
        model.poll()?;
        model.add_id(site.site_id.as_str())?;
        model.add_id(site.file_id.as_str())?;
        model.add_optional_id(site.owner_symbol_id.as_ref().map(SymbolId::as_str))?;
        model.add_string(&site.operation)?;
        model.add_string(&site.hazard)?;
        model.add_string(&site.precision)?;
        model.add_id(site.expression_digest.as_str())?;
        model.add_string(&site.provenance)?;
        model.add_string(&site.evidence_level)?;
        model.add_string(&site.unknowns)?;
    }
    Ok(())
}

struct DocumentModel<'a> {
    document_id: &'a DocumentId,
    file_id: Option<&'a FileId>,
    symbol_id: Option<&'a SymbolId>,
    path: &'a String,
    language: &'a String,
    qualified_name: &'a String,
    code: &'a String,
    natural_text: &'a String,
}

fn model_document_fields<Cancel>(
    model: &mut MemoryModel<Cancel>,
    document: &DocumentModel<'_>,
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    model.add_id(document.document_id.as_str())?;
    model.add_optional_id(document.file_id.map(FileId::as_str))?;
    model.add_optional_id(document.symbol_id.map(SymbolId::as_str))?;
    model.add_string(document.path)?;
    model.add_string(document.language)?;
    model.add_string(document.qualified_name)?;
    model.add_string(document.code)?;
    model.add_string(document.natural_text)
}

fn model_json<Cancel>(
    value: &Value,
    model: &mut MemoryModel<Cancel>,
) -> Result<(), GenerationMemoryModelError>
where
    Cancel: FnMut() -> bool,
{
    model.poll()?;
    let mut stack = Vec::<JsonFrame<'_>>::new();
    stack
        .try_reserve_exact(MAX_MODELED_METADATA_DEPTH + 1)
        .map_err(|_| GenerationMemoryModelError::RetainedLimit)?;
    model.add_transient(vector_capacity_bytes(&stack))?;
    JsonTraversal { model, stack }.measure(value)
}

struct JsonTraversal<'value, 'model, Cancel> {
    model: &'model mut MemoryModel<Cancel>,
    stack: Vec<JsonFrame<'value>>,
}

impl<'value, Cancel> JsonTraversal<'value, '_, Cancel>
where
    Cancel: FnMut() -> bool,
{
    fn measure(mut self, value: &'value Value) -> Result<(), GenerationMemoryModelError> {
        self.visit(value, 0)?;
        while let Some(frame) = self.stack.last_mut() {
            self.model.poll()?;
            let next = match frame {
                JsonFrame::Array(values) => values.next().map(JsonNext::Value),
                JsonFrame::Object(values) => values
                    .next()
                    .map(|(key, value)| JsonNext::Entry(key, value)),
            };
            match next {
                Some(JsonNext::Value(value)) => {
                    let depth = self.stack.len();
                    self.visit(value, depth)?;
                }
                Some(JsonNext::Entry(key, value)) => {
                    self.model.add_string(key)?;
                    let depth = self.stack.len();
                    self.visit(value, depth)?;
                }
                None => {
                    self.stack.pop();
                }
            }
        }
        Ok(())
    }

    fn visit(
        &mut self,
        value: &'value Value,
        depth: usize,
    ) -> Result<(), GenerationMemoryModelError> {
        self.model.poll()?;
        if depth > MAX_MODELED_METADATA_DEPTH {
            return Err(GenerationMemoryModelError::MetadataDepth);
        }
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
            Value::String(value) => self.model.add_string(value)?,
            Value::Array(values) => {
                self.model.add_retained(vector_capacity_bytes(values))?;
                self.stack.push(JsonFrame::Array(values.iter()));
            }
            Value::Object(values) => {
                let entry_bytes = usize_to_u64(size_of::<(String, Value)>())
                    .saturating_add(JSON_OBJECT_NODE_ALLOWANCE);
                self.model.add_retained(
                    usize_to_u64(values.len())
                        .checked_mul(entry_bytes)
                        .ok_or(GenerationMemoryModelError::RetainedLimit)?,
                )?;
                self.stack.push(JsonFrame::Object(values.iter()));
            }
        }
        Ok(())
    }
}

enum JsonFrame<'a> {
    Array(std::slice::Iter<'a, Value>),
    Object(serde_json::map::Iter<'a>),
}

enum JsonNext<'a> {
    Value(&'a Value),
    Entry(&'a String, &'a Value),
}

struct MemoryModel<Cancel> {
    maximum_retained_bytes: u64,
    retained_bytes: u64,
    transient_bytes: u64,
    cancelled: Cancel,
}

impl<Cancel> MemoryModel<Cancel>
where
    Cancel: FnMut() -> bool,
{
    const fn new(maximum_retained_bytes: u64, cancelled: Cancel) -> Self {
        Self {
            maximum_retained_bytes,
            retained_bytes: 0,
            transient_bytes: 0,
            cancelled,
        }
    }

    fn poll(&mut self) -> Result<(), GenerationMemoryModelError> {
        if (self.cancelled)() {
            Err(GenerationMemoryModelError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn add_retained(&mut self, bytes: u64) -> Result<(), GenerationMemoryModelError> {
        self.poll()?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(bytes)
            .ok_or(GenerationMemoryModelError::RetainedLimit)?;
        if self.retained_bytes > self.maximum_retained_bytes {
            Err(GenerationMemoryModelError::RetainedLimit)
        } else {
            Ok(())
        }
    }

    fn add_transient(&mut self, bytes: u64) -> Result<(), GenerationMemoryModelError> {
        self.poll()?;
        self.transient_bytes = self
            .transient_bytes
            .checked_add(bytes)
            .ok_or(GenerationMemoryModelError::RetainedLimit)?;
        Ok(())
    }

    fn add_vector<T>(&mut self, values: &Vec<T>) -> Result<(), GenerationMemoryModelError> {
        self.add_retained(vector_capacity_bytes(values))
    }

    fn add_id(&mut self, value: &str) -> Result<(), GenerationMemoryModelError> {
        self.add_retained(usize_to_u64(value.len()))
    }

    fn add_optional_id(&mut self, value: Option<&str>) -> Result<(), GenerationMemoryModelError> {
        value.map_or(Ok(()), |value| self.add_id(value))
    }

    fn add_string(&mut self, value: &String) -> Result<(), GenerationMemoryModelError> {
        self.add_retained(usize_to_u64(value.capacity()))
    }

    fn finish(self) -> GenerationMemoryMeasurement {
        GenerationMemoryMeasurement {
            retained_bytes: self.retained_bytes,
            transient_bytes: self.transient_bytes,
        }
    }
}

fn vector_capacity_bytes<T>(values: &Vec<T>) -> u64 {
    usize_to_u64(values.capacity()).saturating_mul(usize_to_u64(size_of::<T>()))
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

impl CanonicalSearchDocument {
    /// Stable logical document identity.
    #[must_use]
    pub const fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    /// Owning file when present.
    #[must_use]
    pub const fn file_id(&self) -> Option<&FileId> {
        self.file_id.as_ref()
    }

    /// Owning symbol when present.
    #[must_use]
    pub const fn symbol_id(&self) -> Option<&SymbolId> {
        self.symbol_id.as_ref()
    }

    /// Canonical project-relative path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stable language identifier.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Intent-routing document category.
    #[must_use]
    pub const fn kind(&self) -> DocumentKind {
        self.kind
    }

    /// Code-aware qualified-name field.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Code-aware declaration/path text.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Natural-language documentation text.
    #[must_use]
    pub fn natural_text(&self) -> &str {
        &self.natural_text
    }

    /// Canonical serialized metadata persisted to JSONB.
    #[must_use]
    pub fn metadata_json(&self) -> &str {
        &self.metadata_json
    }
}
