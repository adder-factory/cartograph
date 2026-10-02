use std::collections::BTreeMap;

use cartograph_db::{InterchangeEdge, InterchangeFile, InterchangeSnapshot, InterchangeSymbol};
use cartograph_domain::{FileId, SymbolId};
use serde::Serialize;

use crate::{
    codec::encode_scip_index,
    model::{
        CartographScipEdge, MAXIMUM_SCIP_BYTES, MAXIMUM_STRING_BYTES, SYMBOL_ROLE_DEFINITION,
        ScipDocument, ScipError, ScipIndex, ScipOccurrence, ScipRelationship,
        ScipRelationshipRoles, ScipSymbolInformation, ScipSymbolKind,
    },
    symbol::{
        Descriptor, DescriptorSuffix, ScipPackage, append_meta_descriptor, build_symbol_string,
    },
};

const SCIP_SCHEME: &str = "cartograph";
const SCIP_MANAGER: &str = "cartograph";
const CARTOGRAPH_SCIP_KINDS: [(&[&str], ScipSymbolKind); 19] = [
    (&["class", "component"], ScipSymbolKind::Class),
    (&["constant"], ScipSymbolKind::Constant),
    (&["enum"], ScipSymbolKind::Enum),
    (&["enum_member"], ScipSymbolKind::EnumMember),
    (&["field"], ScipSymbolKind::Field),
    (&["file"], ScipSymbolKind::File),
    (&["function"], ScipSymbolKind::Function),
    (&["interface"], ScipSymbolKind::Interface),
    (&["method", "route"], ScipSymbolKind::Method),
    (&["module"], ScipSymbolKind::Module),
    (&["namespace"], ScipSymbolKind::Namespace),
    (&["resource"], ScipSymbolKind::Object),
    (&["parameter"], ScipSymbolKind::Parameter),
    (&["property"], ScipSymbolKind::Property),
    (&["protocol"], ScipSymbolKind::Protocol),
    (&["struct", "table"], ScipSymbolKind::Struct),
    (&["trait"], ScipSymbolKind::Trait),
    (&["type_alias"], ScipSymbolKind::TypeAlias),
    (&["variable"], ScipSymbolKind::Variable),
];

/// Validated identity and tool metadata for a SCIP export.
pub struct ScipExportOptions<'a> {
    project_name: &'a str,
    project_version: &'a str,
    tool_version: &'a str,
    project_root_uri: &'a str,
}

/// Project and tool metadata attached to one deterministic SCIP export.
#[derive(Clone, Copy)]
pub struct ScipExportOptionsInput<'a> {
    /// Project name for this record.
    pub project_name: &'a str,
    /// Project version for this record.
    pub project_version: &'a str,
    /// Tool version for this record.
    pub tool_version: &'a str,
    /// Project root uri for this record.
    pub project_root_uri: &'a str,
}

impl<'a> ScipExportOptions<'a> {
    /// Create metadata without retaining source paths or database details.
    /// # Errors
    ///
    /// Returns an error if any required metadata value is empty, oversized, or
    /// contains a NUL byte.
    pub fn new(input: ScipExportOptionsInput<'a>) -> Result<Self, ScipError> {
        let ScipExportOptionsInput {
            project_name,
            project_version,
            tool_version,
            project_root_uri,
        } = input;
        for value in [
            project_name,
            project_version,
            tool_version,
            project_root_uri,
        ] {
            if value.is_empty() || value.len() > MAXIMUM_STRING_BYTES || value.contains('\0') {
                return Err(ScipError::InvalidData);
            }
        }
        Ok(Self {
            project_name,
            project_version,
            tool_version,
            project_root_uri,
        })
    }
}

/// Fixed-size accounting for a deterministic SCIP export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScipExportStats {
    documents: u64,
    symbols: u64,
    occurrences: u64,
    exact_typed_edges: u64,
    disambiguated_symbols: u64,
    bytes: u64,
}

impl ScipExportStats {
    /// Number of emitted source documents.
    #[must_use]
    pub const fn documents(self) -> u64 {
        self.documents
    }

    /// Number of emitted symbol records.
    #[must_use]
    pub const fn symbols(self) -> u64 {
        self.symbols
    }

    /// Number of definition and reference occurrences.
    #[must_use]
    pub const fn occurrences(self) -> u64 {
        self.occurrences
    }

    /// Number of exact Cartograph typed-edge extension records.
    #[must_use]
    pub const fn exact_typed_edges(self) -> u64 {
        self.exact_typed_edges
    }

