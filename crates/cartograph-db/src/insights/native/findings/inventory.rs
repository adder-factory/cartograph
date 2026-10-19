//! Detector inputs stream from canonical metadata and compact topology.

use super::{clones, corrupt, framework, row_bytes, rules, workspace};
use crate::{
    FileInput, GraphDirection, SymbolInput,
    artifacts::native::summaries::visit_documents,
    insights::StructuralFinding,
    retrieval::native::simple_name,
    rocks::{GraphSnapshot, ReadBudget, RocksError, StateTransaction, StoredRevision},
};
use cartograph_domain::{DocumentKind, EdgeKind, FileId, SymbolId, Visibility};
use num_traits::ToPrimitive as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
};

const FIELDS: &[&str] = &[
    "code_lines",
    "parameter_count",
    "cyclomatic",
    "max_nesting",
    "max_conditional_operands",
    "magic_numbers",
    "hardcoded_urls",
    "hardcoded_url_requests",
    "hardcoded_url_configuration",
    "hardcoded_url_presentation_abstentions",
    "secrets_score",
    "stale_doc_numbers",
    "accidental_quadratic",
    "empty_catches",
    "sync_io_in_async",
    "sequential_await_loops",
    "serial_await_dependency_loops",
    "serial_await_control_flow_loops",
    "serial_await_intent_loops",
    "facade_factory_delegates",
    "ts_any_casts",
    "ts_suppressions",
    "debug_logs",
    "incomplete_markers",
    "dynamic_eval",
    "insecure_hash",
    "insecure_random",
    "http_without_timeout",
    "sql_string_concatenation",
    "unsafe_json_parse",
    "unvalidated_env",
    "empty_body",
    "literal_bytes",
];

pub(super) struct Document {
    pub(super) test: bool,
    health: Vec<(u8, f64)>,
    pub(super) clones: ClonePolicy,
    pub(super) shape: Option<String>,
    pub(super) partial: Partial,
    pub(super) secrets_actionable: bool,
    pub(super) facade_factory: bool,
}
pub(super) struct ClonePolicy {
    pub(super) enabled: bool,
    pub(super) near_compatible: bool,
}
#[derive(Default)]
pub(super) struct Partial {
    pub(super) peers: u64,
    pub(super) component: u64,
    pub(super) representative: Option<SymbolId>,
    pub(super) maximum: f64,
    pub(super) minimum: f64,
    pub(super) listed: Vec<SymbolId>,
}
impl Document {
    pub(super) fn health(&self, key: &str) -> f64 {
        self.health_or(key, 0.0)
    }
    pub(super) fn health_or(&self, key: &str, default: f64) -> f64 {
        FIELDS
            .iter()
            .position(|name| *name == key)
            .and_then(|index| self.health.iter().find(|(id, _)| usize::from(*id) == index))
            .map_or(default, |(_, value)| *value)
    }
    fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.health.capacity() * size_of::<(u8, f64)>()
            + self.shape.as_ref().map_or(0, String::capacity)
            + self.partial.listed.capacity() * 96
            + 256
    }
}
pub(super) struct Base<'a> {
    pub(super) symbol: &'a SymbolInput,
    pub(super) file: &'a FileInput,
    pub(super) document: &'a Document,
    pub(super) topology: Topology,
    pub(super) unresolved: u64,
    pub(super) external: u64,
    pub(super) centrality: f64,
    pub(super) envy: Option<&'a Envy>,
    pub(super) coverage: Option<(f64, f64)>,
    pub(super) prior: Option<u32>,
    pub(super) recent_prior: bool,
}
#[derive(Default)]
pub(super) struct Topology {
    pub(super) outgoing: u64,
    pub(super) groups: [u64; 5],
    pub(super) incoming: u64,
    pub(super) external: u64,
    pub(super) exported_api: u64,
    pub(super) function_local: bool,
    pub(super) framework: bool,
    pub(super) behavioral: u64,
    pub(super) complex: u64,
}
#[derive(Default)]
pub(super) struct Envy {
    pub(super) fields: BTreeSet<SymbolId>,
    pub(super) classes: BTreeSet<SymbolId>,
    pub(super) own: u64,
    pub(super) foreign: u64,
}
#[derive(Default)]
struct References {
    unresolved: BTreeMap<SymbolId, u64>,
    dynamic: BTreeMap<String, BTreeMap<FileId, u64>>,
    envy: BTreeMap<SymbolId, Envy>,
    bytes: usize,
}

