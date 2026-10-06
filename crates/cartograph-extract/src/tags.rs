use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    mem::size_of,
    ops::ControlFlow,
    sync::OnceLock,
};

use cartograph_domain::{
    ContentDigest, FileParseStatus, ReferenceKind, SourceLanguage, SymbolId, SymbolKind,
    callable_signature_is_literal_free,
};
use tree_sitter::{
    Language, Node, Query, QueryCursor, QueryCursorOptions, StreamingIterator, TreeCursor,
};

use crate::{
    Containment, ExtractError, ExtractedFile, ExtractedReference, ExtractedSymbol,
    ExtractionDiagnostic, SourceSnapshot, SymbolExecutionFlags, SymbolExportFlags,
    SymbolImplementationFlags,
    budget::{
        ExtractionBudget, containment_budget_bytes, diagnostic_budget_bytes, native_output_limit,
        reference_budget_bytes, symbol_budget_bytes,
    },
    identity::SymbolIdentity,
    walk::syntax::{
        SymbolHealthInput, clone_shape_digest, clone_token_profile, collect_diagnostics, span_for,
        symbol_health_metrics,
    },
};

const QUERY_MATCH_LIMIT: u32 = 65_536;
const MINIMUM_CAPTURE_LIMIT: usize = 1_024;
const CAPTURES_PER_SOURCE_BYTE: usize = 8;
const MAX_TAG_RECORDS: usize = 16_384;
const MODELED_TREE_ENTRY_BYTES: usize = 64;
const VECTOR_GROWTH_ALLOWANCE: usize = 2;
const MAX_SIGNATURE_BYTES: usize = 120;
const TRUNCATED_SIGNATURE_BYTES: usize = 117;
const MAX_DOCSTRING_BYTES: usize = 500;
const TRUNCATED_DOCSTRING_BYTES: usize = 497;
const MAX_TAG_AST_DEPTH: usize = 256;
const MINIMUM_TAG_AST_NODES: usize = 1_024;
const TAG_AST_NODES_PER_SOURCE_BYTE: usize = 8;
const MAXIMUM_TAG_AST_NODES: usize = 4 * 1024 * 1024;
const TAG_HASH_CHUNK_BYTES: usize = 64 * 1024;
const TAG_STRUCTURAL_DIGEST_DOMAIN: &str = "cartograph.v2.tags-structural-digest.2026-07-24";

static ELIXIR_QUERY: OnceLock<Query> = OnceLock::new();
static HASKELL_QUERY: OnceLock<Query> = OnceLock::new();
static JULIA_QUERY: OnceLock<Query> = OnceLock::new();
static OCAML_QUERY: OnceLock<Query> = OnceLock::new();
static OCAML_INTERFACE_QUERY: OnceLock<Query> = OnceLock::new();
static OCAML_INTERFACE_SOURCE: OnceLock<String> = OnceLock::new();
static VERILOG_QUERY: OnceLock<Query> = OnceLock::new();

#[derive(Clone, Copy)]
enum RawRole {
    Definition(SymbolKind),
    Call,
    Signature,
}

struct RawMatch<'tree> {
    role: RawRole,
    role_node: Node<'tree>,
    name_node: Node<'tree>,
    doc_node: Option<Node<'tree>>,
}

struct Definition<'tree> {
    kind: SymbolKind,
    name: String,
    name_node: Node<'tree>,
    node: Node<'tree>,
    docstring: Option<String>,
}

struct EmittedDefinition {
    id: SymbolId,
    qualified_name: String,
}

struct CallReference<'tree> {
    name: String,
    role_node: Node<'tree>,
    name_node: Node<'tree>,
}

#[derive(Clone, Copy)]
struct DefinitionInterval {
    start: usize,
    end: usize,
    owner: usize,
    parent: Option<usize>,
}

struct DigestFrame<'tree> {
    node: Node<'tree>,
    next_child: u32,
    child_digest_start: usize,
    child_count: usize,
}

type DefinitionDigests = HashMap<usize, Option<ContentDigest>>;

#[derive(Clone, Copy)]
pub(crate) struct TagExtractionInput<'tree, 'input> {
    pub(crate) snapshot: &'input SourceSnapshot,
    pub(crate) root: Node<'tree>,
    pub(crate) parse_status: FileParseStatus,
    pub(crate) query: &'input Query,
}

#[derive(Clone, Copy)]
struct TagQueryInput<'tree, 'source> {
    query: &'source Query,
    root: Node<'tree>,
    source: &'source str,
    language: SourceLanguage,
}

struct RecordBuildInput<'tree, 'source> {
    raw: Vec<RawMatch<'tree>>,
    source: &'source str,
    language: SourceLanguage,
}

struct PreparedTagRecords<'tree> {
    definitions: Vec<Definition<'tree>>,
    calls: Vec<CallReference<'tree>>,
    parent_indices: Vec<Option<usize>>,
    call_owner_indices: Vec<Option<usize>>,
    structural_digests: DefinitionDigests,
}

struct DefinitionEmissionInput<'tree, 'input> {
    snapshot: &'input SourceSnapshot,
    source: &'input str,
    definitions: Vec<Definition<'tree>>,
    parent_indices: Vec<Option<usize>>,
    structural_digests: DefinitionDigests,
}

struct DefinitionBuildInput<'tree, 'snapshot, 'source, 'parent> {
    snapshot: &'snapshot SourceSnapshot,
    source: &'source str,
    definition: Definition<'tree>,
    parent: Option<&'parent EmittedDefinition>,
    structural_digest: ContentDigest,
}

struct BuiltTagDefinition {
    symbol: ExtractedSymbol,
    emitted: EmittedDefinition,
    parent_id: Option<SymbolId>,
}

struct TagDefinitionFacts {
    symbols: Vec<ExtractedSymbol>,
    containments: Vec<Containment>,
    emitted: Vec<EmittedDefinition>,
}

struct ReferenceEmissionInput<'tree, 'emitted> {
    calls: Vec<CallReference<'tree>>,
    owner_indices: Vec<Option<usize>>,
    emitted: &'emitted [EmittedDefinition],
}

#[derive(Clone, Copy)]
struct StructuralDigestInput<'tree, 'source, 'definitions> {
    root: Node<'tree>,
    source: &'source str,
    definitions: &'definitions [Definition<'tree>],
}

struct NextDigestChildInput<'frame, 'tree> {
    frame: &'frame mut DigestFrame<'tree>,
    visited: &'frame mut usize,
    node_limit: usize,
}

#[derive(Clone, Copy)]
struct FinishTagDigestInput<'frame, 'tree, 'source, 'digest> {
    frame: &'frame DigestFrame<'tree>,
    source: &'source str,
    child_digests: &'digest [[u8; 32]],
}