    /// Number of symbols receiving stable collision descriptors.
    #[must_use]
    pub const fn disambiguated_symbols(self) -> u64 {
        self.disambiguated_symbols
    }

    /// Encoded protobuf size.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.bytes
    }
}

/// One encoded SCIP index and its deterministic accounting.
pub struct ScipExport {
    index: ScipIndex,
    bytes: Vec<u8>,
    stats: ScipExportStats,
}

impl ScipExport {
    /// Borrow the decoded shape used to produce the protobuf.
    #[must_use]
    pub const fn index(&self) -> &ScipIndex {
        &self.index
    }

    /// Borrow encoded SCIP protobuf bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Export accounting.
    #[must_use]
    pub const fn stats(&self) -> ScipExportStats {
        self.stats
    }

    /// Consume the export and return encoded SCIP protobuf bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Export one immutable PostgreSQL graph snapshot as standard SCIP plus exact typed edges.
/// # Errors
///
/// Returns an error if snapshot identities/relations are inconsistent, source
/// bytes are unavailable or mismatched, cardinality overflows, or encoding is oversized.
pub fn export_snapshot<ReadSource>(
    snapshot: &InterchangeSnapshot,
    options: &ScipExportOptions<'_>,
    mut read_source: ReadSource,
) -> Result<ScipExport, ScipError>
where
    ReadSource: FnMut(&str) -> Option<Vec<u8>>,
{
    let package = ScipPackage {
        manager: SCIP_MANAGER,
        name: options.project_name,
        version: options.project_version,
    };
    let mut assembly = ScipExportAssembly::new(snapshot, &package)?;
    assembly.add_symbols(snapshot, &mut read_source)?;
    assembly.add_references(snapshot, &mut read_source)?;
    let (documents, disambiguated_symbols) = assembly.finish();
    let index = ScipIndex {
        tool_name: "cartograph".to_owned(),
        tool_version: options.tool_version.to_owned(),
        project_root: options.project_root_uri.to_owned(),
        documents,
    };
    let bytes = encode_scip_index(&index)?;
    if bytes.len() > MAXIMUM_SCIP_BYTES {
        return Err(ScipError::LimitExceeded);
    }
    let (symbol_count, occurrence_count) = document_item_counts(&index.documents)?;
    let stats = ScipExportStats {
        documents: usize_to_u64(index.documents.len()),
        symbols: symbol_count,
        occurrences: occurrence_count,
        exact_typed_edges: usize_to_u64(snapshot.edges.len()),
        disambiguated_symbols,
        bytes: usize_to_u64(bytes.len()),
    };
    Ok(ScipExport {
        index,
        bytes,
        stats,
    })
}

struct ScipExportAssembly<'snapshot> {
    file_by_id: BTreeMap<FileId, &'snapshot InterchangeFile>,
    symbol_by_id: BTreeMap<SymbolId, String>,
    symbol_record_by_id: BTreeMap<SymbolId, &'snapshot InterchangeSymbol>,
    enclosing_by_id: BTreeMap<SymbolId, String>,
    relationships_by_id: BTreeMap<SymbolId, Vec<ScipRelationship>>,
    exact_edges_by_id: BTreeMap<SymbolId, Vec<CartographScipEdge>>,
    documents: BTreeMap<FileId, ScipDocument>,
    source_cache: BTreeMap<FileId, Vec<u8>>,
    disambiguated_symbols: u64,
}

impl<'snapshot> ScipExportAssembly<'snapshot> {
    fn new(
        snapshot: &'snapshot InterchangeSnapshot,
        package: &ScipPackage<'_>,
    ) -> Result<Self, ScipError> {
        let file_by_id = snapshot
            .files
            .iter()
            .map(|file| (file.file_id.clone(), file))
            .collect::<BTreeMap<_, _>>();
        let (symbol_by_id, disambiguated_symbols) =
            assign_symbols(&snapshot.symbols, &file_by_id, package)?;
        let symbol_record_by_id = snapshot
            .symbols
            .iter()
            .map(|symbol| (symbol.symbol_id.clone(), symbol))
            .collect::<BTreeMap<_, _>>();
        let (enclosing_by_id, relationships_by_id, exact_edges_by_id) =
            graph_metadata(&snapshot.edges, &symbol_by_id);
        let documents = snapshot
            .files
            .iter()
            .map(|file| {
                (
                    file.file_id.clone(),
                    ScipDocument {
                        relative_path: file.path.clone(),
                        language: file.language.clone(),
                        occurrences: Vec::new(),
                        symbols: Vec::new(),
                    },
                )
            })
            .collect();
        Ok(Self {
            file_by_id,
            symbol_by_id,
            symbol_record_by_id,
            enclosing_by_id,
            relationships_by_id,
            exact_edges_by_id,
            documents,
            source_cache: BTreeMap::new(),
            disambiguated_symbols,
        })
    }