pub(super) fn compute(
    transaction: &StateTransaction<'_>,
    budget: &mut ReadBudget,
    previous: Option<&StoredRevision>,
) -> Result<(Vec<StructuralFinding>, u64), RocksError> {
    let snapshot = transaction.graph_snapshot()?;
    let graph = snapshot.graph(budget)?;
    let documents = documents(&graph, budget)?;
    let population = population(&graph, &documents, budget)?;
    let references = references(&graph, budget)?;
    let coverage = coverage(transaction, &graph, budget)?;
    let clone_classes = clones::Classes::new(transaction, &graph, &documents, budget)?;
    let prior = previous
        .map(|previous| transaction.retained_graph_snapshot(&previous.generation_id))
        .transpose()?;
    let prior_graph = prior
        .as_ref()
        .map(|snapshot| snapshot.graph(budget))
        .transpose()?;
    let recent_prior = previous
        .and_then(|previous| previous.published_at.as_ref())
        .map(|time| time.parse::<jiff::Timestamp>().map_err(|_| corrupt()))
        .transpose()?
        .is_some_and(|time| {
            time.as_second()
                >= jiff::Timestamp::now()
                    .as_second()
                    .saturating_sub(30 * 86400)
        });
    let mut rows = Vec::new();
    let mut bytes = 0_usize;
    let mut analyzed = 0_u64;
    for (id, document) in &documents {
        budget.checkpoint()?;
        if document.test {
            continue;
        }
        let symbol = graph.symbol(id, budget)?.ok_or(corrupt())?;
        if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
            continue;
        }
        let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
        let topology = topology(&graph, &symbol, &file, Some(&documents), budget)?;
        let potential = dynamic_external_references(&references, &symbol)?;
        let external = topology
            .external
            .checked_add(topology.exported_api)
            .and_then(|sum| sum.checked_add(potential))
            .ok_or(corrupt())?;
        let centrality = (topology.incoming.to_f64().ok_or(corrupt())? / population).min(1.0);
        let prior = prior_graph
            .as_ref()
            .map(|graph| graph.symbol(id, budget))
            .transpose()?
            .flatten()
            .map(|symbol| symbol.end_line - symbol.start_line + 1);
        let base = Base {
            symbol: &symbol,
            file: &file,
            document,
            topology,
            external,
            centrality,
            unresolved: references.unresolved.get(id).copied().unwrap_or(0),
            envy: references.envy.get(id),
            coverage: coverage.get(id).copied(),
            prior,
            recent_prior,
        };
        let mut findings = rules::evaluate(&base)?;
        if let Some(finding) = clone_classes.finding(&base, &graph, budget)? {
            findings.push(finding);
        }
        for finding in findings {
            bytes = bytes
                .checked_add(row_bytes(&finding))
                .ok_or_else(workspace)?;
            if bytes > 16 * 1_048_576 {
                return Err(workspace());
            }
            rows.push(finding);
        }
        analyzed += 1;
    }
    Ok((rows, analyzed))
}

fn dynamic_external_references(
    references: &References,
    symbol: &SymbolInput,
) -> Result<u64, RocksError> {
    if symbol.symbol_kind != "method"
        || !symbol.export.exported
        || symbol.visibility == Some(Visibility::Private)
    {
        return Ok(0);
    }
    references
        .dynamic
        .get(simple_name(&symbol.qualified_name))
        .map_or(Ok(0), |files| {
            files
                .iter()
                .filter(|(file, _)| *file != &symbol.file_id)
                .try_fold(0_u64, |sum, (_, count)| {
                    sum.checked_add(*count).ok_or(corrupt())
                })
        })
}

