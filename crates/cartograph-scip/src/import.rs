mod plan;

pub use plan::{ScipOverlayPlan, ScipOverlayPreparation, prepare_scip_overlay, scip_overlay_paths};

use std::collections::{BTreeMap, BTreeSet};

use cartograph_db::{
    EdgeInput, GenerationFacts, ReferenceInput, ReferenceSpanPrecision, SearchDocumentInput,
    SymbolInput,
};
use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, EdgeKind, FileId, NormalizedPath, SymbolId, SymbolKind,
};
use serde::Serialize;
use serde_json::json;

use crate::{
    codec::decode_scip_index,
    model::{
        SYMBOL_ROLE_DEFINITION, ScipDocument, ScipError, ScipOccurrence, ScipRelationship,
        ScipSymbolInformation, ScipSymbolKind,
    },
    symbol::{DescriptorSuffix, descriptors_to_qualified_name, parse_scip_symbol},
};

const SCIP_SYMBOL_ID_DOMAIN: &[u8] = b"cartograph-v2-scip-symbol-v1";
const SCIP_DOCUMENT_ID_DOMAIN: &[u8] = b"cartograph-v2-scip-document-v1";
const SCIP_STRUCTURAL_DIGEST_DOMAIN: &[u8] = b"cartograph-v2-scip-structural-v1";
const SCIP_OVERLAY_PROVENANCE: &str = "scip-overlay-exact";
const SCIP_FOREIGN_PROVENANCE: &str = "scip-overlay-foreign";
const SCIP_UNRESOLVED_PROVENANCE: &str = "scip-overlay-unresolved";
const MAXIMUM_OVERLAY_ROWS: usize = 10_000_000;
const MAXIMUM_QUALIFIED_NAME_BYTES: usize = 2_048;
const MAXIMUM_REFERENCE_NAME_BYTES: usize = 4_096;
const MAXIMUM_NATURAL_TEXT_BYTES: usize = 1_024 * 1_024;
const MAXIMUM_SITE_COUNT: u32 = 100_000_000;
const MAXIMUM_PROVENANCE_BYTES: usize = 256;
const SCIP_PROVENANCE_PREFIX: &str = "scip-overlay:";
const IMPORTED_SCIP_KINDS: [ScipSymbolKind; 36] = [
    ScipSymbolKind::Unspecified,
    ScipSymbolKind::Class,
    ScipSymbolKind::Constant,
    ScipSymbolKind::Constructor,
    ScipSymbolKind::Enum,
    ScipSymbolKind::EnumMember,
    ScipSymbolKind::Field,
    ScipSymbolKind::File,
    ScipSymbolKind::Function,
    ScipSymbolKind::Getter,
    ScipSymbolKind::Interface,
    ScipSymbolKind::Macro,
    ScipSymbolKind::Method,
    ScipSymbolKind::Module,
    ScipSymbolKind::Namespace,
    ScipSymbolKind::Object,
    ScipSymbolKind::Package,
    ScipSymbolKind::Parameter,
    ScipSymbolKind::Property,
    ScipSymbolKind::Protocol,
    ScipSymbolKind::SelfParameter,
    ScipSymbolKind::Setter,
    ScipSymbolKind::Struct,
    ScipSymbolKind::Trait,
    ScipSymbolKind::Type,
    ScipSymbolKind::TypeAlias,
    ScipSymbolKind::TypeParameter,
    ScipSymbolKind::Variable,
    ScipSymbolKind::AbstractMethod,
    ScipSymbolKind::Accessor,
    ScipSymbolKind::SingletonMethod,
    ScipSymbolKind::StaticDataMember,
    ScipSymbolKind::StaticField,
    ScipSymbolKind::StaticMethod,
    ScipSymbolKind::StaticProperty,
    ScipSymbolKind::StaticVariable,
];

struct ScipKindRule {
    scip_kinds: &'static [ScipSymbolKind],
    symbol_kind: SymbolKind,
}

const SCIP_KIND_RULES: [ScipKindRule; 18] = [
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Class, ScipSymbolKind::Object],
        symbol_kind: SymbolKind::Class,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Constant],
        symbol_kind: SymbolKind::Constant,
    },
    ScipKindRule {
        scip_kinds: &[
            ScipSymbolKind::Constructor,
            ScipSymbolKind::Getter,
            ScipSymbolKind::Method,
            ScipSymbolKind::Setter,
            ScipSymbolKind::AbstractMethod,
            ScipSymbolKind::Accessor,
            ScipSymbolKind::SingletonMethod,
            ScipSymbolKind::StaticMethod,
        ],
        symbol_kind: SymbolKind::Method,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Enum],
        symbol_kind: SymbolKind::Enum,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::EnumMember],
        symbol_kind: SymbolKind::EnumMember,
    },
    ScipKindRule {
        scip_kinds: &[
            ScipSymbolKind::Field,
            ScipSymbolKind::StaticDataMember,
            ScipSymbolKind::StaticField,
        ],
        symbol_kind: SymbolKind::Field,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::File],
        symbol_kind: SymbolKind::File,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Function, ScipSymbolKind::Macro],
        symbol_kind: SymbolKind::Function,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Interface],
        symbol_kind: SymbolKind::Interface,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Module, ScipSymbolKind::Package],
        symbol_kind: SymbolKind::Module,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Namespace],
        symbol_kind: SymbolKind::Namespace,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Parameter, ScipSymbolKind::SelfParameter],
        symbol_kind: SymbolKind::Parameter,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Property, ScipSymbolKind::StaticProperty],
        symbol_kind: SymbolKind::Property,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Protocol],
        symbol_kind: SymbolKind::Protocol,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Struct],
        symbol_kind: SymbolKind::Struct,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Trait],
        symbol_kind: SymbolKind::Trait,
    },
    ScipKindRule {
        scip_kinds: &[
            ScipSymbolKind::Type,
            ScipSymbolKind::TypeAlias,
            ScipSymbolKind::TypeParameter,
        ],
        symbol_kind: SymbolKind::TypeAlias,
    },
    ScipKindRule {
        scip_kinds: &[ScipSymbolKind::Variable, ScipSymbolKind::StaticVariable],
        symbol_kind: SymbolKind::Variable,
    },
];