    fn add_symbols<ReadSource>(
        &mut self,
        snapshot: &InterchangeSnapshot,
        read_source: &mut ReadSource,
    ) -> Result<(), ScipError>
    where
        ReadSource: FnMut(&str) -> Option<Vec<u8>>,
    {
        for symbol in &snapshot.symbols {
            let Some(scip_symbol) = self.symbol_by_id.get(&symbol.symbol_id) else {
                return Err(ScipError::InvalidData);
            };
            let Some(file) = self.file_by_id.get(&symbol.file_id) else {
                return Err(ScipError::InvalidData);
            };
            let source = source_for_file(&mut self.source_cache, file, read_source)?;
            let Some(document) = self.documents.get_mut(&symbol.file_id) else {
                return Err(ScipError::InvalidData);
            };
            document.symbols.push(ScipSymbolInformation {
                symbol: scip_symbol.clone(),
                display_name: symbol_display_name(symbol, &file.path),
                kind: scip_kind(&symbol.symbol_kind),
                documentation: bounded_documentation(&symbol.natural_text),
                relationships: self
                    .relationships_by_id
                    .get(&symbol.symbol_id)
                    .cloned()
                    .unwrap_or_default(),
                enclosing_symbol: self
                    .enclosing_by_id
                    .get(&symbol.symbol_id)
                    .cloned()
                    .unwrap_or_default(),
                cartograph_edges: self
                    .exact_edges_by_id
                    .get(&symbol.symbol_id)
                    .cloned()
                    .unwrap_or_default(),
            });
            if symbol.symbol_kind != "file" {
                document.occurrences.push(ScipOccurrence {
                    range: symbol_range(symbol, Some(source)),
                    symbol: scip_symbol.clone(),
                    symbol_roles: SYMBOL_ROLE_DEFINITION,
                    enclosing_range: symbol_range(symbol, Some(source)),
                });
            }
        }
        Ok(())
    }

    fn add_references<ReadSource>(
        &mut self,
        snapshot: &InterchangeSnapshot,
        read_source: &mut ReadSource,
    ) -> Result<(), ScipError>
    where
        ReadSource: FnMut(&str) -> Option<Vec<u8>>,
    {
        for reference in &snapshot.references {
            let Some(target_id) = reference.target_symbol_id.as_ref() else {
                continue;
            };
            let Some(target_symbol) = self.symbol_by_id.get(target_id) else {
                continue;
            };
            let Some(file) = self.file_by_id.get(&reference.file_id) else {
                return Err(ScipError::InvalidData);
            };
            let source = source_for_file(&mut self.source_cache, file, read_source)?;
            let Some(range) = byte_range_to_scip(source, reference.start_byte, reference.end_byte)
            else {
                continue;
            };
            let enclosing_range = reference
                .owner_symbol_id
                .as_ref()
                .and_then(|owner| self.symbol_record_by_id.get(owner))
                .map(|owner| symbol_range(owner, Some(source)))
                .unwrap_or_default();
            let Some(document) = self.documents.get_mut(&reference.file_id) else {
                return Err(ScipError::InvalidData);
            };
            document.occurrences.push(ScipOccurrence {
                range,
                symbol: target_symbol.clone(),
                symbol_roles: 0,
                enclosing_range,
            });
        }
        Ok(())
    }

    fn finish(self) -> (Vec<ScipDocument>, u64) {
        let mut documents = self
            .documents
            .into_values()
            .filter(|document| !document.symbols.is_empty() || !document.occurrences.is_empty())
            .collect::<Vec<_>>();
        for document in &mut documents {
            document
                .symbols
                .sort_by(|left, right| left.symbol.cmp(&right.symbol));
            document.occurrences.sort_by(|left, right| {
                left.range
                    .cmp(&right.range)
                    .then_with(|| left.symbol.cmp(&right.symbol))
                    .then_with(|| left.symbol_roles.cmp(&right.symbol_roles))
            });
        }
        documents.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        (documents, self.disambiguated_symbols)
    }
}