fn population(
    graph: &GraphSnapshot<'_, '_>,
    documents: &BTreeMap<SymbolId, Document>,
    budget: &mut ReadBudget,
) -> Result<f64, RocksError> {
    let mut count = 0_u64;
    for (id, document) in documents {
        if !document.test
            && graph.symbol(id, budget)?.is_some_and(|symbol| {
                !matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter")
            })
        {
            count += 1;
        }
    }
    count.saturating_sub(1).max(1).to_f64().ok_or(corrupt())
}

fn documents(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<SymbolId, Document>, RocksError> {
    let mut output = BTreeMap::<SymbolId, Document>::new();
    let mut bytes = 0_usize;
    visit_documents(graph, budget, |document, _| {
        if !matches!(document.kind, DocumentKind::Symbol | DocumentKind::Test) {
            return Ok(());
        }
        let Some(id) = document.symbol_id else {
            return Ok(());
        };
        if output.get(&id).is_some_and(|document| document.test) {
            return Ok(());
        }
        let value: serde_json::Value =
            serde_json::from_str(&document.metadata_json).map_err(|_| corrupt())?;
        let parsed = parse_document(&value, document.kind == DocumentKind::Test)?;
        bytes = bytes
            .checked_sub(output.get(&id).map_or(0, Document::bytes))
            .and_then(|bytes| bytes.checked_add(parsed.bytes()))
            .ok_or_else(workspace)?;
        if bytes > 24 * 1_048_576 {
            return Err(workspace());
        }
        output.insert(id, parsed);
        Ok(())
    })?;
    Ok(output)
}
fn parse_document(value: &serde_json::Value, test: bool) -> Result<Document, RocksError> {
    let mut health = Vec::new();
    for (index, &key) in FIELDS.iter().enumerate() {
        let number = number(value.get("health").and_then(|health| health.get(key)))?;
        if number != 0.0
            || value
                .get("health")
                .and_then(|health| health.get(key))
                .is_some_and(|value| !value.is_null())
        {
            health.push((u8::try_from(index).map_err(|_| corrupt())?, number));
        }
    }
    let partial = value.get("partial_clone");
    let field = |key| partial.and_then(|partial| partial.get(key));
    let representative = field("representative_symbol_id")
        .and_then(serde_json::Value::as_str)
        .map(SymbolId::parse)
        .transpose()
        .map_err(|_| corrupt())?;
    let mut listed = Vec::new();
    if let Some(values) = field("listed_peer_symbol_ids") {
        let values = values.as_array().ok_or(corrupt())?;
        if values.len() > 128 {
            return Err(workspace());
        }
        for value in values {
            listed.push(SymbolId::parse(value.as_str().ok_or(corrupt())?).map_err(|_| corrupt())?);
        }
    }
    let integer = |key| number(field(key))?.to_u64().ok_or(corrupt());
    let shape = value
        .get("clone_shape_digest")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    if shape.as_ref().is_some_and(|shape| shape.len() > 4096) {
        return Err(workspace());
    }
    Ok(Document {
        test,
        health,
        shape,
        clones: ClonePolicy {
            enabled: boolean(value.get("duplicate_detection_enabled"), true)?,
            near_compatible: boolean(value.get("near_clone_compatible"), false)?,
        },
        secrets_actionable: boolean(
            value
                .get("health")
                .and_then(|value| value.get("secrets_actionable")),
            false,
        )?,
        facade_factory: boolean(
            value
                .get("health")
                .and_then(|value| value.get("facade_factory")),
            false,
        )?,
        partial: Partial {
            peers: integer("peer_count")?,
            component: integer("component_size")?,
            representative,
            maximum: number(field("maximum_overlap_ppm"))? / 1_000_000.0,
            minimum: number(field("minimum_overlap_ppm"))? / 1_000_000.0,
            listed,
        },
    })
}
fn number(value: Option<&serde_json::Value>) -> Result<f64, RocksError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(0.0);
    };
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or(corrupt())?;
    if !number.is_finite() || number < 0.0 {
        return Err(corrupt());
    }
    Ok(number)
}
fn boolean(value: Option<&serde_json::Value>, default: bool) -> Result<bool, RocksError> {
    value
        .filter(|value| !value.is_null())
        .map_or(Ok(default), |value| {
            value
                .as_bool()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .ok_or(corrupt())
        })
}