/// Accounting for a per-file SCIP replacement overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScipOverlayReport {
    covered_documents: u64,
    skipped_documents: u64,
    replaced_native_symbols: u64,
    imported_symbols: u64,
    imported_edges: u64,
    imported_references: u64,
    exact_typed_edges: u64,
    unresolved_links: u64,
}

impl ScipOverlayReport {
    /// Number of source files whose native symbol facts were replaced.
    #[must_use]
    pub const fn covered_documents(self) -> u64 {
        self.covered_documents
    }

    /// Number of invalid, stale, duplicate, or non-project documents skipped.
    #[must_use]
    pub const fn skipped_documents(self) -> u64 {
        self.skipped_documents
    }

    /// Number of native non-file symbols evicted from covered files.
    #[must_use]
    pub const fn replaced_native_symbols(self) -> u64 {
        self.replaced_native_symbols
    }

    /// Number of SCIP symbol facts admitted.
    #[must_use]
    pub const fn imported_symbols(self) -> u64 {
        self.imported_symbols
    }

    /// Number of resolved typed graph edges admitted.
    #[must_use]
    pub const fn imported_edges(self) -> u64 {
        self.imported_edges
    }

    /// Number of exact source occurrences admitted as references.
    #[must_use]
    pub const fn imported_references(self) -> u64 {
        self.imported_references
    }

    /// Number of edges recovered from Cartograph's forward-compatible extension.
    #[must_use]
    pub const fn exact_typed_edges(self) -> u64 {
        self.exact_typed_edges
    }

    /// Number of relationship or occurrence targets not present in this project overlay.
    #[must_use]
    pub const fn unresolved_links(self) -> u64 {
        self.unresolved_links
    }
}

struct PreparedDocument<'a> {
    document: &'a ScipDocument,
    path: String,
    file_id: FileId,
    language: String,
    source: Vec<u8>,
    line_starts: Vec<usize>,
}

/// Mutable generation and bounded SCIP payload for one replacement overlay.
pub struct ScipOverlayRequest<'input> {
    facts: &'input mut GenerationFacts,
    bytes: &'input [u8],
    maximum_rows: usize,
}

impl<'input> ScipOverlayRequest<'input> {
    /// Build one bounded overlay request.
    #[must_use]
    pub const fn new(
        facts: &'input mut GenerationFacts,
        bytes: &'input [u8],
        maximum_rows: usize,
    ) -> Self {
        Self {
            facts,
            bytes,
            maximum_rows,
        }
    }
}

#[derive(Clone)]
struct ImportedSymbolSpan {
    symbol_id: SymbolId,
    start_byte: u64,
    end_byte: u64,
    span_size: u64,
}

struct ImportAccumulator {
    symbols: Vec<SymbolInput>,
    documents: Vec<SearchDocumentInput>,
    references: Vec<ReferenceInput>,
    edges: BTreeMap<(SymbolId, SymbolId, String, String), EdgeInput>,
    symbol_by_key: BTreeMap<String, SymbolId>,
    kind_by_id: BTreeMap<SymbolId, SymbolKind>,
    spans_by_path: BTreeMap<String, Vec<ImportedSymbolSpan>>,
    exact_edge_keys: BTreeSet<(SymbolId, SymbolId, String)>,
    report: ScipOverlayReport,
}

struct DocumentSource<'callbacks, ReadSource, Cancel> {
    read_source: &'callbacks mut ReadSource,
    cancelled: &'callbacks mut Cancel,
}

struct ImportState<'state, Cancel> {
    imported: &'state mut ImportAccumulator,
    cancelled: &'state mut Cancel,
}

#[derive(Clone, Copy)]
struct SymbolBuildLookups<'lookup> {
    file_symbol_by_id: &'lookup BTreeMap<FileId, SymbolId>,
    native_candidates: &'lookup NativeSymbolCandidates,
}

#[derive(Clone, Copy)]
struct ScipSymbolContext<'context, 'document> {
    prepared: &'context PreparedDocument<'document>,
    symbol: &'context ScipSymbolInformation,
    source_id: &'context SymbolId,
}

#[derive(Clone, Copy)]
struct ScipOccurrenceContext<'context, 'document> {
    prepared: &'context PreparedDocument<'document>,
    occurrence: &'context ScipOccurrence,
    file_symbol_id: &'context SymbolId,
}

#[derive(Clone, Copy)]
struct SourceLines<'source> {
    source: &'source [u8],
    line_starts: &'source [usize],
}

impl ImportAccumulator {
    fn new(skipped_documents: u64) -> Self {
        Self {
            symbols: Vec::new(),
            documents: Vec::new(),
            references: Vec::new(),
            edges: BTreeMap::new(),
            symbol_by_key: BTreeMap::new(),
            kind_by_id: BTreeMap::new(),
            spans_by_path: BTreeMap::new(),
            exact_edge_keys: BTreeSet::new(),
            report: ScipOverlayReport {
                skipped_documents,
                ..ScipOverlayReport::default()
            },
        }
    }

    fn unresolved(&mut self) -> Result<(), ScipError> {
        self.report.unresolved_links = self
            .report
            .unresolved_links
            .checked_add(1)
            .ok_or(ScipError::LimitExceeded)?;
        Ok(())
    }