type GraphMetadata = (
    BTreeMap<SymbolId, String>,
    BTreeMap<SymbolId, Vec<ScipRelationship>>,
    BTreeMap<SymbolId, Vec<CartographScipEdge>>,
);

fn graph_metadata(
    edges: &[InterchangeEdge],
    symbol_by_id: &BTreeMap<SymbolId, String>,
) -> GraphMetadata {
    let mut enclosing = BTreeMap::new();
    let mut relationships = BTreeMap::<SymbolId, Vec<ScipRelationship>>::new();
    let mut exact_edges = BTreeMap::<SymbolId, Vec<CartographScipEdge>>::new();
    for edge in edges {
        let Some(source) = symbol_by_id.get(&edge.source_symbol_id) else {
            continue;
        };
        let Some(target) = symbol_by_id.get(&edge.target_symbol_id) else {
            continue;
        };
        if edge.edge_kind == "contains" {
            enclosing
                .entry(edge.target_symbol_id.clone())
                .or_insert_with(|| source.clone());
        }
        if let Some(relationship) = standard_relationship(&edge.edge_kind, target) {
            relationships
                .entry(edge.source_symbol_id.clone())
                .or_default()
                .push(relationship);
        }
        exact_edges
            .entry(edge.source_symbol_id.clone())
            .or_default()
            .push(CartographScipEdge {
                target_symbol: target.clone(),
                edge_kind: edge.edge_kind.clone(),
                site_count: edge.site_count,
                provenance: edge.provenance.clone(),
                confidence_bits: edge.confidence.to_bits(),
            });
    }
    for values in relationships.values_mut() {
        values.sort_by(|left, right| left.symbol.cmp(&right.symbol));
        values.dedup();
    }
    for values in exact_edges.values_mut() {
        values.sort_by(|left, right| {
            left.edge_kind
                .cmp(&right.edge_kind)
                .then_with(|| left.target_symbol.cmp(&right.target_symbol))
                .then_with(|| left.provenance.cmp(&right.provenance))
        });
    }
    (enclosing, relationships, exact_edges)
}

fn standard_relationship(edge_kind: &str, target: &str) -> Option<ScipRelationship> {
    let roles = match edge_kind {
        "extends" | "implements" => ScipRelationshipRoles::default().with_implementation(true),
        "overrides" => ScipRelationshipRoles::default()
            .with_reference(true)
            .with_implementation(true),
        "type_of" => ScipRelationshipRoles::default().with_type_definition(true),
        _ => return None,
    };
    Some(ScipRelationship {
        symbol: target.to_owned(),
        roles,
    })
}

fn assign_symbols(
    symbols: &[InterchangeSymbol],
    file_by_id: &BTreeMap<FileId, &InterchangeFile>,
    package: &ScipPackage<'_>,
) -> Result<(BTreeMap<SymbolId, String>, u64), ScipError> {
    let mut groups = BTreeMap::<String, Vec<&InterchangeSymbol>>::new();
    for symbol in symbols {
        let file = file_by_id
            .get(&symbol.file_id)
            .ok_or(ScipError::InvalidData)?;
        let natural = natural_symbol(symbol, &file.path, package)?;
        groups.entry(natural).or_default().push(symbol);
    }
    let mut assigned = BTreeMap::new();
    let mut disambiguated = 0_u64;
    for (natural, mut group) in groups {
        group.sort_by(|left, right| left.symbol_id.cmp(&right.symbol_id));
        let collides = group.len() > 1;
        for symbol in group {
            let mut value = natural.clone();
            if collides {
                append_meta_descriptor(&mut value, &compact_id(&symbol.symbol_id))?;
                disambiguated = disambiguated
                    .checked_add(1)
                    .ok_or(ScipError::LimitExceeded)?;
            }
            assigned.insert(symbol.symbol_id.clone(), value);
        }
    }
    Ok((assigned, disambiguated))
}

fn natural_symbol(
    symbol: &InterchangeSymbol,
    file_path: &str,
    package: &ScipPackage<'_>,
) -> Result<String, ScipError> {
    let mut descriptors = vec![Descriptor::new(
        file_path.to_owned(),
        DescriptorSuffix::Namespace,
    )];
    if symbol.symbol_kind != "file" {
        let file_prefix = format!("{file_path}::");
        let qualified_name = symbol
            .qualified_name
            .strip_prefix(&file_prefix)
            .unwrap_or(&symbol.qualified_name);
        let mut parts = symbol
            .qualified_name
            .strip_prefix(&file_prefix)
            .unwrap_or(qualified_name)
            .split("::")
            .filter(|part| !part.is_empty())
            .peekable();
        while let Some(part) = parts.next() {
            let suffix = if parts.peek().is_none() {
                descriptor_suffix(&symbol.symbol_kind)
            } else {
                DescriptorSuffix::Type
            };
            descriptors.push(Descriptor::new(part.to_owned(), suffix));
        }
        if descriptors.len() == 1 {
            descriptors.push(Descriptor::new(
                compact_id(&symbol.symbol_id),
                descriptor_suffix(&symbol.symbol_kind),
            ));
        }
    }
    build_symbol_string(SCIP_SCHEME, package, &descriptors)
}