pub(super) fn topology(
    graph: &GraphSnapshot<'_, '_>,
    symbol: &SymbolInput,
    file: &FileInput,
    documents: Option<&BTreeMap<SymbolId, Document>>,
    budget: &mut ReadBudget,
) -> Result<Topology, RocksError> {
    let mut output = Topology::default();
    let contains = contained_symbols(graph, symbol, documents, budget, &mut output)?;
    (output.outgoing, output.groups) = dependency_topology(graph, symbol, &contains, budget)?;
    let mut parent_default = false;
    super::super::neighbors(
        graph,
        &symbol.symbol_id,
        GraphDirection::Incoming,
        budget,
        |link, budget| {
            let source = graph
                .symbol(link.source_symbol_id(), budget)?
                .ok_or(corrupt())?;
            if link.kind() == EdgeKind::Contains {
                output.function_local |= matches!(
                    source.symbol_kind.as_str(),
                    "function" | "method" | "component"
                );
                parent_default |= source.export.default_export;
            } else {
                let count = u64::from(graph.edge(&link, budget)?.site_count);
                output.incoming = output.incoming.checked_add(count).ok_or(corrupt())?;
                if source.file_id != symbol.file_id {
                    output.external += 1;
                }
                if source.file_id == symbol.file_id
                    && source.symbol_id != symbol.symbol_id
                    && source.export.exported
                    && matches!(
                        link.kind(),
                        EdgeKind::Calls
                            | EdgeKind::References
                            | EdgeKind::TypeOf
                            | EdgeKind::Returns
                            | EdgeKind::Instantiates
                            | EdgeKind::DefUse
                    )
                {
                    output.exported_api = output.exported_api.checked_add(count).ok_or(corrupt())?;
                }
            }
            Ok(())
        },
    )?;
    output.framework = framework::convention(symbol, &file.normalized_path, parent_default)?;
    Ok(output)
}

