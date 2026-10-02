#[cfg(test)]
use super::legacy_json::has_valid_extraction_errors;
use super::*;

pub(super) fn require_source_count(
    observed: usize,
    expected: u64,
    field: &'static str,
) -> Result<(), V1PostgresImportError> {
    (usize_to_u64(observed)? == expected).ok_or(invalid_source(field))
}

struct FactMapping<'a> {
    source: &'a SourceSnapshot,
    file_ids: BTreeMap<String, FileId>,
    symbol_ids: BTreeMap<String, SymbolId>,
    nodes: BTreeMap<String, &'a SourceNode>,
    symbol_spans: BTreeMap<String, (usize, usize)>,
}

struct MappedSymbol {
    symbol: SymbolInput,
    document: SearchDocumentInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReferenceAnchor {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) precision: crate::ReferenceSpanPrecision,
}

#[derive(Clone, Copy)]
pub(super) struct ReferenceSite {
    pub(super) line: u32,
    pub(super) column: Option<u32>,
}

#[derive(Clone, Copy)]
struct ReferenceAnchorInput<'a> {
    file: &'a SourceFile,
    owner_span: (usize, usize),
    site: ReferenceSite,
    reference_name: &'a str,
}

impl<'a> ReferenceAnchorInput<'a> {
    const fn new(file: &'a SourceFile, owner_span: (usize, usize), site: ReferenceSite) -> Self {
        Self {
            file,
            owner_span,
            site,
            reference_name: "",
        }
    }

    const fn with_name(mut self, reference_name: &'a str) -> Self {
        self.reference_name = reference_name;
        self
    }
}

struct ReferenceEvidence<'a> {
    owner_legacy_id: &'a str,
    target_legacy_id: Option<&'a str>,
    file_path: &'a str,
    reference_name: &'a str,
    reference_kind: &'a str,
    confidence: f32,
    provenance: String,
    total_sites: u32,
    sites: Vec<ReferenceSite>,
}

