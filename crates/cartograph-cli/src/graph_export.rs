use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use cartograph_db::{
    CartographDatabase, InterchangeEdge, InterchangeSnapshot, InterchangeSnapshotRequest,
    InterchangeSymbol,
};
use cartograph_domain::{EdgeKind, ProjectId, SymbolKind};
use clap::ValueEnum;
use serde::Serialize;

const DEFAULT_MAXIMUM_ROWS: u64 = 5_000_000;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_mins(10);

pub(super) const DEFAULT_NODE_LIMIT: u16 = 1_000;
const MAXIMUM_NODE_LIMIT: u16 = 50_000;

/// Stable spellings `--kind` accepts: every symbol kind the domain defines.
fn node_kind_names() -> Vec<&'static str> {
    SymbolKind::ALL.iter().map(|kind| kind.as_str()).collect()
}

/// Stable spellings `--edge-kind` accepts: every stored edge kind the domain defines.
fn edge_kind_names() -> Vec<&'static str> {
    EdgeKind::ALL.iter().map(|kind| kind.as_str()).collect()
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub(super) enum GraphExportFormat {
    #[default]
    Json,
    Dot,
    Mermaid,
    Cytoscape,
}

pub(super) struct GraphExportRequest {
    pub project_id: ProjectId,
    pub format: GraphExportFormat,
    pub output: Option<PathBuf>,
    pub limit: u16,
    pub kinds: Option<String>,
    pub edge_kinds: Option<String>,
    pub languages: Option<String>,
    pub file_prefix: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphExportSnapshot {
    format_version: u8,
    generation_id: String,
    filters: GraphExportFilters,
    stats: GraphExportStats,
    nodes: Vec<GraphExportNode>,
    edges: Vec<GraphExportEdge>,
    files: Vec<GraphExportFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphExportFilters {
    kinds: Vec<String>,
    edge_kinds: Vec<String>,
    languages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_prefix: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphExportStats {
    total_nodes: usize,
    total_edges: usize,
    exported_nodes: usize,
    exported_edges: usize,
    exported_files: usize,
    truncated_nodes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphExportNode {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    signature: String,
    file_path: String,
    language: String,
    start_line: u32,
    end_line: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphExportEdge {
    source: String,
    target: String,
    kind: String,
    confidence: f32,
    provenance: String,
    site_count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphExportFile {
    path: String,
    language: String,
    node_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CytoscapeExport<'a> {
    format_version: u8,
    metadata: CytoscapeMetadata<'a>,
    elements: CytoscapeElements<'a>,
}

#[derive(Serialize)]
struct CytoscapeMetadata<'a> {
    filters: &'a GraphExportFilters,
    stats: &'a GraphExportStats,
}

#[derive(Serialize)]
struct CytoscapeElements<'a> {
    nodes: Vec<CytoscapeNode<'a>>,
    edges: Vec<CytoscapeEdge<'a>>,
}

#[derive(Serialize)]
struct CytoscapeNode<'a> {
    data: &'a GraphExportNode,
}

#[derive(Serialize)]
struct CytoscapeEdge<'a> {
    data: CytoscapeEdgeData<'a>,
}

#[derive(Serialize)]
struct CytoscapeEdgeData<'a> {
    id: String,
    source: &'a str,
    target: &'a str,
    kind: &'a str,
    label: &'a str,
    confidence: f32,
    provenance: &'a str,
    site_count: u32,
}

pub(super) async fn run_graph_export(
    database: &CartographDatabase,
    request: GraphExportRequest,
) -> Result<String, String> {
    if request.limit == 0 || request.limit > MAXIMUM_NODE_LIMIT {
        return Err(format!(
            "--limit must be between 1 and {MAXIMUM_NODE_LIMIT}"
        ));
    }
    let kinds = parse_filter(request.kinds.as_deref(), &node_kind_names(), "--kind")?;
    let edge_kinds = parse_filter(
        request.edge_kinds.as_deref(),
        &edge_kind_names(),
        "--edge-kind",
    )?;
    let languages = parse_unrestricted_filter(request.languages.as_deref());
    let file_prefix = request.file_prefix.as_deref().map(normalize_prefix);
    let raw = database
        .current_interchange_snapshot(InterchangeSnapshotRequest {
            project_id: &request.project_id,
            maximum_rows: DEFAULT_MAXIMUM_ROWS,
            statement_timeout: SNAPSHOT_TIMEOUT,
        })
        .await
        .map_err(|error| error.to_string())?;
    let snapshot = build_snapshot(GraphSnapshotInput {
        raw,
        limit: request.limit,
        kinds,
        edge_kinds,
        languages,
        file_prefix,
    });
    let artifact = format_snapshot(&snapshot, request.format)?;
    if let Some(output) = request.output {
        write_artifact(&output, artifact.as_bytes())?;
        Ok(format!(
            "Exported graph snapshot -> {} ({} nodes, {} edges)\n",
            output.display(),
            snapshot.stats.exported_nodes,
            snapshot.stats.exported_edges
        ))
    } else {
        Ok(artifact)
    }
}

struct GraphSnapshotInput {
    raw: InterchangeSnapshot,
    limit: u16,
    kinds: Vec<String>,
    edge_kinds: Vec<String>,
    languages: Vec<String>,
    file_prefix: Option<String>,
}

fn build_snapshot(input: GraphSnapshotInput) -> GraphExportSnapshot {
    let GraphSnapshotInput {
        raw,
        limit,
        kinds,
        edge_kinds,
        languages,
        file_prefix,
    } = input;
    let kind_filter = nonempty_set(&kinds);
    let edge_filter = nonempty_set(&edge_kinds);
    let language_filter = nonempty_set(&languages);
    let total_nodes = raw.symbols.len();
    let total_edges = raw.edges.len();
    let files = raw
        .files
        .iter()
        .map(|file| (file.file_id.as_str(), (&file.path, &file.language)))
        .collect::<BTreeMap<_, _>>();
    let mut filtered = raw
        .symbols
        .into_iter()
        .filter(|symbol| {
            let Some((path, language)) = files.get(symbol.file_id.as_str()) else {
                return false;
            };
            kind_filter
                .as_ref()
                .is_none_or(|filter| filter.contains(symbol.symbol_kind.as_str()))
                && language_filter
                    .as_ref()
                    .is_none_or(|filter| filter.contains(language.as_str()))
                && file_prefix
                    .as_ref()
                    .is_none_or(|prefix| path.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    filtered.sort_by(|left, right| {
        files
            .get(left.file_id.as_str())
            .map(|(path, _)| path.as_str())
            .cmp(
                &files
                    .get(right.file_id.as_str())
                    .map(|(path, _)| path.as_str()),
            )
            .then(left.start_line.cmp(&right.start_line))
            .then(left.symbol_kind.cmp(&right.symbol_kind))
            .then(left.qualified_name.cmp(&right.qualified_name))
            .then(left.symbol_id.cmp(&right.symbol_id))
    });
    let truncated_nodes = filtered.len().saturating_sub(usize::from(limit));
    filtered.truncate(usize::from(limit));
    let exported_ids = filtered
        .iter()
        .map(|symbol| symbol.symbol_id.as_str())
        .collect::<BTreeSet<_>>();
    let edges = raw
        .edges
        .into_iter()
        .filter(|edge| {
            exported_ids.contains(edge.source_symbol_id.as_str())
                && exported_ids.contains(edge.target_symbol_id.as_str())
                && edge_filter
                    .as_ref()
                    .is_none_or(|filter| filter.contains(edge.edge_kind.as_str()))
        })
        .map(export_edge)
        .collect::<Vec<_>>();
    let (nodes, exported_files) = export_nodes(filtered, &files);
    GraphExportSnapshot {
        format_version: 1,
        generation_id: raw.generation_id.as_str().to_owned(),
        filters: GraphExportFilters {
            kinds,
            edge_kinds,
            languages,
            file_prefix,
        },
        stats: GraphExportStats {
            total_nodes,
            total_edges,
            exported_nodes: nodes.len(),
            exported_edges: edges.len(),
            exported_files: exported_files.len(),
            truncated_nodes,
        },
        nodes,
        edges,
        files: exported_files,
    }
}

fn export_nodes(
    symbols: Vec<InterchangeSymbol>,
    files: &BTreeMap<&str, (&String, &String)>,
) -> (Vec<GraphExportNode>, Vec<GraphExportFile>) {
    let mut file_counts = BTreeMap::<String, (String, usize)>::new();
    let nodes = symbols
        .into_iter()
        .filter_map(|symbol| {
            let (path, language) = files.get(symbol.file_id.as_str())?;
            let entry = file_counts
                .entry((*path).clone())
                .or_insert_with(|| ((*language).clone(), 0));
            entry.1 = entry.1.saturating_add(1);
            Some(GraphExportNode {
                id: symbol.symbol_id.as_str().to_owned(),
                kind: symbol.symbol_kind,
                name: short_name(&symbol.qualified_name),
                qualified_name: symbol.qualified_name,
                signature: symbol.signature,
                file_path: (*path).clone(),
                language: (*language).clone(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
            })
        })
        .collect::<Vec<_>>();
    let exported_files = file_counts
        .into_iter()
        .map(|(path, (language, node_count))| GraphExportFile {
            path,
            language,
            node_count,
        })
        .collect::<Vec<_>>();
    (nodes, exported_files)
}

fn export_edge(edge: InterchangeEdge) -> GraphExportEdge {
    GraphExportEdge {
        source: edge.source_symbol_id.as_str().to_owned(),
        target: edge.target_symbol_id.as_str().to_owned(),
        kind: edge.edge_kind,
        confidence: edge.confidence,
        provenance: edge.provenance,
        site_count: edge.site_count,
    }
}

fn format_snapshot(
    snapshot: &GraphExportSnapshot,
    format: GraphExportFormat,
) -> Result<String, String> {
    match format {
        GraphExportFormat::Json => pretty_json(snapshot),
        GraphExportFormat::Cytoscape => {
            let export = CytoscapeExport {
                format_version: 1,
                metadata: CytoscapeMetadata {
                    filters: &snapshot.filters,
                    stats: &snapshot.stats,
                },
                elements: CytoscapeElements {
                    nodes: snapshot
                        .nodes
                        .iter()
                        .map(|data| CytoscapeNode { data })
                        .collect(),
                    edges: snapshot
                        .edges
                        .iter()
                        .enumerate()
                        .map(|(index, edge)| CytoscapeEdge {
                            data: CytoscapeEdgeData {
                                id: format!("e{index}"),
                                source: &edge.source,
                                target: &edge.target,
                                kind: &edge.kind,
                                label: &edge.kind,
                                confidence: edge.confidence,
                                provenance: &edge.provenance,
                                site_count: edge.site_count,
                            },
                        })
                        .collect(),
                },
            };
            pretty_json(&export)
        }
        GraphExportFormat::Dot => Ok(format_dot(snapshot)),
        GraphExportFormat::Mermaid => Ok(format_mermaid(snapshot)),
    }
}

fn pretty_json(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map(|value| format!("{value}\n"))
        .map_err(|_| "could not serialize graph export".to_owned())
}

fn format_dot(snapshot: &GraphExportSnapshot) -> String {
    let mut lines = vec![
        "digraph cartograph {".to_owned(),
        "  graph [rankdir=LR];".to_owned(),
        "  node [shape=box, style=\"rounded\"];".to_owned(),
        "  edge [fontsize=10];".to_owned(),
    ];
    for node in &snapshot.nodes {
        lines.push(format!(
            "  {} [label={}];",
            json_quote(&node.id),
            json_quote(&node_label(node))
        ));
    }
    for edge in &snapshot.edges {
        lines.push(format!(
            "  {} -> {} [label={}];",
            json_quote(&edge.source),
            json_quote(&edge.target),
            json_quote(&edge.kind)
        ));
    }
    lines.push("}".to_owned());
    format!("{}\n", lines.join("\n"))
}

fn format_mermaid(snapshot: &GraphExportSnapshot) -> String {
    let identifiers = snapshot
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), format!("n{index}")))
        .collect::<BTreeMap<_, _>>();
    let mut lines = vec!["flowchart LR".to_owned()];
    for node in &snapshot.nodes {
        if let Some(identifier) = identifiers.get(node.id.as_str()) {
            lines.push(format!(
                "  {identifier}[\"{}\"]",
                escape_mermaid(&node_label(node))
            ));
        }
    }
    for edge in &snapshot.edges {
        let (Some(source), Some(target)) = (
            identifiers.get(edge.source.as_str()),
            identifiers.get(edge.target.as_str()),
        ) else {
            continue;
        };
        lines.push(format!(
            "  {source} -->|{}| {target}",
            escape_mermaid(&edge.kind)
        ));
    }
    format!("{}\n", lines.join("\n"))
}

fn parse_filter(raw: Option<&str>, allowed: &[&str], label: &str) -> Result<Vec<String>, String> {
    let values = parse_unrestricted_filter(raw);
    let invalid = values
        .iter()
        .filter(|value| !allowed.contains(&value.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if invalid.is_empty() {
        Ok(values)
    } else {
        Err(format!(
            "{label} contains unsupported values: {}; allowed values: {}",
            invalid.join(", "),
            allowed.join(", ")
        ))
    }
}

fn parse_unrestricted_filter(raw: Option<&str>) -> Vec<String> {
    raw.into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn nonempty_set(values: &[String]) -> Option<BTreeSet<&str>> {
    (!values.is_empty()).then(|| values.iter().map(String::as_str).collect())
}

fn normalize_prefix(value: &str) -> String {
    value
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned()
}

fn short_name(qualified: &str) -> String {
    qualified
        .rsplit([':', '.', '#'])
        .find(|part| !part.is_empty())
        .unwrap_or(qualified)
        .to_owned()
}

fn node_label(node: &GraphExportNode) -> String {
    format!(
        "{}\n{}\n{}:{}",
        node.qualified_name, node.kind, node.file_path, node.start_line
    )
}

fn json_quote(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"invalid\"".to_owned())
}

fn escape_mermaid(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('[', "&#91;")
        .replace(']', "&#93;")
        .replace('|', "&#124;")
        .replace('\n', "<br/>")
}

fn write_artifact(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|_| {
            format!(
                "could not create graph export directory {}",
                parent.display()
            )
        })?;
    }
    fs::write(path, bytes)
        .map_err(|_| format!("could not write graph export artifact {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cartograph_db::{InterchangeFile, InterchangeSymbol};
    use cartograph_domain::{FileId, GenerationId, SymbolId};

    #[test]
    fn filters_limits_and_formats_a_deterministic_graph_snapshot() {
        let file = FileId::parse("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap_or_else(|error| panic!("invalid file fixture: {error}"));
        let source = SymbolId::parse("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .unwrap_or_else(|error| panic!("invalid source fixture: {error}"));
        let target = SymbolId::parse("cccccccc-cccc-4ccc-8ccc-cccccccccccc")
            .unwrap_or_else(|error| panic!("invalid target fixture: {error}"));
        let symbol = |id: SymbolId, name: &str, line| InterchangeSymbol {
            symbol_id: id,
            file_id: file.clone(),
            symbol_kind: "function".to_owned(),
            qualified_name: name.to_owned(),
            signature: format!("fn {name}()"),
            code: String::new(),
            natural_text: String::new(),
            start_byte: 0,
            end_byte: 1,
            start_line: line,
            end_line: line,
        };
        let raw = InterchangeSnapshot {
            generation_id: GenerationId::parse("dddddddd-dddd-4ddd-8ddd-dddddddddddd")
                .unwrap_or_else(|error| panic!("invalid generation fixture: {error}")),
            files: vec![InterchangeFile {
                file_id: file.clone(),
                path: "src/lib.rs".to_owned(),
                language: "rust".to_owned(),
                content_hash: "1111111111111111111111111111111111111111111111111111111111111111"
                    .to_owned(),
                byte_size: 10,
            }],
            symbols: vec![
                symbol(source.clone(), "alpha", 1),
                symbol(target.clone(), "beta", 2),
            ],
            edges: vec![InterchangeEdge {
                source_symbol_id: source,
                target_symbol_id: target,
                edge_kind: "calls".to_owned(),
                confidence: 1.0,
                provenance: "test".to_owned(),
                site_count: 2,
            }],
            references: Vec::new(),
        };
        let snapshot = build_snapshot(GraphSnapshotInput {
            raw,
            limit: 2,
            kinds: vec!["function".to_owned()],
            edge_kinds: vec!["calls".to_owned()],
            languages: vec!["rust".to_owned()],
            file_prefix: Some("src/".to_owned()),
        });
        assert_eq!(snapshot.stats.exported_nodes, 2);
        assert_eq!(snapshot.stats.exported_edges, 1);
        assert!(format_dot(&snapshot).contains("digraph cartograph"));
        assert!(format_mermaid(&snapshot).contains("flowchart LR"));
        let json = format_snapshot(&snapshot, GraphExportFormat::Cytoscape)
            .unwrap_or_else(|error| panic!("cytoscape format failed: {error}"));
        assert!(json.contains("site_count"));
    }

    #[test]
    fn enum_filters_reject_unknown_values() {
        assert!(parse_filter(Some("function,unknown"), &node_kind_names(), "--kind").is_err());
        assert_eq!(
            parse_unrestricted_filter(Some("rust,rust,typescript")),
            vec!["rust", "typescript"]
        );
    }

    #[test]
    fn kind_filters_accept_every_stored_symbol_and_edge_kind() {
        // A hand-maintained allowlist once omitted `union`, so exporting C unions failed.
        let every_symbol_kind = SymbolKind::ALL.map(SymbolKind::as_str).join(",");
        let every_edge_kind = EdgeKind::ALL.map(EdgeKind::as_str).join(",");
        assert!(parse_filter(Some(&every_symbol_kind), &node_kind_names(), "--kind").is_ok());
        assert!(parse_filter(Some("union"), &node_kind_names(), "--kind").is_ok());
        assert!(parse_filter(Some(&every_edge_kind), &edge_kind_names(), "--edge-kind").is_ok());
    }
}