fn contained_symbols(
    graph: &GraphSnapshot<'_, '_>,
    symbol: &SymbolInput,
    documents: Option<&BTreeMap<SymbolId, Document>>,
    budget: &mut ReadBudget,
    output: &mut Topology,
) -> Result<BTreeSet<SymbolId>, RocksError> {
    let mut contains = BTreeSet::new();
    super::super::neighbors(
        graph,
        &symbol.symbol_id,
        GraphDirection::Outgoing,
        budget,
        |link, budget| {
            if link.kind() == EdgeKind::Contains {
                contains.insert(link.target_symbol_id().clone());
                if contains.len() > 16_384 {
                    return Err(workspace());
                }
                let child = graph
                    .symbol(link.target_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                if matches!(child.symbol_kind.as_str(), "function" | "method")
                    && let Some(document) = documents
                        .and_then(|documents| documents.get(&child.symbol_id))
                        .filter(|document| !document.test)
                {
                    output.behavioral += u64::from(document.health("code_lines") >= 5.0);
                    output.complex += u64::from(document.health("cyclomatic") >= 3.0);
                }
            }
            Ok(())
        },
    )?;
    Ok(contains)
}

fn dependency_topology(
    graph: &GraphSnapshot<'_, '_>,
    symbol: &SymbolInput,
    contains: &BTreeSet<SymbolId>,
    budget: &mut ReadBudget,
) -> Result<(u64, [u64; 5]), RocksError> {
    let mut dependencies = BTreeSet::new();
    let mut groups: [BTreeSet<SymbolId>; 5] = std::array::from_fn(|_| BTreeSet::new());
    super::super::neighbors(
        graph,
        &symbol.symbol_id,
        GraphDirection::Outgoing,
        budget,
        |link, _| {
            if link.kind() != EdgeKind::Contains
                && link.target_symbol_id() != &symbol.symbol_id
                && !contains.contains(link.target_symbol_id())
            {
                dependencies.insert(link.target_symbol_id().clone());
                if let Some(index) = dependency_group(link.kind()) {
                    groups[index].insert(link.target_symbol_id().clone());
                }
                if dependencies.len() > 16_384 {
                    return Err(workspace());
                }
            }
            Ok(())
        },
    )?;
    Ok((
        dependencies.len() as u64,
        groups.map(|group| group.len() as u64),
    ))
}
fn dependency_group(kind: EdgeKind) -> Option<usize> {
    match kind {
        EdgeKind::Calls => Some(0),
        EdgeKind::Imports | EdgeKind::Exports => Some(1),
        EdgeKind::Implements | EdgeKind::Extends | EdgeKind::TypeOf | EdgeKind::Returns => Some(2),
        EdgeKind::Instantiates | EdgeKind::Decorates => Some(3),
        EdgeKind::FieldAccess | EdgeKind::DefUse => Some(4),
        _ => None,
    }
}

fn references(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<References, RocksError> {
    let mut output = References::default();
    let mut classes = Parents::default();
    super::super::visit_references(graph, budget, |reference, budget| {
        if reference.target_symbol_id.is_none() {
            record_unresolved_reference(&mut output, reference)?;
        } else if reference.reference_kind == "field_access" {
            record_envy(graph, budget, &reference, &mut output, &mut classes)?;
        }
        if output.bytes > 8 * 1_048_576 {
            return Err(workspace());
        }
        Ok(())
    })?;
    Ok(output)
}
fn record_unresolved_reference(
    output: &mut References,
    reference: crate::ReferenceInput,
) -> Result<(), RocksError> {
    let count = u64::from(reference.site_count);
    if reference.reference_kind == "calls"
        && reference.resolution_provenance == "native-dynamic-unresolved"
    {
        let name = dynamic_name(&reference.reference_name);
        if !output.dynamic.contains_key(name) {
            output.bytes += name.len() + 128;
        }
        let files = output.dynamic.entry(name.to_owned()).or_default();
        if !files.contains_key(&reference.file_id) {
            output.bytes += 128;
        }
        let value = files.entry(reference.file_id).or_default();
        *value = value.checked_add(count).ok_or(corrupt())?;
    } else if !excluded_unresolved(&reference.resolution_provenance)
        && let Some(owner) = reference.owner_symbol_id
    {
        if !output.unresolved.contains_key(&owner) {
            output.bytes += 128;
        }
        let value = output.unresolved.entry(owner).or_default();
        *value = value.checked_add(count).ok_or(corrupt())?;
    }
    Ok(())
}

fn dynamic_name(name: &str) -> &str {
    let Some((_, tail)) = name.rsplit_once('.') else {
        return name;
    };
    let candidate = tail.split("::").next().unwrap_or(tail);
    if candidate
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        candidate
    } else {
        name
    }
}
fn excluded_unresolved(provenance: &str) -> bool {
    matches!(
        provenance,
        "native-dynamic-unresolved"
            | "native-member-unresolved"
            | "native-rust-external"
            | "native-rust-intrinsic"
            | "native-rust-macro-unexpanded"
            | "native-external-reference"
            | "native-javascript-intrinsic"
            | "native-python-intrinsic"
            | "native-shell-command"
            | "native-manifest-reference"
    ) || (provenance.starts_with("native-embedded-sql-") && provenance.ends_with("-unresolved"))
}
#[derive(Default)]
struct Parents {
    values: BTreeMap<SymbolId, Vec<(SymbolId, bool)>>,
    bytes: usize,
}
fn parents(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    id: &SymbolId,
    cache: &mut Parents,
) -> Result<Vec<(SymbolId, bool)>, RocksError> {
    if let Some(parents) = cache.values.get(id) {
        return Ok(parents.clone());
    }
    let mut parents = BTreeMap::new();
    super::super::neighbors(
        graph,
        id,
        GraphDirection::Incoming,
        budget,
        |link, budget| {
            if link.kind() == EdgeKind::Contains {
                let parent = graph
                    .symbol(link.source_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                if matches!(
                    parent.symbol_kind.as_str(),
                    "class" | "struct" | "trait" | "interface"
                ) {
                    parents.insert(parent.symbol_id, parent.symbol_kind == "interface");
                    if parents.len() > 32 {
                        return Err(workspace());
                    }
                }
            }
            Ok(())
        },
    )?;
    let parents = parents.into_iter().collect::<Vec<_>>();
    cache.bytes = cache
        .bytes
        .checked_add(128 + parents.len() * 96)
        .ok_or_else(workspace)?;
    if cache.bytes > 8 * 1_048_576 {
        return Err(workspace());
    }
    cache.values.insert(id.clone(), parents.clone());
    Ok(parents)
}
fn record_envy(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    reference: &crate::ReferenceInput,
    output: &mut References,
    cache: &mut Parents,
) -> Result<(), RocksError> {
    let (Some(owner), Some(target)) = (&reference.owner_symbol_id, &reference.target_symbol_id)
    else {
        return Ok(());
    };
    let source = graph.symbol(owner, budget)?.ok_or(corrupt())?;
    let field = graph.symbol(target, budget)?.ok_or(corrupt())?;
    if source.symbol_kind != "method"
        || !matches!(field.symbol_kind.as_str(), "field" | "property" | "method")
    {
        return Ok(());
    }
    let sources = parents(graph, budget, owner, cache)?;
    let targets = parents(graph, budget, target, cache)?;
    for (source, interface) in sources {
        if interface {
            continue;
        }
        for (target_class, _) in &targets {
            record_envy_pair(
                output,
                owner,
                target,
                &source == target_class,
                target_class,
                u64::from(reference.site_count),
            )?;
        }
    }
    Ok(())
}
fn record_envy_pair(
    output: &mut References,
    owner: &SymbolId,
    target: &SymbolId,
    own: bool,
    target_class: &SymbolId,
    count: u64,
) -> Result<(), RocksError> {
    if !output.envy.contains_key(owner) {
        output.bytes += 256;
    }
    let envy = output.envy.entry(owner.clone()).or_default();
    if own {
        envy.own = envy.own.checked_add(count).ok_or(corrupt())?;
    } else {
        envy.foreign = envy.foreign.checked_add(count).ok_or(corrupt())?;
        if envy.fields.insert(target.clone()) {
            output.bytes += 128;
        }
        if envy.classes.insert(target_class.clone()) {
            output.bytes += 128;
        }
    }
    Ok(())
}

fn coverage(
    transaction: &StateTransaction<'_>,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<SymbolId, (f64, f64)>, RocksError> {
    let mut rows = Vec::new();
    for (id, fraction) in crate::coverage::native::fractions(transaction, budget)? {
        let mut incoming = 0_u64;
        super::super::neighbors(
            graph,
            &id,
            GraphDirection::Incoming,
            budget,
            |link, budget| {
                if link.kind() != EdgeKind::Contains {
                    incoming = incoming
                        .checked_add(u64::from(graph.edge(&link, budget)?.site_count))
                        .ok_or(corrupt())?;
                }
                Ok(())
            },
        )?;
        rows.push((incoming, id, fraction));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let denominator = rows
        .len()
        .saturating_sub(1)
        .max(1)
        .to_f64()
        .ok_or(corrupt())?;
    let mut output = BTreeMap::new();
    let mut rank = 0_usize;
    let mut previous = None;
    for (index, (incoming, id, fraction)) in rows.into_iter().enumerate() {
        if previous != Some(incoming) {
            rank = index;
            previous = Some(incoming);
        }
        output.insert(
            id,
            (fraction, rank.to_f64().ok_or(corrupt())? / denominator),
        );
    }
    Ok(output)
}