#[derive(Clone, Copy)]
struct CallOwnerInput<'tree, 'definitions, 'calls> {
    definitions: &'definitions [Definition<'tree>],
    calls: &'calls [CallReference<'tree>],
}

#[derive(Clone, Copy)]
struct DeclarationSignatureInput<'tree, 'source> {
    language: SourceLanguage,
    node: Node<'tree>,
    source: &'source str,
    kind: SymbolKind,
}

struct TagTransientBudget {
    retained: usize,
    maximum: usize,
}

impl TagTransientBudget {
    fn new(snapshot: &SourceSnapshot) -> Result<Self, ExtractError> {
        let maximum = native_output_limit(snapshot.byte_size())
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(ExtractError::OutputLimit)?;
        Ok(Self {
            retained: 0,
            maximum,
        })
    }

    fn charge(&mut self, bytes: usize) -> Result<(), ExtractError> {
        self.retained = self
            .retained
            .checked_add(bytes)
            .ok_or(ExtractError::OutputLimit)?;
        if self.retained > self.maximum {
            return Err(ExtractError::OutputLimit);
        }
        Ok(())
    }

    fn vector_entry<T>(&mut self) -> Result<(), ExtractError> {
        self.charge(size_of::<T>().saturating_mul(VECTOR_GROWTH_ALLOWANCE))
    }

    fn reserve_vector_slot<T>(&mut self, vector: &mut Vec<T>) -> Result<(), ExtractError> {
        if vector.len() < vector.capacity() {
            return Ok(());
        }
        let previous = vector.capacity();
        vector
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.charge(
            vector
                .capacity()
                .saturating_sub(previous)
                .saturating_mul(size_of::<T>()),
        )
    }
}

pub(crate) fn query_for(
    language: SourceLanguage,
    grammar: &Language,
) -> Result<&'static Query, ExtractError> {
    let cell = query_cell(language).ok_or(ExtractError::UnsupportedLanguage)?;
    if let Some(query) = cell.get() {
        return Ok(query);
    }
    let source = query_source(language)?;
    let compiled = Query::new(grammar, source).map_err(|_| ExtractError::GrammarUnavailable)?;
    let _ = cell.set(compiled);
    cell.get().ok_or(ExtractError::GrammarUnavailable)
}

fn query_cell(language: SourceLanguage) -> Option<&'static OnceLock<Query>> {
    match language {
        SourceLanguage::Elixir => Some(&ELIXIR_QUERY),
        SourceLanguage::Haskell => Some(&HASKELL_QUERY),
        SourceLanguage::Julia => Some(&JULIA_QUERY),
        SourceLanguage::Ocaml => Some(&OCAML_QUERY),
        SourceLanguage::OcamlInterface => Some(&OCAML_INTERFACE_QUERY),
        SourceLanguage::Verilog => Some(&VERILOG_QUERY),
        _ => None,
    }
}

fn query_source(language: SourceLanguage) -> Result<&'static str, ExtractError> {
    match language {
        SourceLanguage::Elixir => Ok(include_str!("tags/elixir.scm")),
        SourceLanguage::Haskell => Ok(include_str!("tags/haskell.scm")),
        SourceLanguage::Julia => Ok(include_str!("tags/julia.scm")),
        SourceLanguage::Ocaml => Ok(tree_sitter_ocaml::TAGS_QUERY),
        SourceLanguage::OcamlInterface => ocaml_interface_source(),
        SourceLanguage::Verilog => Ok(include_str!("tags/verilog.scm")),
        _ => Err(ExtractError::UnsupportedLanguage),
    }
}

fn ocaml_interface_source() -> Result<&'static str, ExtractError> {
    if let Some(source) = OCAML_INTERFACE_SOURCE.get() {
        return Ok(source);
    }
    let extra = include_str!("tags/ocaml_interface_extra.scm");
    let mut source = String::new();
    source
        .try_reserve(
            tree_sitter_ocaml::TAGS_QUERY
                .len()
                .saturating_add(extra.len()),
        )
        .map_err(|_| ExtractError::OutputLimit)?;
    source.push_str(tree_sitter_ocaml::TAGS_QUERY);
    source.push_str(extra);
    let _ = OCAML_INTERFACE_SOURCE.set(source);
    OCAML_INTERFACE_SOURCE
        .get()
        .map(String::as_str)
        .ok_or(ExtractError::OutputLimit)
}

pub(crate) fn extract(
    input: TagExtractionInput<'_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<ExtractedFile, ExtractError> {
    let TagExtractionInput {
        snapshot,
        root,
        parse_status,
        query,
    } = input;
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    let source = snapshot.source();
    let mut transient = TagTransientBudget::new(snapshot)?;
    let prepared = prepare_tag_records(
        TagQueryInput {
            query,
            root,
            source,
            language: snapshot.language(),
        },
        &mut transient,
        cancelled,
    )?;
    let PreparedTagRecords {
        definitions,
        calls,
        parent_indices,
        call_owner_indices,
        structural_digests,
    } = prepared;
    let (facts, mut budget) = emit_tag_definitions(
        DefinitionEmissionInput {
            snapshot,
            source,
            definitions,
            parent_indices,
            structural_digests,
        },
        cancelled,
    )?;
    let references = emit_tag_references(
        ReferenceEmissionInput {
            calls,
            owner_indices: call_owner_indices,
            emitted: &facts.emitted,
        },
        &mut budget,
        cancelled,
    )?;

    let diagnostics = diagnostics(root, parse_status, cancelled)?;
    for _ in &diagnostics {
        budget.reserve_fact(diagnostic_budget_bytes(), std::iter::empty())?;
    }
    let output_limit = budget.output_limit();
    let extracted = ExtractedFile {
        file_id: snapshot.file_id().clone(),
        path: snapshot.path().clone(),
        language: snapshot.language(),
        content_hash: snapshot.content_hash().clone(),
        byte_size: snapshot.byte_size(),
        line_count: snapshot.line_count(),
        parse_status,
        symbols: facts.symbols,
        containments: facts.containments,
        references,
        numerical_sites: Vec::new(),
        import_bindings: Vec::new(),
        has_inline_tests: false,
        test_search_text: String::new(),
        test_search_truncated: false,
        diagnostics,
    };
    if extracted.modeled_retained_bytes() > output_limit {
        return Err(ExtractError::OutputLimit);
    }
    Ok(extracted)
}