    fn push_edge(&mut self, edge: EdgeInput, exact: bool) -> Result<(), ScipError> {
        if invalid_edge_score(&edge) || invalid_edge_provenance(&edge) {
            return Err(ScipError::InvalidData);
        }
        let relation_key = (
            edge.source_symbol_id.clone(),
            edge.target_symbol_id.clone(),
            edge.kind.as_str().to_owned(),
        );
        if exact {
            self.exact_edge_keys.insert(relation_key.clone());
        } else if self.exact_edge_keys.contains(&relation_key) {
            return Ok(());
        }
        let key = (
            relation_key.0,
            relation_key.1,
            relation_key.2,
            edge.provenance.clone(),
        );
        match self.edges.get_mut(&key) {
            Some(existing) if exact => {
                existing.site_count = existing
                    .site_count
                    .checked_add(edge.site_count)
                    .filter(|count| *count <= MAXIMUM_SITE_COUNT)
                    .ok_or(ScipError::LimitExceeded)?;
                existing.confidence = existing.confidence.max(edge.confidence);
            }
            Some(_) => {}
            None => {
                self.edges.insert(key, edge);
            }
        }
        Ok(())
    }
}

fn invalid_edge_score(edge: &EdgeInput) -> bool {
    edge.site_count == 0
        || edge.site_count > MAXIMUM_SITE_COUNT
        || !edge.confidence.is_finite()
        || !(0.0..=1.0).contains(&edge.confidence)
}

fn invalid_edge_provenance(edge: &EdgeInput) -> bool {
    edge.provenance.is_empty() || edge.provenance.len() > MAXIMUM_PROVENANCE_BYTES
}

/// Replace native facts only for matching project files covered by one bounded SCIP index.
/// # Errors
///
/// Returns an error if the artifact/row cap is invalid, SCIP decoding fails,
/// source bytes are missing/mismatched, or overlay identities/ranges conflict.
pub fn apply_scip_overlay<ReadSource>(
    input: ScipOverlayRequest<'_>,
    read_source: ReadSource,
) -> Result<ScipOverlayReport, ScipError>
where
    ReadSource: FnMut(&str) -> Option<Vec<u8>>,
{
    apply_scip_overlay_with_cancellation(input, read_source, || false)
}

/// Apply a SCIP overlay while cooperatively polling between bounded records.
/// # Errors
///
/// Returns an error on cancellation or if artifact/row bounds, decoding,
/// source verification, symbol relationships, ranges, or native reconciliation fail.
pub fn apply_scip_overlay_with_cancellation<ReadSource, Cancel>(
    input: ScipOverlayRequest<'_>,
    mut read_source: ReadSource,
    mut cancelled: Cancel,
) -> Result<ScipOverlayReport, ScipError>
where
    ReadSource: FnMut(&str) -> Option<Vec<u8>>,
    Cancel: FnMut() -> bool,
{
    let ScipOverlayRequest {
        facts,
        bytes,
        maximum_rows,
    } = input;
    let plan = prepare_scip_overlay(
        ScipOverlayPreparation {
            facts,
            bytes,
            maximum_rows,
        },
        &mut read_source,
        &mut cancelled,
    )?;
    plan.retain_native_facts(facts, &mut cancelled)?;
    let (mut imported, report) = plan.into_imported();
    facts.symbols.append(&mut imported.symbols);
    facts.documents.append(&mut imported.documents);
    facts.references.append(&mut imported.references);
    facts.edges.append(&mut imported.edges);
    Ok(report)
}