pub(super) fn map_source_facts(
    source: &SourceSnapshot,
    deadline: Instant,
) -> Result<GenerationFacts, V1PostgresImportError> {
    let mapping = FactMapping::new(source, deadline)?;
    let files = source
        .files
        .iter()
        .map(|(path, file)| {
            ensure_analysis_active(deadline)?;
            map_file_fact(&mapping, path, file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mapped_symbols = source
        .nodes
        .iter()
        .map(|node| {
            ensure_analysis_active(deadline)?;
            map_symbol_fact(&mapping, node)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let edges = source
        .edges
        .iter()
        .map(|edge| {
            ensure_analysis_active(deadline)?;
            map_edge_fact(&mapping, edge)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut references = Vec::new();
    for edge in &source.edges {
        ensure_analysis_active(deadline)?;
        references.extend(map_edge_reference_facts(&mapping, edge)?);
    }
    for reference in &source.references {
        ensure_analysis_active(deadline)?;
        references.extend(map_reference_facts(&mapping, reference)?);
    }
    let (symbols, mut documents): (Vec<_>, Vec<_>) = mapped_symbols
        .into_iter()
        .map(|mapped| (mapped.symbol, mapped.document))
        .unzip();
    for (path, file) in &source.files {
        ensure_analysis_active(deadline)?;
        documents.push(map_file_document(&mapping, path, file)?);
    }
    Ok(GenerationFacts {
        files,
        symbols,
        edges,
        references,
        numerical_sites: Vec::new(),
        documents,
    })
}

impl<'a> FactMapping<'a> {
    fn new(source: &'a SourceSnapshot, deadline: Instant) -> Result<Self, V1PostgresImportError> {
        let file_ids = source
            .files
            .keys()
            .map(|path| {
                (
                    path.clone(),
                    FileId::from_uuid_v8(deterministic_uuid_bytes(b"file", path.as_bytes())),
                )
            })
            .collect();
        let mut symbol_ids = BTreeMap::new();
        let mut nodes = BTreeMap::new();
        let mut symbol_spans = BTreeMap::new();
        for node in &source.nodes {
            ensure_analysis_active(deadline)?;
            let file = source
                .files
                .get(&node.file_path)
                .ok_or_else(|| invalid_source("node_file_relation"))?;
            if file.language != node.language {
                return Err(invalid_source("node_language"));
            }
            let symbol_id = SymbolId::from_uuid_v8(deterministic_uuid_bytes(
                b"symbol",
                node.legacy_id.as_bytes(),
            ));
            if symbol_ids
                .insert(node.legacy_id.clone(), symbol_id)
                .is_some()
                || nodes.insert(node.legacy_id.clone(), node).is_some()
                || symbol_spans
                    .insert(node.legacy_id.clone(), source_span(file, node)?)
                    .is_some()
            {
                return Err(invalid_source("duplicate_node_id"));
            }
        }
        Ok(Self {
            source,
            file_ids,
            symbol_ids,
            nodes,
            symbol_spans,
        })
    }
}

fn map_file_fact(
    mapping: &FactMapping<'_>,
    path: &str,
    file: &SourceFile,
) -> Result<FileInput, V1PostgresImportError> {
    Ok(FileInput {
        file_id: mapping
            .file_ids
            .get(path)
            .ok_or_else(|| invalid_source("file_identity"))?
            .clone(),
        normalized_path: path.to_owned(),
        language: file.language.clone(),
        content_hash: file.content_hash.clone(),
        byte_size: usize_to_u64(file.source.len())?,
        parse_status: if file.has_extraction_errors {
            FileParseStatus::Partial
        } else {
            FileParseStatus::Parsed
        },
    })
}

fn map_symbol_fact(
    mapping: &FactMapping<'_>,
    node: &SourceNode,
) -> Result<MappedSymbol, V1PostgresImportError> {
    let file = mapping
        .source
        .files
        .get(&node.file_path)
        .ok_or_else(|| invalid_source("node_file_relation"))?;
    let file_id = mapped_file_id(mapping, &node.file_path)?;
    let symbol_id = mapping
        .symbol_ids
        .get(&node.legacy_id)
        .ok_or_else(|| invalid_source("symbol_identity"))?
        .clone();
    let (start_byte, end_byte) = *mapping
        .symbol_spans
        .get(&node.legacy_id)
        .ok_or_else(|| invalid_source("node_span"))?;
    let symbol_kind = SymbolKind::from_stable_str(&node.kind);
    let signature = node
        .signature
        .as_deref()
        .filter(|value| {
            symbol_kind.is_some_and(|kind| symbol_signature_is_search_safe(kind, value))
        })
        .unwrap_or_default()
        .to_owned();
    let code = if signature.is_empty() {
        node.qualified_name.clone()
    } else {
        signature.clone()
    };
    Ok(MappedSymbol {
        symbol: SymbolInput {
            symbol_id: symbol_id.clone(),
            file_id: file_id.clone(),
            symbol_kind: node.kind.clone(),
            qualified_name: node.qualified_name.clone(),
            signature: signature.clone(),
            start_byte: usize_to_u64(start_byte)?,
            end_byte: usize_to_u64(end_byte)?,
            start_line: node.start_line,
            end_line: node.end_line,
            structural_digest: imported_structural_digest(node, &signature),
            visibility: match node.visibility.as_deref() {
                Some(value) => Some(
                    Visibility::from_stable_str(value)
                        .ok_or_else(|| invalid_source("visibility"))?,
                ),
                None => None,
            },
            export: cartograph_domain::SymbolExportFlags::new(
                node.export.exported,
                node.export.default_export,
            ),
            execution: cartograph_domain::SymbolExecutionFlags {
                async_symbol: node.execution.async_symbol,
                static_member: node.execution.static_member,
            },
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        },
        document: SearchDocumentInput {
            document_id: DocumentId::from_uuid_v8(deterministic_uuid_bytes(
                b"document",
                node.legacy_id.as_bytes(),
            )),
            file_id: Some(file_id),
            symbol_id: Some(symbol_id),
            path: node.file_path.clone(),
            language: node.language.clone(),
            kind: imported_document_kind(file),
            qualified_name: node.qualified_name.clone(),
            code,
            natural_text: node.docstring.clone().unwrap_or_default(),
            metadata: serde_json::json!({
                "async": node.execution.async_symbol,
                "body_search_truncated": false,
                "declaration_only": false,
                "default_export": node.export.default_export,
                "exported": node.export.exported,
                "legacy_body_search_omitted": true,
                "legacy_body_search_omission_reason": "privacy_unproven",
                "name": node.name,
                "static": node.execution.static_member,
                "visibility": node.visibility,
            }),
        },
    })
}

fn imported_structural_digest(node: &SourceNode, signature: &str) -> ContentDigest {
    let mut structural = blake3::Hasher::new();
    for value in [
        node.kind.as_bytes(),
        node.qualified_name.as_bytes(),
        signature.as_bytes(),
        node.body_hash.as_bytes(),
    ] {
        hash_field(&mut structural, value);
    }
    ContentDigest::from_bytes(*structural.finalize().as_bytes())
}

fn imported_document_kind(file: &SourceFile) -> DocumentKind {
    if file.is_test {
        DocumentKind::Test
    } else {
        DocumentKind::Symbol
    }
}

fn map_file_document(
    mapping: &FactMapping<'_>,
    path: &str,
    file: &SourceFile,
) -> Result<SearchDocumentInput, V1PostgresImportError> {
    let file_id = mapped_file_id(mapping, path)?;
    let parse_status = if file.has_extraction_errors {
        FileParseStatus::Partial
    } else {
        FileParseStatus::Parsed
    };
    Ok(SearchDocumentInput {
        document_id: DocumentId::from_uuid_v8(deterministic_uuid_bytes(
            b"file-document",
            path.as_bytes(),
        )),
        file_id: Some(file_id),
        symbol_id: None,
        path: path.to_owned(),
        language: file.language.clone(),
        kind: if file.is_test {
            DocumentKind::Test
        } else {
            DocumentKind::File
        },
        qualified_name: String::new(),
        code: path.to_owned(),
        natural_text: String::new(),
        metadata: serde_json::json!({
            "byte_size": file.source.len(),
            "parse_status": parse_status.as_str(),
        }),
    })
}

fn map_edge_fact(
    mapping: &FactMapping<'_>,
    edge: &SourceEdge,
) -> Result<EdgeInput, V1PostgresImportError> {
    Ok(EdgeInput {
        source_symbol_id: mapped_symbol_id(mapping, &edge.source, "edge_source")?,
        target_symbol_id: mapped_symbol_id(mapping, &edge.target, "edge_target")?,
        kind: parse_edge_kind(&edge.kind)?,
        confidence: edge_confidence(edge)?,
        provenance: edge_provenance(edge),
        site_count: edge_site_count(edge)?,
    })
}

fn map_reference_facts(
    mapping: &FactMapping<'_>,
    reference: &SourceReference,
) -> Result<Vec<ReferenceInput>, V1PostgresImportError> {
    let file = mapping
        .source
        .files
        .get(&reference.file_path)
        .ok_or_else(|| invalid_source("reference_file_relation"))?;
    if file.language != reference.language {
        return Err(invalid_source("reference_language"));
    }
    let mut sites = vec![ReferenceSite {
        line: reference.line,
        column: Some(reference.column),
    }];
    for line in &reference.extra_lines {
        if !sites.iter().any(|site| site.line == *line) {
            sites.push(ReferenceSite {
                line: *line,
                column: None,
            });
        }
    }
    map_reference_evidence(
        mapping,
        ReferenceEvidence {
            owner_legacy_id: &reference.from_node_id,
            target_legacy_id: None,
            file_path: &reference.file_path,
            reference_name: &reference.reference_name,
            reference_kind: &reference.reference_kind,
            confidence: UNRESOLVED_CONFIDENCE,
            provenance: unresolved_reference_provenance(reference),
            total_sites: reference.site_count,
            sites,
        },
    )
}

fn map_edge_reference_facts(
    mapping: &FactMapping<'_>,
    edge: &SourceEdge,
) -> Result<Vec<ReferenceInput>, V1PostgresImportError> {
    let owner = mapped_node(mapping, &edge.source, "edge_source")?;
    let target = mapped_node(mapping, &edge.target, "edge_target")?;
    let mut sites = Vec::new();
    let reference_name;
    if edge.kind == "def_use" && !edge.metadata.def_use_lines.is_empty() {
        let Some(def_use_name) = edge.metadata.def_use_name.as_deref() else {
            return Ok(Vec::new());
        };
        reference_name = def_use_name;
        for line in &edge.metadata.def_use_lines {
            sites.push(ReferenceSite {
                line: *line,
                column: None,
            });
        }
    } else {
        reference_name = &target.name;
        if let Some(line) = edge.line {
            sites.push(ReferenceSite {
                line,
                column: edge.column,
            });
        }
        for line in &edge.metadata.extra_lines {
            if !sites.iter().any(|site| site.line == *line) {
                sites.push(ReferenceSite {
                    line: *line,
                    column: None,
                });
            }
        }
    }
    if sites.is_empty() {
        return Ok(Vec::new());
    }
    map_reference_evidence(
        mapping,
        ReferenceEvidence {
            owner_legacy_id: &edge.source,
            target_legacy_id: Some(&edge.target),
            file_path: &owner.file_path,
            reference_name,
            reference_kind: &edge.kind,
            confidence: edge_confidence(edge)?,
            provenance: edge_provenance(edge),
            total_sites: edge_site_count(edge)?,
            sites,
        },
    )
}

fn map_reference_evidence(
    mapping: &FactMapping<'_>,
    evidence: ReferenceEvidence<'_>,
) -> Result<Vec<ReferenceInput>, V1PostgresImportError> {
    if evidence.total_sites == 0
        || usize_to_u64(evidence.sites.len())? > u64::from(evidence.total_sites)
    {
        return Err(invalid_source("reference_site_count"));
    }
    let file = mapping
        .source
        .files
        .get(evidence.file_path)
        .ok_or_else(|| invalid_source("reference_file_relation"))?;
    let owner = mapped_symbol_id(mapping, evidence.owner_legacy_id, "reference_owner")?;
    let owner_span = *mapping
        .symbol_spans
        .get(evidence.owner_legacy_id)
        .ok_or_else(|| invalid_source("reference_owner_span"))?;
    let target = evidence
        .target_legacy_id
        .map(|target| mapped_symbol_id(mapping, target, "reference_target"))
        .transpose()?;
    let mut anchors: Vec<(ReferenceAnchor, u32)> = Vec::new();
    for site in evidence.sites {
        let anchor = reference_anchor(
            ReferenceAnchorInput::new(file, owner_span, site).with_name(evidence.reference_name),
        );
        if let Some((_, count)) = anchors.iter_mut().find(|(existing, _)| *existing == anchor) {
            *count = count
                .checked_add(1)
                .ok_or(V1PostgresImportError::SourceLimit)?;
        } else {
            anchors.push((anchor, 1));
        }
    }
    let represented = anchors
        .iter()
        .try_fold(0_u32, |total, (_, count)| total.checked_add(*count))
        .ok_or(V1PostgresImportError::SourceLimit)?;
    if let Some((_, primary_count)) = anchors.first_mut() {
        *primary_count = primary_count
            .checked_add(evidence.total_sites.saturating_sub(represented))
            .ok_or(V1PostgresImportError::SourceLimit)?;
    }
    anchors
        .into_iter()
        .map(|(anchor, site_count)| {
            Ok(ReferenceInput {
                file_id: mapped_file_id(mapping, evidence.file_path)?,
                owner_symbol_id: Some(owner.clone()),
                target_symbol_id: target.clone(),
                reference_name: evidence.reference_name.to_owned(),
                reference_kind: evidence.reference_kind.to_owned(),
                start_byte: usize_to_u64(anchor.start)?,
                end_byte: usize_to_u64(anchor.end)?,
                confidence: evidence.confidence,
                resolution_provenance: evidence.provenance.clone(),
                site_count,
                span_precision: anchor.precision,
            })
        })
        .collect()
}

fn reference_anchor(input: ReferenceAnchorInput<'_>) -> ReferenceAnchor {
    let ReferenceAnchorInput {
        file,
        owner_span,
        site,
        reference_name,
    } = input;
    let owner = ReferenceAnchor {
        start: owner_span.0,
        end: owner_span.1,
        precision: crate::ReferenceSpanPrecision::CoarseOwner,
    };
    let Some(column) = site.column else {
        return source_position_candidates(file, site.line, 0)
            .ok()
            .and_then(|positions| unique_position(&positions))
            .map_or(owner, |start| coarse_point(file, start));
    };
    let Ok(positions) = source_position_candidates(file, site.line, column) else {
        return owner;
    };
    let exact = positions
        .iter()
        .copied()
        .filter_map(|start| {
            exact_reference_end(&file.source, start, reference_name).map(|end| (start, end))
        })
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return ReferenceAnchor {
            start: exact[0].0,
            end: exact[0].1,
            precision: crate::ReferenceSpanPrecision::Exact,
        };
    }
    unique_position(&positions).map_or(owner, |start| coarse_point(file, start))
}

fn coarse_point(file: &SourceFile, start: usize) -> ReferenceAnchor {
    ReferenceAnchor {
        start,
        end: next_character_end(&file.source, start).unwrap_or(start),
        precision: crate::ReferenceSpanPrecision::CoarsePoint,
    }
}

fn exact_reference_end(source: &str, start: usize, reference_name: &str) -> Option<usize> {
    if reference_name.is_empty() || !source.get(start..)?.starts_with(reference_name) {
        return None;
    }
    let end = start.checked_add(reference_name.len())?;
    if !source.is_char_boundary(end) {
        return None;
    }
    let last = reference_name.chars().next_back()?;
    let first = reference_name.chars().next()?;
    let previous = source.get(..start)?.chars().next_back();
    let next = source.get(end..)?.chars().next();
    if (is_reference_identifier_character(first)
        && previous.is_some_and(is_reference_identifier_character))
        || (is_reference_identifier_character(last)
            && next.is_some_and(is_reference_identifier_character))
    {
        None
    } else {
        Some(end)
    }
}

fn is_reference_identifier_character(character: char) -> bool {
    unicode_ident::is_xid_continue(character)
        || matches!(character, '_' | '$' | '#' | '\u{200c}' | '\u{200d}')
}

fn edge_confidence(edge: &SourceEdge) -> Result<f32, V1PostgresImportError> {
    Ok(edge
        .metadata
        .numeric_confidence
        .unwrap_or(confidence(edge.confidence.as_deref())?))
}

fn edge_site_count(edge: &SourceEdge) -> Result<u32, V1PostgresImportError> {
    if edge.kind == "def_use" && !edge.metadata.def_use_lines.is_empty() {
        u32::try_from(edge.metadata.def_use_lines.len())
            .map_err(|_| V1PostgresImportError::SourceLimit)
    } else {
        Ok(edge.metadata.site_count)
    }
}

fn edge_provenance(edge: &SourceEdge) -> String {
    edge.metadata.provenance.as_deref().map_or_else(
        || imported_provenance("legacy-unknown"),
        bounded_imported_provenance,
    )
}

fn bounded_imported_provenance(value: &str) -> String {
    let provenance = imported_provenance(value);
    if provenance.len() <= 256 {
        provenance
    } else {
        let digest = blake3::hash(value.as_bytes()).to_hex();
        imported_provenance(&format!("metadata-blake3:{}", &digest[..32]))
    }
}

fn unresolved_reference_provenance(reference: &SourceReference) -> String {
    let mut hasher = blake3::Hasher::new();
    for candidate in &reference.candidates {
        hash_field(&mut hasher, candidate.as_bytes());
    }
    let digest = hasher.finalize().to_hex();
    imported_provenance(&format!(
        "unresolved:language={}:candidates={}:{}",
        reference.language,
        reference.candidates.len(),
        &digest[..16]
    ))
}

fn mapped_file_id(mapping: &FactMapping<'_>, path: &str) -> Result<FileId, V1PostgresImportError> {
    mapping
        .file_ids
        .get(path)
        .cloned()
        .ok_or_else(|| invalid_source("file_identity"))
}

fn mapped_symbol_id(
    mapping: &FactMapping<'_>,
    legacy_id: &str,
    field: &'static str,
) -> Result<SymbolId, V1PostgresImportError> {
    mapping
        .symbol_ids
        .get(legacy_id)
        .cloned()
        .ok_or_else(|| invalid_source(field))
}

fn mapped_node<'a>(
    mapping: &'a FactMapping<'a>,
    legacy_id: &str,
    field: &'static str,
) -> Result<&'a SourceNode, V1PostgresImportError> {
    mapping
        .nodes
        .get(legacy_id)
        .copied()
        .ok_or_else(|| invalid_source(field))
}

fn imported_provenance(value: &str) -> String {
    format!("{LEGACY_PROVENANCE}:{value}")
}

fn source_span(
    file: &SourceFile,
    node: &SourceNode,
) -> Result<(usize, usize), V1PostgresImportError> {
    let (start, end) = if node.body_hash.is_empty() {
        let start = source_positions_by_encoding(file, node.start_line, node.start_column)?;
        let end = source_positions_by_encoding(file, node.end_line, node.end_column)?;
        let mut spans = Vec::with_capacity(2);
        if let (Some(start), Some(end)) = (start.byte, end.byte) {
            spans.push((start, end));
        }
        if let (Some(start), Some(end)) = (start.utf16, end.utf16)
            && !spans.contains(&(start, end))
        {
            spans.push((start, end));
        }
        match spans.as_slice() {
            [span] => *span,
            [] if file.language == "vb6" => legacy_block_span(file, node)?,
            _ => return Err(invalid_source("coordinate_encoding")),
        }
    } else {
        // A validated v1 body hash proves this row came through createNode, whose tree-sitter
        // columns are UTF-8 byte offsets. Apply one encoding consistently to both endpoints.
        (
            byte_source_position(file, node.start_line, node.start_column)?,
            byte_source_position(file, node.end_line, node.end_column)?,
        )
    };
    if start > end || !file.source.is_char_boundary(start) || !file.source.is_char_boundary(end) {
        return Err(invalid_source("node_span"));
    }
    Ok((start, end))
}

fn legacy_block_span(
    file: &SourceFile,
    node: &SourceNode,
) -> Result<(usize, usize), V1PostgresImportError> {
    let starts = source_position_candidates(file, node.start_line, node.start_column)?;
    let start = unique_position(&starts).ok_or_else(|| invalid_source("coordinate_encoding"))?;
    let (_, end) = source_line_bounds(file, node.end_line)?;
    (start <= end)
        .then_some((start, end))
        .ok_or_else(|| invalid_source("node_span"))
}

#[cfg(test)]
fn source_position(
    file: &SourceFile,
    line: u32,
    column: u32,
) -> Result<usize, V1PostgresImportError> {
    let positions = source_position_candidates(file, line, column)?;
    unique_position(&positions).ok_or_else(|| invalid_source("coordinate_encoding"))
}

fn source_position_candidates(
    file: &SourceFile,
    line: u32,
    column: u32,
) -> Result<Vec<usize>, V1PostgresImportError> {
    let encoded = source_positions_by_encoding(file, line, column)?;
    let mut positions = Vec::with_capacity(2);
    if let Some(position) = encoded.byte {
        positions.push(position);
    }
    if let Some(position) = encoded.utf16
        && !positions.contains(&position)
    {
        positions.push(position);
    }
    if positions.is_empty() {
        Err(invalid_source("source_position"))
    } else {
        Ok(positions)
    }
}

struct EncodedSourcePositions {
    byte: Option<usize>,
    utf16: Option<usize>,
}

fn source_positions_by_encoding(
    file: &SourceFile,
    line: u32,
    column: u32,
) -> Result<EncodedSourcePositions, V1PostgresImportError> {
    let (line_start, line_end) = source_line_bounds(file, line)?;
    let column = usize::try_from(column).map_err(|_| invalid_source("source_position"))?;
    let byte = line_start
        .checked_add(column)
        .filter(|position| *position <= line_end && file.source.is_char_boundary(*position));
    let utf16 = utf16_column_to_byte(
        &file.source,
        Utf16Position {
            line_start,
            line_end,
            column,
        },
    );
    Ok(EncodedSourcePositions { byte, utf16 })
}

fn byte_source_position(
    file: &SourceFile,
    line: u32,
    column: u32,
) -> Result<usize, V1PostgresImportError> {
    source_positions_by_encoding(file, line, column)?
        .byte
        .ok_or_else(|| invalid_source("source_position"))
}

fn source_line_bounds(
    file: &SourceFile,
    line: u32,
) -> Result<(usize, usize), V1PostgresImportError> {
    if line == 0 {
        return Err(invalid_source("source_position"));
    }
    let line_index = usize::try_from(line - 1).map_err(|_| invalid_source("source_position"))?;
    let line_start = usize::try_from(
        *file
            .line_starts
            .get(line_index)
            .ok_or_else(|| invalid_source("source_position"))?,
    )
    .map_err(|_| invalid_source("source_position"))?;
    let raw_line_end = match file.line_starts.get(line_index + 1).copied() {
        Some(position) => {
            usize::try_from(position).map_err(|_| invalid_source("source_position"))?
        }
        None => file.source.len(),
    };
    let line = file
        .source
        .get(line_start..raw_line_end)
        .ok_or_else(|| invalid_source("source_position"))?;
    // Legacy manual extractors commonly used `source.split('\n')` and recorded
    // `line.length` as the exclusive end column. For CRLF input that length
    // includes the trailing carriage return, so exclude only the line-feed
    // delimiter here. The resulting boundary can sit after `\r`, but can never
    // advance to the start of the next line.
    let line_end = raw_line_end.saturating_sub(usize::from(line.ends_with('\n')));
    Ok((line_start, line_end))
}

fn unique_position(positions: &[usize]) -> Option<usize> {
    (positions.len() == 1).then_some(positions[0])
}

fn utf16_column_to_byte(source: &str, position: Utf16Position) -> Option<usize> {
    let line = source.get(position.line_start..position.line_end)?;
    let mut utf16_units = 0_usize;
    for (byte_offset, character) in line.char_indices() {
        if utf16_units == position.column {
            return position.line_start.checked_add(byte_offset);
        }
        utf16_units = utf16_units.checked_add(character.len_utf16())?;
        if utf16_units > position.column {
            return None;
        }
    }
    (utf16_units == position.column).then_some(position.line_end)
}

pub(super) fn validate_v1_body_hash(
    node: &SourceNode,
    files: &BTreeMap<String, SourceFile>,
) -> Result<(), V1PostgresImportError> {
    if node.body_hash.is_empty() {
        return Ok(());
    }
    let file = files
        .get(&node.file_path)
        .ok_or_else(|| invalid_source("node_file_relation"))?;
    let body = v1_symbol_body(file, node)?;
    let mut hasher = Sha256::new();
    hasher.update(node.signature.as_deref().unwrap_or_default().as_bytes());
    hasher.update(V1_SIGNATURE_SEPARATOR);
    hasher.update(body.as_bytes());
    let expected = encode_hex(hasher.finalize().as_slice());
    if node.body_hash.to_ascii_lowercase() == expected[..V1_BODY_HASH_HEX_LENGTH] {
        Ok(())
    } else {
        Err(invalid_source("body_hash"))
    }
}

fn v1_symbol_body<'a>(
    file: &'a SourceFile,
    node: &SourceNode,
) -> Result<Cow<'a, str>, V1PostgresImportError> {
    const TRUNCATION_SUFFIX: &str = "\n// ... (truncated)";

    if node.start_line == 0 || node.end_line < node.start_line {
        return Err(invalid_source("body_hash_span"));
    }
    let start =
        usize::try_from(node.start_line - 1).map_err(|_| invalid_source("body_hash_span"))?;
    let end = usize::try_from(node.end_line).map_err(|_| invalid_source("body_hash_span"))?;
    let start_byte = usize::try_from(
        *file
            .line_starts
            .get(start)
            .ok_or_else(|| invalid_source("body_hash_span"))?,
    )
    .map_err(|_| invalid_source("body_hash_span"))?;
    let end_byte = match file
        .line_starts
        .get(end)
        .and_then(|value| value.checked_sub(1))
    {
        Some(position) => {
            usize::try_from(position).map_err(|_| invalid_source("body_hash_span"))?
        }
        None => file.source.len(),
    };
    let body = file
        .source
        .get(start_byte..end_byte)
        .ok_or_else(|| invalid_source("body_hash_span"))?;
    let maximum_prefix_units = V1_MAX_BODY_UTF16_UNITS.saturating_add(1);
    let mut utf16_prefix = Vec::new();
    utf16_prefix
        .try_reserve_exact(maximum_prefix_units)
        .map_err(|_| V1PostgresImportError::SourceLimit)?;
    utf16_prefix.extend(body.encode_utf16().take(maximum_prefix_units));
    if utf16_prefix.len() <= V1_MAX_BODY_UTF16_UNITS {
        return Ok(Cow::Borrowed(body));
    }
    let mut prefix = String::new();
    prefix
        .try_reserve_exact(
            V1_MAX_BODY_UTF16_UNITS
                .saturating_mul(3)
                .saturating_add(TRUNCATION_SUFFIX.len()),
        )
        .map_err(|_| V1PostgresImportError::SourceLimit)?;
    for character in char::decode_utf16(utf16_prefix[..V1_MAX_BODY_UTF16_UNITS].iter().copied()) {
        prefix.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    prefix.push_str(TRUNCATION_SUFFIX);
    Ok(Cow::Owned(prefix))
}

fn next_character_end(source: &str, start: usize) -> Result<usize, V1PostgresImportError> {
    let tail = source
        .get(start..)
        .ok_or_else(|| invalid_source("reference_span"))?;
    let character = tail
        .chars()
        .next()
        .ok_or_else(|| invalid_source("reference_span"))?;
    start
        .checked_add(character.len_utf8())
        .ok_or_else(|| invalid_source("reference_span"))
}

fn parse_edge_kind(raw: &str) -> Result<EdgeKind, V1PostgresImportError> {
    EdgeKind::parse(raw).ok_or_else(|| invalid_source("edge_kind"))
}

fn confidence(raw: Option<&str>) -> Result<f32, V1PostgresImportError> {
    match raw {
        Some("EXTRACTED") | None => Ok(EXTRACTED_CONFIDENCE),
        Some("INFERRED") => Ok(INFERRED_CONFIDENCE),
        Some("AMBIGUOUS") => Ok(AMBIGUOUS_CONFIDENCE),
        Some(_) => Err(invalid_source("edge_confidence")),
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::ReferenceSpanPrecision;
    use cartograph_domain::ContentDigest;
    use std::{collections::BTreeMap, time::Duration};
    use tokio::time::Instant;

    const EMBEDDED_REFERENCE_COLUMN: u32 = 3;
    const PACKAGE_REFERENCE_COLUMN: u32 = 7;
    const MISSING_REFERENCE_LINE: u32 = 99;
    const AMBIGUOUS_COLUMN: u32 = 2;
    const BOM_START_COLUMN: u32 = 3;
    const BOM_END_COLUMN: u32 = 12;
    const BOM_START_BYTE: usize = 3;
    const BOM_END_BYTE: usize = 12;
    const MIXED_START_COLUMN: u32 = 1;
    const MIXED_END_COLUMN: u32 = 5;
    const OWNER_SOURCE: &str = "fn owner() {}\n";
    const OWNER_END_COLUMN: u32 = 13;
    const DEF_USE_TEST_DIGEST_BYTE: u8 = 1;
    const MAX_ID_TEST_DIGEST_BYTE: u8 = 2;
    const TEST_DIGEST_BYTES: usize = 32;
    const EXPECTED_DEF_USE_SITES: u32 = 2;
    const TEST_DEADLINE: Duration = Duration::from_secs(1);
    const MAXIMUM_TEST_ROWS: u64 = 10;
    const MAXIMUM_TEST_SOURCE_BYTES: u64 = 1_024;
    const MINIMUM_VALIDATION_OUTPUT_BYTES: u64 = 1;

    #[test]
    fn optional_legacy_json_falls_back_without_inventing_multiplicity() {
        let metadata = parse_legacy_edge_metadata(Some(
            r#"{"resolvedBy":"qualified-name","siteCount":"wrong","extraLines":[2,3]}"#,
        ))
        .unwrap_or_else(|error| panic!("optional metadata fallback failed: {error}"));
        assert_eq!(metadata.site_count, 1);
        assert_eq!(metadata.extra_lines, [] as [u32; 0]);
        assert_eq!(
            metadata.provenance.as_deref(),
            Some("resolvedBy:qualified-name")
        );

        assert_eq!(
            parse_legacy_string_array(Some(r#"["valid",7]"#), "candidates")
                .unwrap_or_else(|error| panic!("mixed candidate fallback failed: {error}")),
            [] as [String; 0]
        );
        assert_eq!(
            parse_legacy_u32_array(Some(r#"[2,"bad"]"#), "extra_lines")
                .unwrap_or_else(|error| panic!("mixed line fallback failed: {error}")),
            [] as [u32; 0]
        );
    }

    #[test]
    fn only_schema_valid_nonempty_v1_error_arrays_mark_partial_files() {
        assert!(has_valid_extraction_errors(Some(
            r#"[{"message":"broken","severity":"warning","line":2}]"#
        )));
        for unavailable in [
            None,
            Some("[]"),
            Some("not-json"),
            Some(r#"[{"message":"broken","severity":"notice"}]"#),
            Some(r#"[{"message":7,"severity":"error"}]"#),
        ] {
            assert!(!has_valid_extraction_errors(unavailable));
        }
    }

    #[test]
    fn legacy_reference_precision_is_exact_only_for_a_proven_full_token() {
        let exact_file = source_file("call()\n");
        let exact = reference_anchor(
            ReferenceAnchorInput::new(
                &exact_file,
                (0, exact_file.source.len()),
                ReferenceSite {
                    line: 1,
                    column: Some(0),
                },
            )
            .with_name("call"),
        );
        assert_eq!(exact.precision, ReferenceSpanPrecision::Exact);
        assert_eq!((exact.start, exact.end), (0, "call".len()));

        let embedded_file = source_file("foobar()\n");
        let embedded = reference_anchor(
            ReferenceAnchorInput::new(
                &embedded_file,
                (0, embedded_file.source.len()),
                ReferenceSite {
                    line: 1,
                    column: Some(EMBEDDED_REFERENCE_COLUMN),
                },
            )
            .with_name("bar"),
        );
        assert_eq!(embedded.precision, ReferenceSpanPrecision::CoarsePoint);

        for (source, column) in [("foo$bar()\n", 0), ("$foo()\n", 1), ("#foo()\n", 1)] {
            let file = source_file(source);
            let anchor = reference_anchor(
                ReferenceAnchorInput::new(
                    &file,
                    (0, file.source.len()),
                    ReferenceSite {
                        line: 1,
                        column: Some(column),
                    },
                )
                .with_name("foo"),
            );
            assert_eq!(anchor.precision, ReferenceSpanPrecision::CoarsePoint);
        }
        let combining = source_file("fo\u{0301}o()\n");
        let anchor = reference_anchor(
            ReferenceAnchorInput::new(
                &combining,
                (0, combining.source.len()),
                ReferenceSite {
                    line: 1,
                    column: Some(0),
                },
            )
            .with_name("fo"),
        );
        assert_eq!(anchor.precision, ReferenceSpanPrecision::CoarsePoint);

        for (source, column, name) in [
            (
                "import \"example.com/widgets\"\n",
                PACKAGE_REFERENCE_COLUMN,
                "widgets",
            ),
            ("@route()\n", 0, "route"),
            ("<Service />\n", 0, "Service"),
        ] {
            let file = source_file(source);
            let anchor = reference_anchor(
                ReferenceAnchorInput::new(
                    &file,
                    (0, file.source.len()),
                    ReferenceSite {
                        line: 1,
                        column: Some(column),
                    },
                )
                .with_name(name),
            );
            assert_eq!(anchor.precision, ReferenceSpanPrecision::CoarsePoint);
        }

        let zero_owner = reference_anchor(
            ReferenceAnchorInput::new(
                &exact_file,
                (0, 0),
                ReferenceSite {
                    line: MISSING_REFERENCE_LINE,
                    column: Some(0),
                },
            )
            .with_name("missing"),
        );
        assert_eq!(zero_owner.precision, ReferenceSpanPrecision::CoarseOwner);
        assert_eq!((zero_owner.start, zero_owner.end), (0, 0));
    }

    #[test]
    fn ambiguous_legacy_columns_fail_closed_and_zero_width_symbols_are_retained() {
        let unicode = source_file("\u{e9}foo\n");
        assert_matches!(
            source_position(&unicode, 1, AMBIGUOUS_COLUMN),
            Err(super::V1PostgresImportError::InvalidSourceData {
                field: "coordinate_encoding"
            })
        );

        let empty = source_file("\n");
        let node = source_node("node", 0, 0);
        assert_eq!(source_span(&empty, &node), Ok((0, 0)));

        let bom = source_file("\u{feff}fn x() {}\n");
        let mut tree_sitter_node = source_node("bom", BOM_START_COLUMN, BOM_END_COLUMN);
        tree_sitter_node.body_hash = "validated-v1-tree-sitter-proof".to_owned();
        assert_eq!(
            source_span(&bom, &tree_sitter_node),
            Ok((BOM_START_BYTE, BOM_END_BYTE))
        );

        let mixed_only = source_file("\u{e9}abc\n");
        let mixed_node = source_node("mixed", MIXED_START_COLUMN, MIXED_END_COLUMN);
        assert_matches!(
            source_span(&mixed_only, &mixed_node),
            Err(super::V1PostgresImportError::InvalidSourceData {
                field: "coordinate_encoding"
            })
        );
    }

    #[test]
    fn crlf_properties_manual_coordinates_retain_the_carriage_return_boundary() {
        let source = "Enabled=true\r\nOther=false\r\n";
        let mut properties = source_file(source);
        properties.language = "properties".to_owned();
        let legacy_line = source
            .split('\n')
            .next()
            .unwrap_or_else(|| panic!("CRLF properties fixture lost its first line"));
        let end_column = u32::try_from(legacy_line.len())
            .unwrap_or_else(|_| panic!("CRLF properties fixture is too large"));
        let node = source_node("Enabled", 0, end_column);

        assert_eq!(node.body_hash, "");
        assert_eq!(source_span(&properties, &node), Ok((0, legacy_line.len())));
        assert_eq!(source.as_bytes()[legacy_line.len() - 1], b'\r');
        assert_eq!(
            properties.line_starts[1],
            u32::try_from(legacy_line.len() + 1)
                .unwrap_or_else(|_| panic!("CRLF properties fixture is too large"))
        );
    }

    #[test]
    fn legacy_block_extractors_fall_back_when_the_name_end_column_is_not_on_the_end_line() {
        let source = "Public Type Person\nEnd Type\n";
        let mut file = source_file(source);
        file.language = "vb6".to_owned();
        let mut node = source_node("Person", 12, 18);
        node.end_line = 2;
        assert_eq!(source_span(&file, &node), Ok((12, source.len() - 1)));
    }

    #[test]
    fn scip_placeholder_is_bound_to_the_normalized_path() {
        let path = "src/generated.rs";
        let digest = sha256_hex(path.as_bytes());
        let placeholder = format!("scip:{}", &digest[..24]);
        assert!(is_scip_placeholder(&placeholder, 0, path));
        assert!(!is_scip_placeholder(&placeholder, 0, "src/other.rs"));
        assert!(!is_scip_placeholder(&placeholder, 1, path));
    }

    #[test]
    fn def_use_without_a_valid_name_retains_the_edge_but_omits_fabricated_refs() {
        let file = source_file(OWNER_SOURCE);
        let node = source_node("owner", 0, OWNER_END_COLUMN);
        let snapshot = SourceSnapshot {
            files: BTreeMap::from([("src/lib.rs".to_owned(), file)]),
            nodes: vec![node],
            edges: Vec::new(),
            references: Vec::new(),
            source_revision: ContentDigest::from_bytes(
                [DEF_USE_TEST_DIGEST_BYTE; TEST_DIGEST_BYTES],
            ),
            fingerprint: ContentDigest::from_bytes([DEF_USE_TEST_DIGEST_BYTE; TEST_DIGEST_BYTES]),
            source_bytes: u64::try_from(OWNER_SOURCE.len())
                .unwrap_or_else(|_| panic!("owner source length overflowed")),
        };
        let mapping = FactMapping::new(&snapshot, Instant::now() + TEST_DEADLINE)
            .unwrap_or_else(|error| panic!("mapping fixture was rejected: {error}"));
        let edge = SourceEdge {
            source: "owner".to_owned(),
            target: "owner".to_owned(),
            kind: "def_use".to_owned(),
            confidence: None,
            line: None,
            column: None,
            metadata: LegacyEdgeMetadata {
                def_use_lines: vec![1, 1],
                ..LegacyEdgeMetadata::default()
            },
        };
        let mapped = map_edge_fact(&mapping, &edge)
            .unwrap_or_else(|error| panic!("def-use edge mapping failed: {error}"));
        assert_eq!(mapped.site_count, EXPECTED_DEF_USE_SITES);
        assert_eq!(
            map_edge_reference_facts(&mapping, &edge)
                .unwrap_or_else(|error| panic!("def-use reference mapping failed: {error}")),
            []
        );
    }

    #[test]
    fn max_legacy_id_clones_are_admitted_before_fact_mapping() {
        let legacy_id = "x".repeat(MAXIMUM_LEGACY_ID_BYTES);
        let file = source_file("x\n");
        let node = source_node(&legacy_id, 0, 1);
        let snapshot = SourceSnapshot {
            files: BTreeMap::from([("src/lib.rs".to_owned(), file)]),
            nodes: vec![node],
            edges: Vec::new(),
            references: Vec::new(),
            source_revision: ContentDigest::from_bytes(
                [MAX_ID_TEST_DIGEST_BYTE; TEST_DIGEST_BYTES],
            ),
            fingerprint: ContentDigest::from_bytes([MAX_ID_TEST_DIGEST_BYTE; TEST_DIGEST_BYTES]),
            source_bytes: u64::try_from("x\n".len())
                .unwrap_or_else(|_| panic!("maximum-id source length overflowed")),
        };
        let retained = source_retained_bytes(&snapshot)
            .unwrap_or_else(|error| panic!("source retained model failed: {error}"));
        let (_, derived) = derived_mapping_bounds(&snapshot)
            .unwrap_or_else(|error| panic!("derived mapping model failed: {error}"));
        let clone_floor = u64::try_from(
            legacy_id
                .len()
                .saturating_mul(FACT_MAPPING_LEGACY_ID_COPIES)
                .saturating_add(
                    FACT_MAPPING_BTREE_ENTRY_ALLOWANCE
                        .saturating_mul(FACT_MAPPING_LEGACY_ID_COPIES),
                ),
        )
        .unwrap_or_else(|_| panic!("legacy id clone floor overflowed"));
        assert!(derived >= clone_floor);
        let admitted = retained
            .checked_add(derived)
            .unwrap_or_else(|| panic!("admitted mapping bytes overflowed"));
        let limits = |working| {
            V1PostgresImportLimits::new(
                MAXIMUM_TEST_ROWS,
                MAXIMUM_TEST_SOURCE_BYTES,
                GenerationValidationLimits::new(MINIMUM_VALIDATION_OUTPUT_BYTES, working)
                    .unwrap_or_else(|error| panic!("validation limits failed: {error}")),
            )
            .unwrap_or_else(|error| panic!("import limits failed: {error}"))
        };
        assert_matches!(
            admit_mapping(&snapshot, limits(admitted - 1)).err(),
            Some(V1PostgresImportError::SourceLimit)
        );
        assert!(admit_mapping(&snapshot, limits(admitted)).is_ok());
    }

    #[test]
    fn imported_and_native_signatures_share_the_literal_safe_persistence_policy() {
        let cases = [
            (
                "function",
                Some("(value: u32) -> Result"),
                "(value: u32) -> Result",
            ),
            ("constant", Some("= OTHER_LIMIT"), "= OTHER_LIMIT"),
            ("field", Some("[string]"), "[string]"),
            ("constant", Some("= sk_live_import_secret"), ""),
            ("variable", Some("= 42"), ""),
            ("import", Some("#include \"private.h\""), ""),
        ];
        for (index, (kind, signature, expected)) in cases.into_iter().enumerate() {
            let source = "x\n";
            let mut node = source_node(&format!("node-{index}"), 0, 1);
            node.kind = kind.to_owned();
            node.signature = signature.map(str::to_owned);
            let snapshot = SourceSnapshot {
                files: BTreeMap::from([("src/lib.rs".to_owned(), source_file(source))]),
                nodes: vec![node],
                edges: Vec::new(),
                references: Vec::new(),
                source_revision: ContentDigest::from_bytes([u8::try_from(index).unwrap_or(0); 32]),
                fingerprint: ContentDigest::from_bytes([u8::try_from(index).unwrap_or(0); 32]),
                source_bytes: u64::try_from(source.len())
                    .unwrap_or_else(|_| panic!("signature fixture length overflowed")),
            };
            let mapping = FactMapping::new(&snapshot, Instant::now() + TEST_DEADLINE)
                .unwrap_or_else(|error| panic!("signature mapping fixture failed: {error}"));
            let mapped = map_symbol_fact(&mapping, &snapshot.nodes[0])
                .unwrap_or_else(|error| panic!("signature mapping failed: {error}"));
            assert_eq!(mapped.symbol.signature, expected, "{kind}");
            assert_eq!(
                mapped.document.code,
                if expected.is_empty() {
                    snapshot.nodes[0].qualified_name.as_str()
                } else {
                    expected
                },
                "{kind}"
            );
            assert!(!format!("{:?}", mapped.symbol).contains("sk_live_import_secret"));
        }
    }

    fn source_file(source: &str) -> SourceFile {
        SourceFile {
            language: "rust".to_owned(),
            source: source.to_owned(),
            line_starts: line_starts(source)
                .unwrap_or_else(|error| panic!("source line index failed: {error}")),
            content_hash: ContentDigest::from_bytes(*blake3::hash(source.as_bytes()).as_bytes()),
            has_extraction_errors: false,
            is_test: false,
        }
    }

    fn source_node(id: &str, start_column: u32, end_column: u32) -> SourceNode {
        SourceNode {
            legacy_id: id.to_owned(),
            name: id.to_owned(),
            kind: "function".to_owned(),
            qualified_name: id.to_owned(),
            file_path: "src/lib.rs".to_owned(),
            language: "rust".to_owned(),
            start_line: 1,
            end_line: 1,
            start_column,
            end_column,
            docstring: None,
            signature: None,
            body_hash: String::new(),
            visibility: None,
            export: cartograph_domain::SymbolExportFlags::default(),
            execution: cartograph_domain::SymbolExecutionFlags::default(),
        }
    }
}