fn prepare_tag_records<'tree>(
    input: TagQueryInput<'tree, '_>,
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<PreparedTagRecords<'tree>, ExtractError> {
    let raw = collect_matches(input, transient, cancelled)?;
    let (mut definitions, mut calls) = build_records(
        RecordBuildInput {
            raw,
            source: input.source,
            language: input.language,
        },
        transient,
        cancelled,
    )?;
    if definitions.len() > MAX_TAG_RECORDS || calls.len() > MAX_TAG_RECORDS {
        return Err(ExtractError::OutputLimit);
    }
    definitions.sort_unstable_by(|left, right| {
        left.node
            .start_byte()
            .cmp(&right.node.start_byte())
            .then_with(|| right.node.end_byte().cmp(&left.node.end_byte()))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.name.cmp(&right.name))
    });
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    calls.sort_unstable_by(|left, right| {
        left.role_node
            .start_byte()
            .cmp(&right.role_node.start_byte())
            .then_with(|| left.role_node.end_byte().cmp(&right.role_node.end_byte()))
            .then_with(|| left.name.cmp(&right.name))
    });
    if cancelled() {
        return Err(ExtractError::Cancelled);
    }
    let parent_indices = definition_parents(&definitions, transient, cancelled)?;
    let call_owner_indices = call_owners(
        CallOwnerInput {
            definitions: &definitions,
            calls: &calls,
        },
        transient,
        cancelled,
    )?;
    let structural_digests = tag_structural_digests(
        StructuralDigestInput {
            root: input.root,
            source: input.source,
            definitions: &definitions,
        },
        transient,
        cancelled,
    )?;
    Ok(PreparedTagRecords {
        definitions,
        calls,
        parent_indices,
        call_owner_indices,
        structural_digests,
    })
}