fn prepare_documents<'a, ReadSource, Cancel>(
    documents: &'a [ScipDocument],
    file_by_path: &BTreeMap<String, &cartograph_db::FileInput>,
    source: &mut DocumentSource<'_, ReadSource, Cancel>,
) -> Result<(Vec<PreparedDocument<'a>>, u64), ScipError>
where
    ReadSource: FnMut(&str) -> Option<Vec<u8>>,
    Cancel: FnMut() -> bool,
{
    let mut prepared = Vec::new();
    let mut seen = BTreeSet::new();
    let mut skipped = 0_u64;
    for document in documents {
        poll(source.cancelled)?;
        let Some(path) = NormalizedPath::parse(&document.relative_path)
            .ok()
            .map(NormalizedPath::into_string)
        else {
            skipped = checked_increment(skipped)?;
            continue;
        };
        let Some(file) = file_by_path.get(&path) else {
            skipped = checked_increment(skipped)?;
            continue;
        };
        let Some(source_bytes) = (source.read_source)(&path) else {
            skipped = checked_increment(skipped)?;
            continue;
        };
        if !seen.insert(path.clone())
            || std::str::from_utf8(&source_bytes).is_err()
            || ContentDigest::from_bytes(*blake3::hash(&source_bytes).as_bytes())
                != file.content_hash
        {
            skipped = checked_increment(skipped)?;
            continue;
        }
        let line_starts = source_line_starts(&source_bytes);
        prepared.push(PreparedDocument {
            document,
            path,
            file_id: file.file_id.clone(),
            language: if document.language.is_empty() {
                file.language.clone()
            } else {
                bounded_text(&document.language, 64)
            },
            source: source_bytes,
            line_starts,
        });
    }
    Ok((prepared, skipped))
}

type NativeSymbolCandidates = BTreeMap<(FileId, String, String), Vec<SymbolId>>;

fn native_symbol_candidates(facts: &GenerationFacts) -> NativeSymbolCandidates {
    let mut candidates = NativeSymbolCandidates::new();
    for symbol in &facts.symbols {
        if symbol.symbol_kind != SymbolKind::File.as_str() {
            candidates
                .entry((
                    symbol.file_id.clone(),
                    symbol.symbol_kind.clone(),
                    symbol.qualified_name.clone(),
                ))
                .or_default()
                .push(symbol.symbol_id.clone());
        }
    }
    for symbols in candidates.values_mut() {
        symbols.sort();
    }
    candidates
}

fn build_symbols<Cancel>(
    documents: &[PreparedDocument<'_>],
    lookups: SymbolBuildLookups<'_>,
    state: &mut ImportState<'_, Cancel>,
) -> Result<(), ScipError>
where
    Cancel: FnMut() -> bool,
{
    for prepared in documents {
        poll(state.cancelled)?;
        let Some(file_symbol_id) = lookups.file_symbol_by_id.get(&prepared.file_id) else {
            return Err(ScipError::OverlayFailed);
        };
        let definitions = definition_occurrences(prepared);
        for symbol in &prepared.document.symbols {
            build_scip_symbol(
                ScipSymbolBuild {
                    prepared,
                    lookups: &lookups,
                    definitions: &definitions,
                    symbol,
                    file_symbol_id,
                },
                state,
            )?;
        }
        if let Some(spans) = state.imported.spans_by_path.get_mut(&prepared.path) {
            spans.sort_by(|left, right| {
                left.start_byte
                    .cmp(&right.start_byte)
                    .then_with(|| left.span_size.cmp(&right.span_size))
                    .then_with(|| left.symbol_id.cmp(&right.symbol_id))
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ScipSymbolBuild<'context, 'document, 'lookup> {
    prepared: &'context PreparedDocument<'document>,
    lookups: &'context SymbolBuildLookups<'lookup>,
    definitions: &'context BTreeMap<String, ScipOccurrence>,
    symbol: &'context ScipSymbolInformation,
    file_symbol_id: &'context SymbolId,
}

fn build_scip_symbol<Cancel>(
    input: ScipSymbolBuild<'_, '_, '_>,
    state: &mut ImportState<'_, Cancel>,
) -> Result<(), ScipError>
where
    Cancel: FnMut() -> bool,
{
    poll(state.cancelled)?;
    if input.symbol.symbol.is_empty() {
        state.imported.unresolved()?;
        return Ok(());
    }
    let key = scoped_symbol_key(&input.prepared.path, &input.symbol.symbol);
    let kind = symbol_kind(input.symbol);
    if kind == SymbolKind::File {
        state
            .imported
            .symbol_by_key
            .insert(key, input.file_symbol_id.clone());
        state
            .imported
            .kind_by_id
            .insert(input.file_symbol_id.clone(), SymbolKind::File);
        return Ok(());
    }
    if state.imported.symbol_by_key.contains_key(&key) {
        state.imported.unresolved()?;
        return Ok(());
    }
    let qualified_name = qualified_name(input.symbol);
    let span = input
        .definitions
        .get(&key)
        .and_then(|occurrence| definition_span(input.prepared, occurrence))
        .unwrap_or_else(|| fallback_span(input.prepared));
    let candidate_key = (
        input.prepared.file_id.clone(),
        kind.as_str().to_owned(),
        qualified_name.clone(),
    );
    let symbol_id = input
        .lookups
        .native_candidates
        .get(&candidate_key)
        .filter(|candidates| candidates.len() == 1)
        .and_then(|candidates| candidates.first())
        .cloned()
        .unwrap_or_else(|| deterministic_symbol_id(&key));
    persist_scip_symbol(
        state.imported,
        ScipSymbolPersistence {
            prepared: input.prepared,
            symbol: input.symbol,
            key,
            kind,
            span,
            symbol_id,
            qualified_name,
        },
    );
    Ok(())
}

struct ScipSymbolPersistence<'context, 'document> {
    prepared: &'context PreparedDocument<'document>,
    symbol: &'context ScipSymbolInformation,
    key: String,
    kind: SymbolKind,
    span: ImportedSpan,
    symbol_id: SymbolId,
    qualified_name: String,
}

fn persist_scip_symbol(imported: &mut ImportAccumulator, input: ScipSymbolPersistence<'_, '_>) {
    imported
        .symbol_by_key
        .insert(input.key.clone(), input.symbol_id.clone());
    imported
        .kind_by_id
        .insert(input.symbol_id.clone(), input.kind);
    imported
        .spans_by_path
        .entry(input.prepared.path.clone())
        .or_default()
        .push(ImportedSymbolSpan {
            symbol_id: input.symbol_id.clone(),
            start_byte: input.span.start_byte,
            end_byte: input.span.end_byte,
            span_size: input.span.end_byte.saturating_sub(input.span.start_byte),
        });
    imported.symbols.push(SymbolInput {
        symbol_id: input.symbol_id.clone(),
        file_id: input.prepared.file_id.clone(),
        symbol_kind: input.kind.as_str().to_owned(),
        qualified_name: input.qualified_name.clone(),
        signature: String::new(),
        start_byte: input.span.start_byte,
        end_byte: input.span.end_byte,
        start_line: input.span.start_line,
        end_line: input.span.end_line,
        structural_digest: structural_digest(&input.key, input.kind, &input.span),
        visibility: None,
        export: cartograph_domain::SymbolExportFlags::default(),
        execution: cartograph_domain::SymbolExecutionFlags::default(),
        declaration_only: false,
        betweenness_ppb: None,
        pagerank_ppb: None,
    });
    imported.documents.push(SearchDocumentInput {
        document_id: deterministic_document_id(&input.key),
        file_id: Some(input.prepared.file_id.clone()),
        symbol_id: Some(input.symbol_id),
        path: input.prepared.path.clone(),
        language: input.prepared.language.clone(),
        kind: document_kind(&input.prepared.path),
        qualified_name: input.qualified_name.clone(),
        code: input.qualified_name,
        natural_text: bounded_documentation(&input.symbol.documentation),
        metadata: json!({
            "interchange": "scip",
            "positionEncoding": "utf8",
        }),
    });
}

fn build_edges_and_references<Cancel>(
    documents: &[PreparedDocument<'_>],
    file_symbol_by_id: &BTreeMap<FileId, SymbolId>,
    state: &mut ImportState<'_, Cancel>,
) -> Result<(), ScipError>
where
    Cancel: FnMut() -> bool,
{
    for prepared in documents {
        poll(state.cancelled)?;
        let Some(file_symbol_id) = file_symbol_by_id.get(&prepared.file_id) else {
            return Err(ScipError::OverlayFailed);
        };
        for symbol in &prepared.document.symbols {
            poll(state.cancelled)?;
            let source_key = scoped_symbol_key(&prepared.path, &symbol.symbol);
            let Some(source_id) = state.imported.symbol_by_key.get(&source_key).cloned() else {
                continue;
            };
            let symbol_context = ScipSymbolContext {
                prepared,
                symbol,
                source_id: &source_id,
            };
            add_custom_edges(symbol_context, state)?;
            add_enclosing_edge(symbol_context, file_symbol_id, state.imported)?;
            add_standard_relationships(symbol_context, state.imported)?;
        }
        for occurrence in &prepared.document.occurrences {
            poll(state.cancelled)?;
            if occurrence.symbol_roles & SYMBOL_ROLE_DEFINITION != 0 {
                continue;
            }
            add_reference_occurrence(
                ScipOccurrenceContext {
                    prepared,
                    occurrence,
                    file_symbol_id,
                },
                state.imported,
            )?;
        }
    }
    state.imported.report.imported_edges = usize_to_u64(state.imported.edges.len());
    Ok(())
}

fn add_custom_edges<Cancel>(
    context: ScipSymbolContext<'_, '_>,
    state: &mut ImportState<'_, Cancel>,
) -> Result<(), ScipError>
where
    Cancel: FnMut() -> bool,
{
    for edge in &context.symbol.cartograph_edges {
        poll(state.cancelled)?;
        let target_key = scoped_symbol_key(&context.prepared.path, &edge.target_symbol);
        let Some(target_id) = state.imported.symbol_by_key.get(&target_key).cloned() else {
            state.imported.unresolved()?;
            continue;
        };
        let Some(kind) = edge_kind(&edge.edge_kind) else {
            state.imported.unresolved()?;
            continue;
        };
        let provenance = imported_provenance(&edge.provenance)?;
        state.imported.push_edge(
            EdgeInput {
                source_symbol_id: context.source_id.clone(),
                target_symbol_id: target_id,
                kind,
                confidence: f32::from_bits(edge.confidence_bits),
                provenance,
                site_count: edge.site_count,
            },
            true,
        )?;
        state.imported.report.exact_typed_edges =
            checked_increment(state.imported.report.exact_typed_edges)?;
    }
    Ok(())
}

fn add_enclosing_edge(
    context: ScipSymbolContext<'_, '_>,
    file_symbol_id: &SymbolId,
    imported: &mut ImportAccumulator,
) -> Result<(), ScipError> {
    if symbol_kind(context.symbol) == SymbolKind::File {
        return Ok(());
    }
    let parent_id = if context.symbol.enclosing_symbol.is_empty() {
        Some(file_symbol_id.clone())
    } else {
        let parent_key =
            scoped_symbol_key(&context.prepared.path, &context.symbol.enclosing_symbol);
        imported.symbol_by_key.get(&parent_key).cloned()
    };
    let Some(parent_id) = parent_id else {
        imported.unresolved()?;
        return Ok(());
    };
    if parent_id != *context.source_id {
        imported.push_edge(
            EdgeInput {
                source_symbol_id: parent_id,
                target_symbol_id: context.source_id.clone(),
                kind: EdgeKind::Contains,
                confidence: 1.0,
                provenance: SCIP_FOREIGN_PROVENANCE.to_owned(),
                site_count: 1,
            },
            false,
        )?;
    }
    Ok(())
}

fn add_standard_relationships(
    context: ScipSymbolContext<'_, '_>,
    imported: &mut ImportAccumulator,
) -> Result<(), ScipError> {
    for relationship in &context.symbol.relationships {
        let target_key = scoped_symbol_key(&context.prepared.path, &relationship.symbol);
        let Some(target_id) = imported.symbol_by_key.get(&target_key).cloned() else {
            imported.unresolved()?;
            continue;
        };
        let Some(kind) = relationship_edge_kind(
            relationship,
            imported.kind_by_id.get(context.source_id).copied(),
            imported.kind_by_id.get(&target_id).copied(),
        ) else {
            continue;
        };
        imported.push_edge(
            EdgeInput {
                source_symbol_id: context.source_id.clone(),
                target_symbol_id: target_id,
                kind,
                confidence: 1.0,
                provenance: SCIP_FOREIGN_PROVENANCE.to_owned(),
                site_count: 1,
            },
            false,
        )?;
    }
    Ok(())
}

fn add_reference_occurrence(
    context: ScipOccurrenceContext<'_, '_>,
    imported: &mut ImportAccumulator,
) -> Result<(), ScipError> {
    let target_key = scoped_symbol_key(&context.prepared.path, &context.occurrence.symbol);
    let Some(target_id) = imported.symbol_by_key.get(&target_key).cloned() else {
        imported.unresolved()?;
        return Ok(());
    };
    let Some(span) = occurrence_span(context.prepared, context.occurrence) else {
        imported.unresolved()?;
        return Ok(());
    };
    let owner_id = smallest_owner(
        imported.spans_by_path.get(&context.prepared.path),
        span.start_byte,
    )
    .unwrap_or_else(|| context.file_symbol_id.clone());
    let kind = exact_edge_kind(imported, &owner_id, &target_id).unwrap_or(EdgeKind::References);
    imported.references.push(ReferenceInput {
        file_id: context.prepared.file_id.clone(),
        owner_symbol_id: Some(owner_id.clone()),
        target_symbol_id: Some(target_id.clone()),
        reference_name: bounded_text(
            &reference_name(context.occurrence),
            MAXIMUM_REFERENCE_NAME_BYTES,
        ),
        reference_kind: kind.as_str().to_owned(),
        start_byte: span.start_byte,
        end_byte: span.end_byte,
        confidence: 1.0,
        resolution_provenance: SCIP_OVERLAY_PROVENANCE.to_owned(),
        site_count: 1,
        span_precision: ReferenceSpanPrecision::Exact,
    });
    imported.push_edge(
        EdgeInput {
            source_symbol_id: owner_id,
            target_symbol_id: target_id,
            kind,
            confidence: 1.0,
            provenance: SCIP_FOREIGN_PROVENANCE.to_owned(),
            site_count: 1,
        },
        false,
    )?;
    Ok(())
}

fn definition_occurrences(prepared: &PreparedDocument<'_>) -> BTreeMap<String, ScipOccurrence> {
    let mut definitions = BTreeMap::new();
    for occurrence in &prepared.document.occurrences {
        if occurrence.symbol_roles & SYMBOL_ROLE_DEFINITION != 0 {
            definitions
                .entry(scoped_symbol_key(&prepared.path, &occurrence.symbol))
                .or_insert_with(|| occurrence.clone());
        }
    }
    definitions
}

#[derive(Clone, Copy)]
struct ImportedSpan {
    start_byte: u64,
    end_byte: u64,
    start_line: u32,
    end_line: u32,
}

fn definition_span(
    prepared: &PreparedDocument<'_>,
    occurrence: &ScipOccurrence,
) -> Option<ImportedSpan> {
    scip_range_to_span(
        if occurrence.enclosing_range.is_empty() {
            &occurrence.range
        } else {
            &occurrence.enclosing_range
        },
        &prepared.source,
        &prepared.line_starts,
    )
}

fn occurrence_span(
    prepared: &PreparedDocument<'_>,
    occurrence: &ScipOccurrence,
) -> Option<ImportedSpan> {
    scip_range_to_span(&occurrence.range, &prepared.source, &prepared.line_starts)
}

fn scip_range_to_span(range: &[u32], source: &[u8], line_starts: &[usize]) -> Option<ImportedSpan> {
    let (start_line, start_column, end_line, end_column) = match range {
        [line, start, end] => (*line, *start, *line, *end),
        [start_line, start, end_line, end] => (*start_line, *start, *end_line, *end),
        _ => return None,
    };
    let lines = SourceLines {
        source,
        line_starts,
    };
    let start = line_column_to_byte(lines, start_line, start_column)?;
    let end = line_column_to_byte(lines, end_line, end_column)?;
    if start >= end {
        return None;
    }
    Some(ImportedSpan {
        start_byte: u64::try_from(start).ok()?,
        end_byte: u64::try_from(end).ok()?,
        start_line: start_line.checked_add(1)?,
        end_line: end_line.checked_add(1)?,
    })
}

fn source_line_starts(source: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.iter().enumerate() {
        if *byte == b'\n' && index + 1 < source.len() {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_column_to_byte(lines: SourceLines<'_>, line: u32, column: u32) -> Option<usize> {
    let line = usize::try_from(line).ok()?;
    let column = usize::try_from(column).ok()?;
    let start = *lines.line_starts.get(line)?;
    let next_line = lines
        .line_starts
        .get(line.checked_add(1)?)
        .copied()
        .unwrap_or(lines.source.len());
    let line_end = if next_line > start && lines.source.get(next_line - 1) == Some(&b'\n') {
        next_line - 1
    } else {
        next_line
    };
    let position = start.checked_add(column)?;
    if position > line_end
        || !std::str::from_utf8(lines.source)
            .ok()?
            .is_char_boundary(position)
    {
        return None;
    }
    Some(position)
}

fn fallback_span(prepared: &PreparedDocument<'_>) -> ImportedSpan {
    ImportedSpan {
        start_byte: 0,
        end_byte: usize_to_u64(prepared.source.len()),
        start_line: 1,
        end_line: u32::try_from(prepared.line_starts.len()).unwrap_or(u32::MAX),
    }
}

fn smallest_owner(spans: Option<&Vec<ImportedSymbolSpan>>, position: u64) -> Option<SymbolId> {
    spans?
        .iter()
        .filter(|span| span.start_byte <= position && position < span.end_byte)
        .min_by(|left, right| {
            left.span_size
                .cmp(&right.span_size)
                .then_with(|| left.symbol_id.cmp(&right.symbol_id))
        })
        .map(|span| span.symbol_id.clone())
}

fn exact_edge_kind(
    imported: &ImportAccumulator,
    source: &SymbolId,
    target: &SymbolId,
) -> Option<EdgeKind> {
    const PRIORITY: [EdgeKind; 14] = [
        EdgeKind::Calls,
        EdgeKind::Instantiates,
        EdgeKind::Imports,
        EdgeKind::Tests,
        EdgeKind::Implements,
        EdgeKind::Extends,
        EdgeKind::Overrides,
        EdgeKind::TypeOf,
        EdgeKind::Returns,
        EdgeKind::FieldAccess,
        EdgeKind::Decorates,
        EdgeKind::DefUse,
        EdgeKind::Exports,
        EdgeKind::References,
    ];
    PRIORITY.into_iter().find(|kind| {
        imported.exact_edge_keys.contains(&(
            source.clone(),
            target.clone(),
            kind.as_str().to_owned(),
        ))
    })
}

fn symbol_kind(symbol: &ScipSymbolInformation) -> SymbolKind {
    scip_kind_to_symbol_kind(symbol.kind).unwrap_or_else(|| {
        parse_scip_symbol(&symbol.symbol)
            .and_then(|parsed| {
                parsed
                    .descriptors
                    .last()
                    .map(|descriptor| descriptor.suffix)
            })
            .map_or(SymbolKind::Variable, suffix_to_symbol_kind)
    })
}

fn scip_kind_to_symbol_kind(kind: u32) -> Option<SymbolKind> {
    let scip_kind = IMPORTED_SCIP_KINDS
        .iter()
        .copied()
        .find(|candidate| *candidate as u32 == kind)?;
    SCIP_KIND_RULES
        .iter()
        .find(|rule| rule.scip_kinds.contains(&scip_kind))
        .map(|rule| rule.symbol_kind)
}

fn suffix_to_symbol_kind(suffix: DescriptorSuffix) -> SymbolKind {
    match suffix {
        DescriptorSuffix::Namespace => SymbolKind::Module,
        DescriptorSuffix::Type => SymbolKind::Class,
        DescriptorSuffix::Method => SymbolKind::Method,
        DescriptorSuffix::Parameter | DescriptorSuffix::TypeParameter => SymbolKind::Parameter,
        DescriptorSuffix::Macro => SymbolKind::Function,
        DescriptorSuffix::Term | DescriptorSuffix::Meta => SymbolKind::Variable,
    }
}

fn edge_kind(value: &str) -> Option<EdgeKind> {
    EdgeKind::parse(value)
}

fn relationship_edge_kind(
    relationship: &ScipRelationship,
    source_kind: Option<SymbolKind>,
    target_kind: Option<SymbolKind>,
) -> Option<EdgeKind> {
    if relationship.roles.implementation() {
        match (source_kind, target_kind) {
            (
                Some(SymbolKind::Class | SymbolKind::Struct),
                Some(SymbolKind::Interface | SymbolKind::Trait | SymbolKind::Protocol),
            ) => Some(EdgeKind::Implements),
            _ => Some(EdgeKind::Extends),
        }
    } else if relationship.roles.type_definition() {
        Some(EdgeKind::TypeOf)
    } else if relationship.roles.reference() || relationship.roles.definition() {
        Some(EdgeKind::References)
    } else {
        None
    }
}

fn qualified_name(symbol: &ScipSymbolInformation) -> String {
    let parsed = parse_scip_symbol(&symbol.symbol)
        .map(|parsed| descriptors_to_qualified_name(&parsed.descriptors))
        .unwrap_or_default();
    let value = if parsed.is_empty() {
        if symbol.display_name.is_empty() {
            "(anonymous)"
        } else {
            &symbol.display_name
        }
    } else {
        &parsed
    };
    bounded_text(value, MAXIMUM_QUALIFIED_NAME_BYTES)
}

fn reference_name(occurrence: &ScipOccurrence) -> String {
    parse_scip_symbol(&occurrence.symbol)
        .map(|parsed| descriptors_to_qualified_name(&parsed.descriptors))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| occurrence.symbol.clone())
}

fn scoped_symbol_key(path: &str, symbol: &str) -> String {
    if symbol.starts_with("local ") {
        format!("{path}\0{symbol}")
    } else {
        symbol.to_owned()
    }
}

fn deterministic_symbol_id(key: &str) -> SymbolId {
    SymbolId::from_uuid_v8(uuid_hash(SCIP_SYMBOL_ID_DOMAIN, key))
}

fn deterministic_document_id(key: &str) -> DocumentId {
    DocumentId::from_uuid_v8(uuid_hash(SCIP_DOCUMENT_ID_DOMAIN, key))
}

fn structural_digest(key: &str, kind: SymbolKind, span: &ImportedSpan) -> ContentDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SCIP_STRUCTURAL_DIGEST_DOMAIN);
    hash_text(&mut hasher, key);
    hash_text(&mut hasher, kind.as_str());
    hasher.update(&span.start_byte.to_be_bytes());
    hasher.update(&span.end_byte.to_be_bytes());
    ContentDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn uuid_hash(domain: &[u8], value: &str) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hash_text(&mut hasher, value);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes
}

fn hash_text(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&usize_to_u64(value.len()).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn bounded_documentation(values: &[String]) -> String {
    let joined = values.join("\n\n");
    bounded_text(&joined, MAXIMUM_NATURAL_TEXT_BYTES)
}

fn bounded_text(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
}

fn imported_provenance(value: &str) -> Result<String, ScipError> {
    if value.is_empty() {
        return Ok(SCIP_OVERLAY_PROVENANCE.to_owned());
    }
    if value.starts_with(SCIP_PROVENANCE_PREFIX) {
        return (value.len() <= MAXIMUM_PROVENANCE_BYTES)
            .then(|| value.to_owned())
            .ok_or(ScipError::InvalidData);
    }
    let length = SCIP_PROVENANCE_PREFIX
        .len()
        .checked_add(value.len())
        .ok_or(ScipError::LimitExceeded)?;
    if length > MAXIMUM_PROVENANCE_BYTES {
        return Err(ScipError::InvalidData);
    }
    let mut provenance = String::with_capacity(length);
    provenance.push_str(SCIP_PROVENANCE_PREFIX);
    provenance.push_str(value);
    Ok(provenance)
}

fn document_kind(path: &str) -> DocumentKind {
    let lower = path.to_ascii_lowercase();
    if lower.contains("test") || lower.contains("spec") {
        DocumentKind::Test
    } else {
        DocumentKind::Symbol
    }
}

fn checked_increment(value: u64) -> Result<u64, ScipError> {
    value.checked_add(1).ok_or(ScipError::LimitExceeded)
}

fn poll(cancelled: &mut impl FnMut() -> bool) -> Result<(), ScipError> {
    if cancelled() {
        Err(ScipError::Cancelled)
    } else {
        Ok(())
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use cartograph_db::{FileInput, GenerationValidationLimits, validate_generation_facts};
    use cartograph_domain::{FileParseStatus, GenerationId};

    use crate::{ScipExportOptions, ScipExportOptionsInput, export_snapshot};

    use super::*;

    fn assert_confidence(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= f32::EPSILON,
            "confidence mismatch: expected {expected}, got {actual}"
        );
    }

    fn native_facts(source: &[u8]) -> GenerationFacts {
        let file_id = FileId::from_uuid_v8([1; 16]);
        let file_symbol = SymbolId::from_uuid_v8([2; 16]);
        GenerationFacts {
            files: vec![FileInput {
                file_id: file_id.clone(),
                normalized_path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                content_hash: ContentDigest::from_bytes(*blake3::hash(source).as_bytes()),
                byte_size: usize_to_u64(source.len()),
                parse_status: FileParseStatus::Parsed,
            }],
            symbols: vec![SymbolInput {
                symbol_id: file_symbol.clone(),
                file_id: file_id.clone(),
                symbol_kind: "file".to_owned(),
                qualified_name: "src/lib.rs".to_owned(),
                signature: String::new(),
                start_byte: 0,
                end_byte: usize_to_u64(source.len()),
                start_line: 1,
                end_line: 2,
                structural_digest: ContentDigest::from_bytes(*blake3::hash(source).as_bytes()),
                visibility: None,
                export: cartograph_domain::SymbolExportFlags::default(),
                execution: cartograph_domain::SymbolExecutionFlags::default(),
                declaration_only: false,
                betweenness_ppb: None,
                pagerank_ppb: None,
            }],
            documents: vec![SearchDocumentInput {
                document_id: DocumentId::from_uuid_v8([3; 16]),
                file_id: Some(file_id),
                symbol_id: Some(file_symbol),
                path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                kind: DocumentKind::File,
                qualified_name: String::new(),
                code: "src/lib.rs".to_owned(),
                natural_text: String::new(),
                metadata: json!({}),
            }],
            ..GenerationFacts::default()
        }
    }

    #[test]
    fn cartograph_round_trip_preserves_exact_call_kind_and_site_count() {
        let source = b"fn caller() { callee(); }\nfn callee() {}\n".to_vec();
        let file_id = FileId::from_uuid_v8([1; 16]);
        let source_symbol = SymbolId::from_uuid_v8([4; 16]);
        let target_symbol = SymbolId::from_uuid_v8([5; 16]);
        let snapshot = cartograph_db::InterchangeSnapshot {
            generation_id: GenerationId::from_uuid_v8([6; 16]),
            files: vec![cartograph_db::InterchangeFile {
                file_id: file_id.clone(),
                path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                content_hash: blake3::hash(&source).to_hex().to_string(),
                byte_size: usize_to_u64(source.len()),
            }],
            symbols: vec![
                cartograph_db::InterchangeSymbol {
                    symbol_id: source_symbol.clone(),
                    file_id: file_id.clone(),
                    symbol_kind: "function".to_owned(),
                    qualified_name: "caller".to_owned(),
                    signature: String::new(),
                    code: "caller".to_owned(),
                    natural_text: String::new(),
                    start_byte: 0,
                    end_byte: 25,
                    start_line: 1,
                    end_line: 1,
                },
                cartograph_db::InterchangeSymbol {
                    symbol_id: target_symbol.clone(),
                    file_id,
                    symbol_kind: "function".to_owned(),
                    qualified_name: "callee".to_owned(),
                    signature: String::new(),
                    code: "callee".to_owned(),
                    natural_text: String::new(),
                    start_byte: 26,
                    end_byte: 40,
                    start_line: 2,
                    end_line: 2,
                },
            ],
            edges: vec![cartograph_db::InterchangeEdge {
                source_symbol_id: source_symbol,
                target_symbol_id: target_symbol,
                edge_kind: "calls".to_owned(),
                confidence: 0.9,
                provenance: "native-exact-project".to_owned(),
                site_count: 7,
            }],
            references: Vec::new(),
        };
        let options = ScipExportOptions::new(ScipExportOptionsInput {
            project_name: "demo",
            project_version: "1",
            tool_version: "2",
            project_root_uri: "cartograph://demo",
        })
        .unwrap_or_else(|error| panic!("options failed: {error}"));
        let export = export_snapshot(&snapshot, &options, |_| Some(source.clone()))
            .unwrap_or_else(|error| panic!("export failed: {error}"));
        let mut facts = native_facts(&source);
        let report = apply_scip_overlay(
            ScipOverlayRequest::new(&mut facts, export.bytes(), 100),
            |_| Some(source.clone()),
        )
        .unwrap_or_else(|error| panic!("overlay failed: {error}"));
        assert_eq!(report.exact_typed_edges(), 1);
        let imported_edge = facts
            .edges
            .iter()
            .find(|edge| edge.kind == EdgeKind::Calls && edge.site_count == 7)
            .unwrap_or_else(|| panic!("exact imported call edge was not retained"));
        assert_confidence(imported_edge.confidence, 0.9);
        assert_eq!(
            imported_edge.provenance,
            "scip-overlay:native-exact-project"
        );
        let limits = GenerationValidationLimits::new(64 * 1024 * 1024, 256 * 1024 * 1024)
            .unwrap_or_else(|error| panic!("limits failed: {error}"));
        validate_generation_facts(facts, limits, || false)
            .unwrap_or_else(|error| panic!("validation failed: {error}"));
    }

    #[test]
    fn stale_source_is_skipped_without_replacing_native_facts() {
        let source = b"fn current() {}\n".to_vec();
        let mut facts = native_facts(&source);
        let before = facts.symbols.clone();
        let index = crate::ScipIndex {
            tool_name: "foreign".to_owned(),
            tool_version: "1".to_owned(),
            project_root: String::new(),
            documents: vec![ScipDocument {
                relative_path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                occurrences: Vec::new(),
                symbols: Vec::new(),
            }],
        };
        let bytes = crate::encode_scip_index(&index)
            .unwrap_or_else(|error| panic!("encode failed: {error}"));
        let report = apply_scip_overlay(ScipOverlayRequest::new(&mut facts, &bytes, 10), |_| {
            Some(b"stale".to_vec())
        })
        .unwrap_or_else(|error| panic!("overlay failed: {error}"));
        assert_eq!(report.skipped_documents(), 1);
        assert_eq!(facts.symbols, before);
    }
}