fn compact_id(identifier: &SymbolId) -> String {
    identifier
        .as_str()
        .chars()
        .filter(|character| *character != '-')
        .collect()
}

fn descriptor_suffix(kind: &str) -> DescriptorSuffix {
    match kind {
        "file" | "module" | "namespace" => DescriptorSuffix::Namespace,
        "class" | "struct" | "union" | "interface" | "trait" | "protocol" | "enum"
        | "type_alias" | "component" | "table" | "resource" => DescriptorSuffix::Type,
        "function" | "method" | "route" => DescriptorSuffix::Method,
        "parameter" => DescriptorSuffix::Parameter,
        _ => DescriptorSuffix::Term,
    }
}

fn scip_kind(kind: &str) -> u32 {
    CARTOGRAPH_SCIP_KINDS
        .iter()
        .find(|(cartograph_kinds, _)| cartograph_kinds.contains(&kind))
        .map_or(ScipSymbolKind::Unspecified, |(_, scip_kind)| *scip_kind) as u32
}

fn source_for_file<'a, ReadSource>(
    cache: &'a mut BTreeMap<FileId, Vec<u8>>,
    file: &InterchangeFile,
    read_source: &mut ReadSource,
) -> Result<&'a [u8], ScipError>
where
    ReadSource: FnMut(&str) -> Option<Vec<u8>>,
{
    if !cache.contains_key(&file.file_id) {
        let bytes = read_source(&file.path).ok_or(ScipError::SourceChanged)?;
        if usize_to_u64(bytes.len()) != file.byte_size
            || blake3::hash(&bytes).to_hex().as_str() != file.content_hash
        {
            return Err(ScipError::SourceChanged);
        }
        cache.insert(file.file_id.clone(), bytes);
    }
    cache
        .get(&file.file_id)
        .map(Vec::as_slice)
        .ok_or(ScipError::SourceChanged)
}

fn symbol_range(symbol: &InterchangeSymbol, source: Option<&[u8]>) -> Vec<u32> {
    source
        .and_then(|bytes| byte_range_to_scip(bytes, symbol.start_byte, symbol.end_byte))
        .unwrap_or_else(|| {
            let start = symbol.start_line.saturating_sub(1);
            let end = symbol.end_line.saturating_sub(1);
            if start == end {
                vec![start, 0, 1]
            } else {
                vec![start, 0, end, 1]
            }
        })
}

fn byte_range_to_scip(source: &[u8], start: u64, end: u64) -> Option<Vec<u32>> {
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    let text = std::str::from_utf8(source).ok()?;
    if start >= end
        || end > source.len()
        || !text.is_char_boundary(start)
        || !text.is_char_boundary(end)
    {
        return None;
    }
    let (start_line, start_column) = byte_position(source, start)?;
    let (end_line, end_column) = byte_position(source, end)?;
    if start_line == end_line {
        Some(vec![start_line, start_column, end_column])
    } else {
        Some(vec![start_line, start_column, end_line, end_column])
    }
}

fn byte_position(source: &[u8], position: usize) -> Option<(u32, u32)> {
    if position > source.len() {
        return None;
    }
    let mut line = 0_u32;
    let mut line_start = 0_usize;
    for (index, byte) in source[..position].iter().enumerate() {
        if *byte == b'\n' {
            line = line.checked_add(1)?;
            line_start = index.checked_add(1)?;
        }
    }
    let column = u32::try_from(position.checked_sub(line_start)?).ok()?;
    Some((line, column))
}

fn bounded_documentation(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    vec![text[..text.floor_char_boundary(MAXIMUM_STRING_BYTES)].to_owned()]
}