fn emit_tag_definitions(
    input: DefinitionEmissionInput<'_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(TagDefinitionFacts, ExtractionBudget), ExtractError> {
    let DefinitionEmissionInput {
        snapshot,
        source,
        definitions,
        parent_indices,
        structural_digests,
    } = input;
    let mut budget = ExtractionBudget::new(snapshot)?;
    let mut identities = SymbolIdentity::new(snapshot.path());
    let mut symbols = Vec::new();
    let mut containments = Vec::new();
    let mut emitted = Vec::<EmittedDefinition>::new();
    symbols
        .try_reserve(definitions.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    emitted
        .try_reserve(definitions.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for (index, definition) in definitions.into_iter().enumerate() {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let parent = parent_indices[index].and_then(|parent| emitted.get(parent));
        // Several definitions may share one role node, such as each dotted
        // segment of a Haskell module header or an Elixir module and the struct
        // it defines, so the digest is read rather than consumed.
        let structural_digest = structural_digests
            .get(&definition.node.id())
            .cloned()
            .flatten()
            .ok_or(ExtractError::GrammarUnavailable)?;
        let built = build_tag_definition(
            DefinitionBuildInput {
                snapshot,
                source,
                definition,
                parent,
                structural_digest,
            },
            &mut identities,
            cancelled,
        )?;
        budget.reserve_fact(
            symbol_budget_bytes(&built.symbol),
            [
                built.symbol.name.as_str(),
                built.symbol.qualified_name.as_str(),
                built.symbol.signature.as_deref().unwrap_or_default(),
                built.symbol.docstring.as_deref().unwrap_or_default(),
            ],
        )?;
        if let Some(parent_id) = built.parent_id {
            let containment = Containment {
                parent: parent_id,
                child: built.emitted.id.clone(),
            };
            budget.reserve_fact(containment_budget_bytes(&containment), std::iter::empty())?;
            containments
                .try_reserve(1)
                .map_err(|_| ExtractError::OutputLimit)?;
            containments.push(containment);
        }
        symbols.push(built.symbol);
        emitted.push(built.emitted);
    }
    Ok((
        TagDefinitionFacts {
            symbols,
            containments,
            emitted,
        },
        budget,
    ))
}

fn build_tag_definition(
    input: DefinitionBuildInput<'_, '_, '_, '_>,
    identities: &mut SymbolIdentity<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<BuiltTagDefinition, ExtractError> {
    let qualified_name = if input.snapshot.language() == SourceLanguage::Julia
        && input.definition.name_node.kind() == "field_expression"
    {
        input.definition.name.clone()
    } else if let Some(parent) = input.parent {
        let mut qualified = String::new();
        qualified
            .try_reserve(
                parent
                    .qualified_name
                    .len()
                    .saturating_add(1)
                    .saturating_add(input.definition.name.len()),
            )
            .map_err(|_| ExtractError::OutputLimit)?;
        qualified.push_str(&parent.qualified_name);
        qualified.push('.');
        qualified.push_str(&input.definition.name);
        qualified
    } else {
        input.definition.name.clone()
    };
    let id = identities.next(input.definition.kind, &qualified_name)?;
    let signature = safe_declaration_signature(DeclarationSignatureInput {
        language: input.snapshot.language(),
        node: input.definition.node,
        source: input.source,
        kind: input.definition.kind,
    })?;
    let health_body = matches!(
        input.definition.kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
    )
    .then_some(input.definition.node);
    let health = symbol_health_metrics(
        SymbolHealthInput {
            declaration: input.definition.node,
            body: health_body,
            symbol_kind: input.definition.kind,
            symbol_name: &input.definition.name,
            signature: signature.as_deref(),
            docstring: input.definition.docstring.as_deref(),
            language: input.snapshot.language(),
            async_symbol: false,
            source: input.source,
        },
        cancelled,
    )?;
    let clone_shape_digest = clone_shape_digest(input.definition.node, cancelled)?;
    let clone_token_profile = matches!(
        input.definition.kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Component
    )
    .then(|| clone_token_profile(input.definition.node, input.source, cancelled))
    .transpose()?
    .flatten();
    let parent_id = input.parent.map(|parent| parent.id.clone());
    let symbol = ExtractedSymbol {
        id: id.clone(),
        kind: input.definition.kind,
        name: input.definition.name,
        qualified_name: qualified_name.clone(),
        span: span_for(input.definition.node)?,
        signature,
        docstring: input.definition.docstring,
        body_search_text: String::new(),
        body_search_truncated: false,
        health,
        implementation: SymbolImplementationFlags::default(),
        export: SymbolExportFlags::default(),
        execution: SymbolExecutionFlags::default(),
        visibility: None,
        structural_digest: input.structural_digest,
        clone_shape_digest,
        clone_token_profile,
    };
    Ok(BuiltTagDefinition {
        symbol,
        emitted: EmittedDefinition { id, qualified_name },
        parent_id,
    })
}

fn emit_tag_references(
    input: ReferenceEmissionInput<'_, '_>,
    budget: &mut ExtractionBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<ExtractedReference>, ExtractError> {
    let mut references = Vec::new();
    references
        .try_reserve(input.calls.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    for (call, owner) in input.calls.into_iter().zip(input.owner_indices) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let reference = ExtractedReference {
            owner: owner.and_then(|index| {
                input
                    .emitted
                    .get(index)
                    .map(|definition| definition.id.clone())
            }),
            name: call.name,
            resolution_name: None,
            kind: ReferenceKind::Calls,
            span: span_for(call.name_node)?,
        };
        budget.reserve_fact(
            reference_budget_bytes(&reference),
            [reference.name.as_str()],
        )?;
        references.push(reference);
    }
    Ok(references)
}

fn collect_matches<'tree>(
    input: TagQueryInput<'tree, '_>,
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<RawMatch<'tree>>, ExtractError> {
    let capture_limit = input
        .source
        .len()
        .saturating_mul(CAPTURES_PER_SOURCE_BYTE)
        .saturating_add(MINIMUM_CAPTURE_LIMIT)
        .min(MAX_TAG_RECORDS);
    let capture_names = input.query.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(QUERY_MATCH_LIMIT);
    let mut interrupted = false;
    let mut collector = RawMatchCollector::new(transient, capture_limit);
    {
        let mut progress = |_state: &tree_sitter::QueryCursorState| {
            if cancelled() {
                interrupted = true;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = QueryCursorOptions::new().progress_callback(&mut progress);
        let mut matches =
            cursor.matches_with_options(input.query, input.root, input.source.as_bytes(), options);
        while let Some(query_match) = matches.next() {
            let mut state = RawCaptureState::default();
            for capture in query_match.captures() {
                let Some(capture_name) = capture_names.get(capture.index as usize) else {
                    return Err(ExtractError::GrammarUnavailable);
                };
                collector.apply_capture(&mut state, capture_name, capture.node)?;
            }
            collector.push_state(state)?;
        }
    }
    if interrupted || cancelled() {
        return Err(ExtractError::Cancelled);
    }
    if cursor.did_exceed_match_limit() {
        return Err(ExtractError::OutputLimit);
    }
    Ok(collector.finish())
}

fn build_records<'tree>(
    input: RecordBuildInput<'tree, '_>,
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(Vec<Definition<'tree>>, Vec<CallReference<'tree>>), ExtractError> {
    let mut definitions = Vec::new();
    let mut calls = Vec::new();
    let mut seen_definitions = BTreeSet::new();
    for mut record in input.raw {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if input.language == SourceLanguage::Julia
            && matches!(record.role, RawRole::Call | RawRole::Signature)
            && let Some(declaration) =
                julia_short_form_definition(record.role_node, input.source, cancelled)?
        {
            record.role = RawRole::Definition(SymbolKind::Function);
            record.role_node = declaration;
        }
        let name = if input.language == SourceLanguage::Julia
            && matches!(record.role, RawRole::Definition(SymbolKind::Function))
        {
            julia_callee_name((record.name_node, input.source), transient, cancelled)?
        } else {
            Cow::Borrowed(node_text(record.name_node, input.source).trim())
        };
        if name.is_empty()
            || name
                .split_whitespace()
                .any(crate::walk::specifier_safety::specifier_may_carry_credential)
        {
            continue;
        }
        match record.role {
            RawRole::Definition(kind) => push_tag_definition(TagDefinitionInput {
                record: &record,
                kind,
                name: &name,
                source: input.source,
                definitions: &mut definitions,
                seen: &mut seen_definitions,
                transient,
            })?,
            RawRole::Call => push_tag_call(TagCallInput {
                record: &record,
                name: &name,
                calls: &mut calls,
                transient,
            })?,
            RawRole::Signature => {}
        }
    }
    Ok((definitions, calls))
}

/// A Julia call signature defines a function only on the left of `=`.
/// Return annotations and chained `where` clauses wrap that same signature.
fn julia_short_form_definition<'tree>(
    node: Node<'tree>,
    source: &str,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<Node<'tree>>, ExtractError> {
    if node.kind() != "call_expression" {
        return Ok(None);
    }
    let [Some(callee), _] = julia_signature_parts(node, cancelled)? else {
        return Ok(None);
    };
    if !julia_named_callee(callee, cancelled)? {
        return Ok(None);
    }
    let mut signature = node;
    for _ in 0..MAX_TAG_AST_DEPTH {
        let Some(parent) = signature.parent() else {
            return Ok(None);
        };
        let [first, operator] = julia_signature_parts(parent, cancelled)?;
        if first != Some(signature) {
            return Ok(None);
        }
        match parent.kind() {
            "typed_expression" | "where_expression" => signature = parent,
            "assignment" => {
                return Ok(operator
                    .filter(|operator| node_text(*operator, source) == "=")
                    .filter(|_| julia_assignment_is_unquoted(parent))
                    .map(|_| parent));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Named extras, including trailing comments, do not occupy signature slots.
/// Every skipped extra polls cancellation, even when no declaration is emitted.
fn julia_signature_parts<'tree>(
    node: Node<'tree>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<[Option<Node<'tree>>; 2], ExtractError> {
    let mut parts = [None, None];
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return Ok(parts);
    }
    loop {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let child = cursor.node();
        if child.is_named() && julia_signature_part(&child) {
            if parts[0].is_none() {
                parts[0] = Some(child);
            } else {
                parts[1] = Some(child);
                break;
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    Ok(parts)
}

fn julia_signature_part(node: &Node<'_>) -> bool {
    !node.is_extra() && !matches!(node.kind(), "line_comment" | "block_comment")
}

/// Comments inside a static callee never enter its declaration identity.
/// Leaf punctuation preserves quoted operators and their parentheses.
fn julia_callee_name<'source>(
    input: (Node<'_>, &'source str),
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Cow<'source, str>, ExtractError> {
    let (node, source) = input;
    let raw = node_text(node, source).trim();
    if !raw.contains('#') {
        return Ok(Cow::Borrowed(raw));
    }
    let mut name = julia_callee_buffer(raw.len(), transient)?;
    let mut cursor = node.walk();
    let maximum_depth = u32::try_from(MAX_TAG_AST_DEPTH).map_err(|_| ExtractError::NestingLimit)?;
    loop {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        if cursor.depth() > maximum_depth {
            return Err(ExtractError::NestingLimit);
        }
        let part = cursor.node();
        if julia_signature_part(&part) {
            if cursor.goto_first_child() {
                continue;
            }
            name.push_str(node_text(part, source));
        }
        if !julia_next_name_part(&mut cursor) {
            break;
        }
    }
    Ok(Cow::Owned(name))
}

fn julia_callee_buffer(
    length: usize,
    transient: &mut TagTransientBudget,
) -> Result<String, ExtractError> {
    transient.charge(length)?;
    let mut name = String::new();
    name.try_reserve_exact(length)
        .map_err(|_| ExtractError::OutputLimit)?;
    Ok(name)
}

fn julia_next_name_part(cursor: &mut TreeCursor<'_>) -> bool {
    while !cursor.goto_next_sibling() {
        if !cursor.goto_parent() {
            return false;
        }
    }
    true
}

fn julia_signature_child<'tree>(
    node: Node<'tree>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<Node<'tree>>, ExtractError> {
    let [first, second] = julia_signature_parts(node, cancelled)?;
    Ok(first.filter(|_| second.is_none()))
}

/// A quoted operator name is a child; quotation around the assignment is data.
fn julia_assignment_is_unquoted(mut node: Node<'_>) -> bool {
    for _ in 0..MAX_TAG_AST_DEPTH {
        let Some(parent) = node.parent() else {
            return true;
        };
        if matches!(parent.kind(), "quote_expression" | "quote_statement") {
            return false;
        }
        node = parent;
    }
    false
}

/// Only static identifier/operator paths can name a short-form declaration.
fn julia_named_callee(
    mut node: Node<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    for _ in 0..MAX_TAG_AST_DEPTH {
        match node.kind() {
            "identifier" | "operator" => return Ok(true),
            "quote_expression" => return julia_quoted_operator(node, cancelled),
            "field_expression" => {
                let [_, Some(field)] = julia_signature_parts(node, cancelled)? else {
                    return Ok(false);
                };
                if field.kind() != "identifier" && !julia_quoted_operator(field, cancelled)? {
                    return Ok(false);
                }
                let Some(value) = node.child_by_field_name("value") else {
                    return Ok(false);
                };
                node = value;
            }
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// A quoted operator may have bounded single-child parentheses around it.
fn julia_quoted_operator(
    node: Node<'_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<bool, ExtractError> {
    if node.kind() != "quote_expression" {
        return Ok(false);
    }
    let Some(mut inner) = julia_signature_child(node, cancelled)? else {
        return Ok(false);
    };
    for _ in 0..MAX_TAG_AST_DEPTH {
        if inner.kind() != "parenthesized_expression" {
            return Ok(inner.kind() == "operator");
        }
        let Some(child) = julia_signature_child(inner, cancelled)? else {
            return Ok(false);
        };
        inner = child;
    }
    Ok(false)
}

struct TagDefinitionInput<'input, 'tree> {
    record: &'input RawMatch<'tree>,
    kind: SymbolKind,
    name: &'input str,
    source: &'input str,
    definitions: &'input mut Vec<Definition<'tree>>,
    seen: &'input mut BTreeSet<(SymbolKind, usize, usize, usize)>,
    transient: &'input mut TagTransientBudget,
}

fn push_tag_definition<'tree>(input: TagDefinitionInput<'_, 'tree>) -> Result<(), ExtractError> {
    let TagDefinitionInput {
        record,
        kind,
        name,
        source,
        definitions,
        seen,
        transient,
    } = input;
    let key = (
        kind,
        record.role_node.id(),
        record.name_node.start_byte(),
        record.name_node.end_byte(),
    );
    if !seen.insert(key) {
        return Ok(());
    }
    transient.charge(MODELED_TREE_ENTRY_BYTES)?;
    let owned_name = owned_tag_name(name, transient)?;
    transient.vector_entry::<Definition<'tree>>()?;
    let docstring = match record.doc_node {
        Some(node) => clean_doc(node_text(node, source))?,
        None => None,
    };
    if let Some(docstring) = &docstring {
        transient.charge(docstring.capacity())?;
    }
    definitions
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    definitions.push(Definition {
        kind,
        name: owned_name,
        name_node: record.name_node,
        node: record.role_node,
        docstring,
    });
    Ok(())
}

struct TagCallInput<'input, 'tree> {
    record: &'input RawMatch<'tree>,
    name: &'input str,
    calls: &'input mut Vec<CallReference<'tree>>,
    transient: &'input mut TagTransientBudget,
}

fn push_tag_call<'tree>(input: TagCallInput<'_, 'tree>) -> Result<(), ExtractError> {
    let TagCallInput {
        record,
        name,
        calls,
        transient,
    } = input;
    let owned_name = owned_tag_name(name, transient)?;
    transient.vector_entry::<CallReference<'tree>>()?;
    calls
        .try_reserve(1)
        .map_err(|_| ExtractError::OutputLimit)?;
    calls.push(CallReference {
        name: owned_name,
        role_node: record.role_node,
        name_node: record.name_node,
    });
    Ok(())
}

fn owned_tag_name(name: &str, transient: &mut TagTransientBudget) -> Result<String, ExtractError> {
    let mut owned = String::new();
    owned
        .try_reserve(name.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    owned.push_str(name);
    transient.charge(owned.capacity())?;
    Ok(owned)
}

fn tag_structural_digests(
    input: StructuralDigestInput<'_, '_, '_>,
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<DefinitionDigests, ExtractError> {
    let mut targets = DefinitionDigests::new();
    targets
        .try_reserve(input.definitions.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    transient.charge(targets.capacity().saturating_mul(MODELED_TREE_ENTRY_BYTES))?;
    for definition in input.definitions {
        targets.entry(definition.node.id()).or_insert(None);
    }

    let node_limit = input
        .source
        .len()
        .saturating_mul(TAG_AST_NODES_PER_SOURCE_BYTE)
        .saturating_add(MINIMUM_TAG_AST_NODES)
        .min(MAXIMUM_TAG_AST_NODES);
    let mut visited = 1_usize;
    let mut frames = Vec::<DigestFrame<'_>>::new();
    let mut child_digests = Vec::<[u8; 32]>::new();
    transient.reserve_vector_slot(&mut frames)?;
    frames.push(DigestFrame {
        node: input.root,
        next_child: 0,
        child_digest_start: 0,
        child_count: 0,
    });

    while !frames.is_empty() {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let next_child = next_digest_child(
            NextDigestChildInput {
                frame: frames.last_mut().ok_or(ExtractError::GrammarUnavailable)?,
                visited: &mut visited,
                node_limit,
            },
            cancelled,
        )?;
        if let Some(child) = next_child {
            if frames.len() > MAX_TAG_AST_DEPTH {
                return Err(ExtractError::NestingLimit);
            }
            let child_digest_start = child_digests.len();
            transient.reserve_vector_slot(&mut frames)?;
            frames.push(DigestFrame {
                node: child,
                next_child: 0,
                child_digest_start,
                child_count: 0,
            });
            continue;
        }

        let frame = frames.pop().ok_or(ExtractError::GrammarUnavailable)?;
        let digest = finish_tag_structural_digest(
            FinishTagDigestInput {
                frame: &frame,
                source: input.source,
                child_digests: &child_digests,
            },
            cancelled,
        )?;
        if let Some(target) = targets.get_mut(&frame.node.id()) {
            *target = Some(ContentDigest::from_bytes(digest));
        }
        child_digests.truncate(frame.child_digest_start);
        transient.reserve_vector_slot(&mut child_digests)?;
        child_digests.push(digest);
    }

    if child_digests.len() != 1 || targets.values().any(Option::is_none) {
        return Err(ExtractError::GrammarUnavailable);
    }
    Ok(targets)
}

fn next_digest_child<'tree>(
    input: NextDigestChildInput<'_, 'tree>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Option<Node<'tree>>, ExtractError> {
    let NextDigestChildInput {
        frame,
        visited,
        node_limit,
    } = input;
    while frame.next_child < frame.node.child_count() {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let child_index = frame.next_child;
        frame.next_child = frame.next_child.saturating_add(1);
        *visited = visited.checked_add(1).ok_or(ExtractError::OutputLimit)?;
        if *visited > node_limit {
            return Err(ExtractError::OutputLimit);
        }
        let Some(child) = frame.node.child(child_index) else {
            continue;
        };
        if is_comment_node(child) {
            continue;
        }
        frame.child_count = frame.child_count.saturating_add(1);
        return Ok(Some(child));
    }
    Ok(None)
}

fn finish_tag_structural_digest(
    input: FinishTagDigestInput<'_, '_, '_, '_>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<[u8; 32], ExtractError> {
    let children = input
        .child_digests
        .get(input.frame.child_digest_start..)
        .ok_or(ExtractError::GrammarUnavailable)?;
    if children.len() != input.frame.child_count {
        return Err(ExtractError::GrammarUnavailable);
    }
    let mut hasher = blake3::Hasher::new_derive_key(TAG_STRUCTURAL_DIGEST_DOMAIN);
    hash_tag_field(&mut hasher, input.frame.node.kind().as_bytes());
    hasher.update(
        &u64::try_from(input.frame.child_count)
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    if input.frame.child_count == 0 {
        hash_tag_cancellable_field(
            &mut hasher,
            node_text(input.frame.node, input.source).as_bytes(),
            cancelled,
        )?;
    } else {
        for digest in children {
            if cancelled() {
                return Err(ExtractError::Cancelled);
            }
            hasher.update(digest);
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

fn hash_tag_field(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(field);
}

fn hash_tag_cancellable_field(
    hasher: &mut blake3::Hasher,
    field: &[u8],
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(), ExtractError> {
    hasher.update(&u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
    for chunk in field.chunks(TAG_HASH_CHUNK_BYTES) {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        hasher.update(chunk);
    }
    Ok(())
}

fn is_comment_node(node: Node<'_>) -> bool {
    node.kind().contains("comment")
}

#[derive(Clone, Copy, Default)]
struct RawCaptureState<'tree> {
    role: Option<RawRole>,
    role_node: Option<Node<'tree>>,
    name_node: Option<Node<'tree>>,
    doc_node: Option<Node<'tree>>,
}

struct RawMatchCollector<'tree, 'budget> {
    raw: Vec<RawMatch<'tree>>,
    ignored_node_ids: BTreeSet<usize>,
    definition_name_ids: BTreeSet<usize>,
    transient: &'budget mut TagTransientBudget,
    capture_limit: usize,
}

impl<'tree, 'budget> RawMatchCollector<'tree, 'budget> {
    fn new(transient: &'budget mut TagTransientBudget, capture_limit: usize) -> Self {
        Self {
            raw: Vec::new(),
            ignored_node_ids: BTreeSet::new(),
            definition_name_ids: BTreeSet::new(),
            transient,
            capture_limit,
        }
    }

    fn apply_capture(
        &mut self,
        state: &mut RawCaptureState<'tree>,
        capture_name: &str,
        node: Node<'tree>,
    ) -> Result<(), ExtractError> {
        match capture_name {
            "ignore" => {
                if self.ignored_node_ids.insert(node.id()) {
                    self.transient.charge(MODELED_TREE_ENTRY_BYTES)?;
                }
                if self.ignored_node_ids.len() > self.capture_limit {
                    return Err(ExtractError::OutputLimit);
                }
            }
            "name" => state.name_node = Some(node),
            "doc" => state.doc_node = Some(node),
            name if name.starts_with("definition.") => {
                state.role = definition_kind(&name["definition.".len()..]).map(RawRole::Definition);
                state.role_node = Some(node);
            }
            "reference.call" => {
                state.role = Some(RawRole::Call);
                state.role_node = Some(node);
            }
            "reference.signature" => {
                state.role = Some(RawRole::Signature);
                state.role_node = Some(node);
            }
            _ => {}
        }
        Ok(())
    }

    fn push_state(&mut self, state: RawCaptureState<'tree>) -> Result<(), ExtractError> {
        if matches!(state.role, Some(RawRole::Definition(_)))
            && let Some(name_node) = state.name_node
            && self.definition_name_ids.insert(name_node.id())
        {
            self.transient.charge(MODELED_TREE_ENTRY_BYTES)?;
            if self.definition_name_ids.len() > self.capture_limit {
                return Err(ExtractError::OutputLimit);
            }
        }
        let (Some(role), Some(role_node), Some(name_node)) =
            (state.role, state.role_node, state.name_node)
        else {
            return Ok(());
        };
        if self.raw.len() >= self.capture_limit
            || self.ignored_node_ids.len() > self.capture_limit
            || self.definition_name_ids.len() > self.capture_limit
        {
            return Err(ExtractError::OutputLimit);
        }
        self.transient.vector_entry::<RawMatch<'tree>>()?;
        self.raw
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        self.raw.push(RawMatch {
            role,
            role_node,
            name_node,
            doc_node: state.doc_node,
        });
        Ok(())
    }

    fn finish(mut self) -> Vec<RawMatch<'tree>> {
        self.raw.retain(|record| {
            let ignored = self.ignored_node_ids.contains(&record.name_node.id());
            let definition_self_match = matches!(record.role, RawRole::Call)
                && self.definition_name_ids.contains(&record.name_node.id());
            !ignored && !definition_self_match
        });
        self.raw
    }
}

const DEFINITION_KINDS: &[(&str, SymbolKind)] = &[
    ("class", SymbolKind::Class),
    ("function", SymbolKind::Function),
    ("macro", SymbolKind::Function),
    ("operator", SymbolKind::Function),
    ("method", SymbolKind::Method),
    ("module", SymbolKind::Module),
    ("interface", SymbolKind::Interface),
    ("constant", SymbolKind::Constant),
    ("struct", SymbolKind::Struct),
    ("type", SymbolKind::TypeAlias),
    ("enum", SymbolKind::Enum),
    ("enum_variant", SymbolKind::EnumMember),
    ("field", SymbolKind::Field),
    ("variable", SymbolKind::Variable),
    ("namespace", SymbolKind::Namespace),
    ("protocol", SymbolKind::Protocol),
    ("trait", SymbolKind::Trait),
];

fn definition_kind(suffix: &str) -> Option<SymbolKind> {
    DEFINITION_KINDS
        .iter()
        .find_map(|(candidate, kind)| (*candidate == suffix).then_some(*kind))
}

fn definition_parents(
    definitions: &[Definition<'_>],
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<Option<usize>>, ExtractError> {
    let mut parents = Vec::new();
    let mut active = Vec::<DefinitionInterval>::new();
    parents
        .try_reserve(definitions.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    active
        .try_reserve(definitions.len().min(MAX_TAG_RECORDS))
        .map_err(|_| ExtractError::OutputLimit)?;
    transient.charge(
        definitions
            .len()
            .saturating_mul(size_of::<Option<usize>>() + size_of::<DefinitionInterval>()),
    )?;
    for (index, definition) in definitions.iter().enumerate() {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let parent = definition_parent(&mut active, definition, index);
        parents.push(parent);
    }
    Ok(parents)
}

fn definition_parent(
    active: &mut Vec<DefinitionInterval>,
    definition: &Definition<'_>,
    owner: usize,
) -> Option<usize> {
    let start = definition.node.start_byte();
    let end = definition.node.end_byte();
    while active
        .last()
        .is_some_and(|interval| interval.start > start || interval.end < end)
    {
        active.pop();
    }
    let parent = active.last().and_then(|interval| {
        if interval.start == start && interval.end == end {
            interval.parent
        } else {
            Some(interval.owner)
        }
    });
    let duplicate = active
        .last()
        .is_some_and(|interval| interval.start == start && interval.end == end);
    if !duplicate {
        active.push(DefinitionInterval {
            start,
            end,
            owner,
            parent,
        });
    }
    parent
}

fn call_owners(
    input: CallOwnerInput<'_, '_, '_>,
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<Option<usize>>, ExtractError> {
    let intervals = definition_intervals(input.definitions, transient, cancelled)?;

    let mut owners = Vec::new();
    let mut active = Vec::<DefinitionInterval>::new();
    owners
        .try_reserve(input.calls.len())
        .map_err(|_| ExtractError::OutputLimit)?;
    active
        .try_reserve(intervals.len().min(MAX_TAG_RECORDS))
        .map_err(|_| ExtractError::OutputLimit)?;
    transient.charge(
        input
            .calls
            .len()
            .saturating_mul(size_of::<Option<usize>>())
            .saturating_add(
                intervals
                    .len()
                    .saturating_mul(size_of::<DefinitionInterval>()),
            ),
    )?;
    let mut next_interval = 0;
    let mut traversal = CallOwnerTraversal {
        intervals: &intervals,
        active: &mut active,
        next_interval: &mut next_interval,
    };
    for call in input.calls {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        owners.push(traversal.owner_for(call));
    }
    Ok(owners)
}

fn definition_intervals(
    definitions: &[Definition<'_>],
    transient: &mut TagTransientBudget,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<DefinitionInterval>, ExtractError> {
    let mut intervals = Vec::new();
    for (owner, definition) in definitions.iter().enumerate() {
        if cancelled() {
            return Err(ExtractError::Cancelled);
        }
        let interval = DefinitionInterval {
            start: definition.node.start_byte(),
            end: definition.node.end_byte(),
            owner,
            parent: None,
        };
        let duplicate = intervals.last().is_some_and(|last: &DefinitionInterval| {
            last.start == interval.start && last.end == interval.end
        });
        if duplicate {
            continue;
        }
        transient.vector_entry::<DefinitionInterval>()?;
        intervals
            .try_reserve(1)
            .map_err(|_| ExtractError::OutputLimit)?;
        intervals.push(interval);
    }
    Ok(intervals)
}

struct CallOwnerTraversal<'a> {
    intervals: &'a [DefinitionInterval],
    active: &'a mut Vec<DefinitionInterval>,
    next_interval: &'a mut usize,
}

impl CallOwnerTraversal<'_> {
    fn owner_for(&mut self, call: &CallReference<'_>) -> Option<usize> {
        let call_start = call.role_node.start_byte();
        let call_end = call.role_node.end_byte();
        while self
            .intervals
            .get(*self.next_interval)
            .is_some_and(|interval| interval.start <= call_start)
        {
            let interval = self.intervals[*self.next_interval];
            while self
                .active
                .last()
                .is_some_and(|current| current.end <= interval.start)
            {
                self.active.pop();
            }
            self.active.push(interval);
            *self.next_interval = self.next_interval.saturating_add(1);
        }
        while self
            .active
            .last()
            .is_some_and(|interval| !strictly_contains_interval(*interval, call_start, call_end))
        {
            self.active.pop();
        }
        self.active.last().map(|interval| interval.owner)
    }
}

fn strictly_contains_interval(interval: DefinitionInterval, start: usize, end: usize) -> bool {
    interval.start <= start && interval.end >= end && (interval.start < start || interval.end > end)
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source
        .get(node.start_byte()..node.end_byte())
        .unwrap_or_default()
}

fn safe_declaration_signature(
    input: DeclarationSignatureInput<'_, '_>,
) -> Result<Option<String>, ExtractError> {
    if !matches!(input.kind, SymbolKind::Function | SymbolKind::Method) {
        return Ok(None);
    }
    let Some(header) = declaration_signature_header(input) else {
        return Ok(None);
    };
    if header
        .split_whitespace()
        .any(crate::walk::specifier_safety::specifier_may_carry_credential)
    {
        return Ok(None);
    }
    let bounded = truncate_with_ellipsis(header, MAX_SIGNATURE_BYTES, TRUNCATED_SIGNATURE_BYTES)?;
    let contains_unmodeled_literal = bounded
        .bytes()
        .any(|byte| matches!(byte, b'\\' | b'%' | b'{' | b'}'))
        || contains_colon_atom(&bounded);
    Ok(
        (!contains_unmodeled_literal && callable_signature_is_literal_free(&bounded))
            .then_some(bounded),
    )
}

fn declaration_signature_header<'source>(
    input: DeclarationSignatureInput<'_, 'source>,
) -> Option<&'source str> {
    let line = node_text(input.node, input.source)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let header = match input.language {
        SourceLanguage::Elixir => {
            let without_inline_body = line.split_once(", do:").map_or(line, |(header, _)| header);
            without_inline_body
                .strip_suffix(" do")
                .unwrap_or(without_inline_body)
        }
        SourceLanguage::Haskell => line.split_once('=').map_or(line, |(header, _)| header),
        SourceLanguage::Julia => callable_prefix_through_parameters(line)?,
        SourceLanguage::Ocaml => line.split_once('=').map_or(line, |(header, _)| header),
        SourceLanguage::OcamlInterface => line,
        SourceLanguage::Verilog => line.split_once(';').map_or(line, |(header, _)| header),
        _ => return None,
    };
    Some(header.trim())
}

fn callable_prefix_through_parameters(line: &str) -> Option<&str> {
    let end = line.find(')')?;
    // A partial line or comment-bearing header cannot establish a safe signature.
    line.get(..=end).filter(|header| !header.contains('#'))
}

fn contains_colon_atom(value: &str) -> bool {
    value
        .as_bytes()
        .array_windows()
        .any(|&[colon, next]| colon == b':' && (next.is_ascii_alphabetic() || next == b'_'))
}

fn clean_doc(raw: &str) -> Result<Option<String>, ExtractError> {
    if raw
        .split_whitespace()
        .any(crate::walk::specifier_safety::specifier_may_carry_credential)
    {
        return Ok(None);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    truncate_with_ellipsis(trimmed, MAX_DOCSTRING_BYTES, TRUNCATED_DOCSTRING_BYTES).map(Some)
}

fn truncate_with_ellipsis(
    value: &str,
    maximum: usize,
    prefix: usize,
) -> Result<String, ExtractError> {
    let mut output = String::new();
    output
        .try_reserve(value.len().min(maximum))
        .map_err(|_| ExtractError::OutputLimit)?;
    if value.len() <= maximum {
        output.push_str(value);
        return Ok(output);
    }
    output.push_str(&value[..value.floor_char_boundary(prefix)]);
    output.push_str("...");
    Ok(output)
}

fn diagnostics(
    root: Node<'_>,
    parse_status: FileParseStatus,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<ExtractionDiagnostic>, ExtractError> {
    if parse_status == FileParseStatus::Partial {
        let diagnostics = collect_diagnostics(root, cancelled)?;
        if diagnostics.is_empty() {
            return Ok(vec![ExtractionDiagnostic {
                code: crate::DiagnosticCode::SyntaxError,
                span: None,
            }]);
        }
        Ok(diagnostics)
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_julia(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::NativeGrammar::Julia.language())
            .unwrap_or_else(|error| panic!("Julia grammar failed: {error}"));
        parser
            .parse(source, None)
            .unwrap_or_else(|| panic!("Julia parse failed"))
    }

    #[test]
    fn julia_signature_comment_runs_poll_cancellation() {
        let source = format!("f(x) {} = helper(x)\n", "#==# ".repeat(10_000));
        let tree = parsed_julia(&source);
        let assignment = tree
            .root_node()
            .named_child(0)
            .unwrap_or_else(|| panic!("missing assignment"));
        let signature = assignment
            .named_child(0)
            .unwrap_or_else(|| panic!("missing signature"));
        assert_eq!(signature.kind(), "call_expression");
        assert_eq!(
            julia_short_form_definition(signature, &source, &mut || false)
                .unwrap_or_else(|error| panic!("classification failed: {error}")),
            Some(assignment)
        );
        let mut polls = 0;
        let result = julia_short_form_definition(signature, &source, &mut || {
            polls += 1;
            polls == 8
        });
        assert!(matches!(result, Err(ExtractError::Cancelled)));
        assert_eq!(polls, 8);
    }

    #[test]
    fn julia_commented_callee_name_obeys_the_ast_depth_limit() {
        let source = format!(
            "{}#= note =# leaf(x) = helper(x)\n",
            "Root.".repeat(MAX_TAG_AST_DEPTH + 1)
        );
        let tree = parsed_julia(&source);
        let callee = tree
            .root_node()
            .named_child(0)
            .and_then(|assignment| assignment.named_child(0))
            .and_then(|call| call.named_child(0))
            .unwrap_or_else(|| panic!("missing callee"));
        assert_eq!(callee.kind(), "field_expression");
        let mut transient = TagTransientBudget {
            retained: 0,
            maximum: source.len() * 2,
        };
        let result = julia_callee_name((callee, &source), &mut transient, &mut || false);
        assert!(matches!(result, Err(ExtractError::NestingLimit)));
    }

    #[test]
    fn every_tags_query_compiles_against_its_exact_native_grammar() {
        for (language, expected_query_digest) in [
            (
                SourceLanguage::Elixir,
                "798a26b714be320983c330a656b5f56c8c67ea80436b4ff555dde346e0d21603",
            ),
            (
                SourceLanguage::Haskell,
                "6645f764d60813208f762857a8b30f47fab4b5f01acb5d26005256efc62285ce",
            ),
            (
                SourceLanguage::Julia,
                // Six added lines capture qualified/operator short-form signatures.
                // Dropping the quoted-callee alternative restores the round-1
                // aac7264a... bytes; its four new operator declarations are regression-tested.
                // Removing them restores the original 91f15fa5... query byte for byte;
                // the v1 corpus has no further Julia fact changes from these captures.
                "adca979b387065d5d7e0a96c1a4ca2848f5f91edab5da947509a4836d576f244",
            ),
            (
                SourceLanguage::Ocaml,
                "a553884ce18e3dfab3aa9c1051caf2d8b84164fba0a4fd542f3d3e9d333a438d",
            ),
            (
                SourceLanguage::OcamlInterface,
                "204ace3a5601871e166eb630c87844c5211ac5bab96c16e7aef4bf712de316c5",
            ),
            (
                SourceLanguage::Verilog,
                "68b5c86c7a875ee38e9375c00657d33ebcc524ddefb5ee3f1879fa4109fd0c06",
            ),
        ] {
            let source = query_source(language)
                .unwrap_or_else(|error| panic!("query source missing: {error}"));
            assert_eq!(
                blake3::hash(source.as_bytes()).to_hex().as_str(),
                expected_query_digest,
                "{} query drifted from its reviewed bytes",
                language.as_str()
            );
            let grammar = crate::NativeGrammar::for_source_language(language)
                .unwrap_or_else(|| panic!("{} grammar mapping is absent", language.as_str()))
                .language();
            query_for(language, &grammar)
                .unwrap_or_else(|error| panic!("{} tags query failed: {error}", language.as_str()));
        }
    }
}