fn symbol_display_name(symbol: &InterchangeSymbol, file_path: &str) -> String {
    let candidate = if symbol.symbol_kind == "file" {
        file_path.rsplit('/').next().unwrap_or(file_path)
    } else {
        symbol
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(&symbol.qualified_name)
    };
    if candidate.is_empty() {
        compact_id(&symbol.symbol_id)
    } else {
        candidate.to_owned()
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn document_item_counts(documents: &[ScipDocument]) -> Result<(u64, u64), ScipError> {
    documents
        .iter()
        .try_fold((0_u64, 0_u64), |counts, document| {
            Ok((
                counts
                    .0
                    .checked_add(usize_to_u64(document.symbols.len()))
                    .ok_or(ScipError::LimitExceeded)?,
                counts
                    .1
                    .checked_add(usize_to_u64(document.occurrences.len()))
                    .ok_or(ScipError::LimitExceeded)?,
            ))
        })
}

#[cfg(test)]
mod tests {
    use cartograph_db::{InterchangeFile, InterchangeReference, InterchangeSnapshot};
    use cartograph_domain::{FileId, GenerationId, SymbolId};

    use super::*;

    fn assert_confidence(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= f32::EPSILON,
            "confidence mismatch: expected {expected}, got {actual}"
        );
    }

    fn file_id(value: u8) -> FileId {
        FileId::from_uuid_v8([value; 16])
    }

    fn symbol_id(value: u8) -> SymbolId {
        SymbolId::from_uuid_v8([value; 16])
    }

    #[test]
    fn export_is_deterministic_and_retains_exact_typed_edges() {
        let source = b"fn caller() { callee(); }\nfn callee() {}\n".to_vec();
        let file = file_id(1);
        let source_symbol = symbol_id(2);
        let target_symbol = symbol_id(3);
        let snapshot = InterchangeSnapshot {
            generation_id: GenerationId::from_uuid_v8([4; 16]),
            files: vec![InterchangeFile {
                file_id: file.clone(),
                path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                content_hash: blake3::hash(&source).to_hex().to_string(),
                byte_size: usize_to_u64(source.len()),
            }],
            symbols: vec![
                InterchangeSymbol {
                    symbol_id: source_symbol.clone(),
                    file_id: file.clone(),
                    symbol_kind: "function".to_owned(),
                    qualified_name: "caller".to_owned(),
                    signature: "fn caller()".to_owned(),
                    code: "fn caller()".to_owned(),
                    natural_text: "Calls the callee.".to_owned(),
                    start_byte: 0,
                    end_byte: 25,
                    start_line: 1,
                    end_line: 1,
                },
                InterchangeSymbol {
                    symbol_id: target_symbol.clone(),
                    file_id: file.clone(),
                    symbol_kind: "function".to_owned(),
                    qualified_name: "callee".to_owned(),
                    signature: "fn callee()".to_owned(),
                    code: "fn callee()".to_owned(),
                    natural_text: String::new(),
                    start_byte: 26,
                    end_byte: 40,
                    start_line: 2,
                    end_line: 2,
                },
            ],
            edges: vec![InterchangeEdge {
                source_symbol_id: source_symbol.clone(),
                target_symbol_id: target_symbol.clone(),
                edge_kind: "calls".to_owned(),
                confidence: 0.95,
                provenance: "native-exact-project".to_owned(),
                site_count: 3,
            }],
            references: vec![InterchangeReference {
                file_id: file,
                owner_symbol_id: Some(source_symbol),
                target_symbol_id: Some(target_symbol),
                reference_name: "callee".to_owned(),
                reference_kind: "calls".to_owned(),
                start_byte: 14,
                end_byte: 20,
                site_count: 1,
            }],
        };
        let options = ScipExportOptions::new(ScipExportOptionsInput {
            project_name: "demo",
            project_version: "1",
            tool_version: "2.0.0",
            project_root_uri: "cartograph://demo",
        })
        .unwrap_or_else(|error| panic!("options failed: {error}"));
        let exported = export_snapshot(&snapshot, &options, |_| Some(source.clone()))
            .unwrap_or_else(|error| panic!("export failed: {error}"));
        assert_eq!(exported.stats().exact_typed_edges(), 1);
        assert_eq!(exported.stats().occurrences(), 3);
        let decoded = crate::decode_scip_index(exported.bytes())
            .unwrap_or_else(|error| panic!("decode failed: {error}"));
        let exact = decoded.documents[0]
            .symbols
            .iter()
            .flat_map(|symbol| &symbol.cartograph_edges)
            .collect::<Vec<_>>();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].site_count, 3);
        assert_eq!(exact[0].provenance, "native-exact-project");
        assert_confidence(f32::from_bits(exact[0].confidence_bits), 0.95);
    }
}
