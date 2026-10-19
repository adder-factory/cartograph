use std::collections::{BTreeSet, VecDeque};

use cartograph_db::{
    EdgeInput, FileInput, GraphDirection, SymbolInput,
    rocks::{
        GraphLink, GraphSnapshot, ReadBudget, RecordKey, RevisionSnapshot, RocksError,
        StorageRequest,
    },
};
use cartograph_domain::{EdgeKind, NormalizedPath, SymbolId};
use serde::Serialize;
use serde_json::{Value, json};

use super::{RocksProjectError, RocksProjectRuntime};

/// Bounded native query, independent of CLI or MCP JSON syntax.
#[derive(Clone)]
pub enum RocksQuery {
    /// Exact simple or qualified declaration name.
    Name {
        /// Exact name.
        query: String,
        /// Maximum rows.
        limit: u16,
    },
    /// Exact source file and its declarations.
    Path {
        /// Project-relative source path.
        path: String,
        /// Maximum declarations.
        limit: u16,
    },
    /// Exact resolved or unresolved reference text.
    Reference {
        /// Exact reference text.
        query: String,
        /// Maximum rows.
        limit: u16,
    },
    /// Deterministic native BM25 over canonical document fields.
    Lexical {
        /// Bounded query terms.
        query: String,
        /// Maximum ranked hits.
        limit: u16,
    },
    /// Source-file inventory.
    Files {
        /// Optional project-relative subtree.
        directory: Option<String>,
        /// Maximum files.
        limit: u16,
    },
    /// Directed traversal of compact topology, with optional edge filtering.
    Graph {
        /// Exact root symbol.
        symbol: SymbolId,
        /// Traversal orientation.
        direction: GraphDirection,
        /// Optional relationship kind.
        kind: Option<EdgeKind>,
        /// Maximum depth.
        depth: u8,
        /// Maximum non-root nodes.
        maximum_nodes: u16,
    },
}

/// Generation-fenced query evidence with explicit live-source freshness.
#[derive(Clone, Debug, Serialize)]
pub struct RocksQueryResult {
    /// Native storage selection.
    pub storage: &'static str,
    /// Complete pinned generation.
    pub generation_id: cartograph_domain::GenerationId,
    /// Source and policy identity of that generation.
    pub source_revision: cartograph_domain::ContentDigest,
    /// Source was checked before and after the query.
    pub fresh: bool,
    /// Bounded operation-specific evidence.
    #[serde(flatten)]
    pub evidence: Value,
}

impl RocksProjectRuntime {
    /// Query one immutable revision and reject live drift unless explicitly allowed.
    ///
    /// # Errors
    /// Returns stale, malformed, missing-entity, storage, or finite-budget errors.
    pub(super) async fn query_inner(
        &self,
        query: RocksQuery,
        allow_stale: bool,
        request: &StorageRequest,
    ) -> Result<RocksQueryResult, RocksProjectError> {
        query.validate()?;
        let status = self.status_inner(request).await?;
        if !allow_stale && !status.fresh {
            return Err(RocksProjectError::SourceChanged);
        }
        let current = status.current.ok_or(RocksProjectError::NotIndexed)?;
        let result = self
            .store
            .read(
                Some(current.generation_id),
                ReadBudget::new(
                    2_000_000,
                    192 * 1_048_576,
                    std::time::Duration::from_secs(120),
                )?,
                request,
                move |snapshot, budget| {
                    let evidence = execute(snapshot, budget, query)?;
                    if serde_json::to_vec(&evidence).map_err(|_| invalid())?.len() > 2 * 1_048_576 {
                        return Err(RocksError::Limit {
                            resource: "query_output_bytes",
                        });
                    }
                    Ok(RocksQueryResult {
                        storage: "rocksdb",
                        generation_id: snapshot.revision().generation_id.clone(),
                        source_revision: snapshot.revision().source_revision.clone(),
                        fresh: status.fresh,
                        evidence,
                    })
                },
            )
            .await?;
        let (after, _) = self.scan(request, false).await?;
        if after.digest != status.live_source_revision {
            return Err(RocksProjectError::SourceChanged);
        }
        Ok(result)
    }
}

impl RocksQuery {
    fn validate(&self) -> Result<(), RocksProjectError> {
        let valid = match self {
            Self::Name { query, limit }
            | Self::Reference { query, limit }
            | Self::Lexical { query, limit } => {
                !query.is_empty() && query.len() <= 4096 && (1..=100).contains(limit)
            }
            Self::Path { path, limit } => {
                NormalizedPath::parse(path).is_ok() && (1..=200).contains(limit)
            }
            Self::Files { directory, limit } => {
                directory
                    .as_ref()
                    .is_none_or(|path| NormalizedPath::parse(path).is_ok())
                    && (1..=500).contains(limit)
            }
            Self::Graph {
                depth,
                maximum_nodes,
                ..
            } => (1..=8).contains(depth) && (1..=500).contains(maximum_nodes),
        };
        if valid {
            Ok(())
        } else {
            Err(RocksProjectError::InvalidQuery)
        }
    }
}

fn invalid() -> RocksError {
    RocksError::Corrupt {
        field: "native_query",
    }
}

fn execute(
    snapshot: &RevisionSnapshot<'_>,
    budget: &mut ReadBudget,
    query: RocksQuery,
) -> Result<Value, RocksError> {
    let graph = snapshot.graph(budget)?;
    match query {
        RocksQuery::Name { query, limit } => symbols(&graph, budget, None, Some(&query), limit),
        RocksQuery::Path { path, limit } => symbols(&graph, budget, Some(&path), None, limit),
        RocksQuery::Files { directory, limit } => {
            files(&graph, budget, directory.as_deref(), limit)
        }
        RocksQuery::Reference { query, limit } => references(&graph, budget, &query, limit),
        RocksQuery::Lexical { query, limit } => lexical(&graph, budget, &query, limit),
        RocksQuery::Graph {
            symbol,
            direction,
            kind,
            depth,
            maximum_nodes,
        } => traversal(
            &graph,
            budget,
            &Traversal {
                symbol,
                direction,
                kind,
                depth,
                maximum_nodes,
            },
        ),
    }
}

fn named(symbol: &SymbolInput, query: &str) -> bool {
    symbol.qualified_name == query
        || symbol.qualified_name.rsplit([':', '.', '#', '/']).next() == Some(query)
}

fn symbols(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    path: Option<&str>,
    name: Option<&str>,
    limit: u16,
) -> Result<Value, RocksError> {
    let mut cursor = None;
    let mut hits = Vec::new();
    let matching_file = path
        .map(|path| find_file(graph, budget, path))
        .transpose()?
        .flatten();
    if path.is_some() && matching_file.is_none() {
        return Ok(json!({"results": [], "file": null, "truncated": false}));
    }
    let mut truncated = false;
    'pages: loop {
        let (rows, next) = symbol_candidates(graph, budget, cursor.as_ref(), name)?;
        for symbol in rows {
            if !matches_symbol(&symbol, name, matching_file.as_ref()) {
                continue;
            }
            if hits.len() == usize::from(limit) {
                truncated = true;
                break 'pages;
            }
            let file = graph.file(&symbol.file_id, budget)?.ok_or_else(invalid)?;
            hits.push(json!({"symbol_id": symbol.symbol_id, "qualified_name": symbol.qualified_name, "symbol_kind": symbol.symbol_kind, "path": file.normalized_path, "language": file.language, "start_line": symbol.start_line, "end_line": symbol.end_line, "signature": symbol.signature}));
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(json!({"results": hits, "file": matching_file, "truncated": truncated, "channel": "exact"}))
}

fn symbol_candidates(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    cursor: Option<&RecordKey>,
    name: Option<&str>,
) -> Result<(Vec<SymbolInput>, Option<RecordKey>), RocksError> {
    if let Some(name) = name.filter(|_| graph.info().name_index_version == 1) {
        graph.symbols_by_name(name, cursor, 256, budget)
    } else {
        graph.symbols(cursor, 256, budget)
    }
}

fn find_file(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    path: &str,
) -> Result<Option<FileInput>, RocksError> {
    let mut cursor = None;
    loop {
        let (rows, next) = graph.files(cursor.as_ref(), 256, budget)?;
        if let Some(file) = rows.into_iter().find(|file| file.normalized_path == path) {
            return Ok(Some(file));
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(None);
        }
    }
}

fn matches_symbol(symbol: &SymbolInput, name: Option<&str>, file: Option<&FileInput>) -> bool {
    name.is_none_or(|name| named(symbol, name))
        && file.is_none_or(|file| file.file_id == symbol.file_id)
}

fn files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    directory: Option<&str>,
    limit: u16,
) -> Result<Value, RocksError> {
    let mut cursor = None;
    let mut found = Vec::new();
    let mut truncated = false;
    'pages: loop {
        let (rows, next) = graph.files(cursor.as_ref(), 256, budget)?;
        for file in rows {
            if directory.is_some_and(|dir| {
                file.normalized_path != dir
                    && !file
                        .normalized_path
                        .strip_prefix(dir)
                        .is_some_and(|tail| tail.starts_with('/'))
            }) {
                continue;
            }
            if found.len() == usize::from(limit) {
                truncated = true;
                break 'pages;
            }
            found.push(file);
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    found.sort_unstable_by(|left, right| left.normalized_path.cmp(&right.normalized_path));
    Ok(json!({"files": found, "truncated": truncated}))
}

fn references(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    query: &str,
    limit: u16,
) -> Result<Value, RocksError> {
    let mut cursor = None;
    let mut hits = Vec::new();
    let mut truncated = false;
    'pages: loop {
        let (rows, next) = graph.reference_records(cursor.as_ref(), 256, budget)?;
        for reference in rows {
            if reference.reference_name != query {
                continue;
            }
            if hits.len() == usize::from(limit) {
                truncated = true;
                break 'pages;
            }
            let file = graph
                .file(&reference.file_id, budget)?
                .ok_or_else(invalid)?;
            hits.push(json!({"reference": reference, "path": file.normalized_path}));
        }
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok(json!({"results": hits, "truncated": truncated, "channel": "reference"}))
}

struct Traversal {
    symbol: SymbolId,
    direction: GraphDirection,
    kind: Option<EdgeKind>,
    depth: u8,
    maximum_nodes: u16,
}

fn traversal(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    query: &Traversal,
) -> Result<Value, RocksError> {
    let root = graph
        .symbol(&query.symbol, budget)?
        .ok_or(RocksError::GenerationUnavailable)?;
    let mut walk = TraversalWalk {
        nodes: vec![json!(root)],
        edges: Vec::new(),
        seen: BTreeSet::from([query.symbol.clone()]),
        queue: VecDeque::from([(query.symbol.clone(), 0_u8)]),
    };
    let mut truncated = false;
    while let Some((node, depth)) = walk.queue.pop_front() {
        if depth == query.depth {
            continue;
        }
        if !walk.visit_node(graph, budget, query, &node, depth)? {
            truncated = true;
            break;
        }
    }
    Ok(json!({"root": root, "nodes": walk.nodes, "edges": walk.edges, "truncated": truncated}))
}

struct TraversalWalk {
    nodes: Vec<Value>,
    edges: Vec<EdgeInput>,
    seen: BTreeSet<SymbolId>,
    queue: VecDeque<(SymbolId, u8)>,
}

impl TraversalWalk {
    fn visit_node(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
        query: &Traversal,
        node: &SymbolId,
        depth: u8,
    ) -> Result<bool, RocksError> {
        let mut cursor = None;
        loop {
            let page = graph.neighbors(node, query.direction, cursor.as_ref(), 128, budget)?;
            for link in page.links {
                if query.kind.is_some_and(|kind| link.kind() != kind) {
                    continue;
                }
                if !self.visit_link(graph, budget, query, (node, depth), &link)? {
                    return Ok(false);
                }
            }
            cursor = page.next;
            if cursor.is_none() {
                return Ok(true);
            }
        }
    }

    fn visit_link(
        &mut self,
        graph: &GraphSnapshot<'_, '_>,
        budget: &mut ReadBudget,
        query: &Traversal,
        current: (&SymbolId, u8),
        link: &GraphLink,
    ) -> Result<bool, RocksError> {
        let (node, depth) = current;
        let neighbor = if link.source_symbol_id() == node {
            link.target_symbol_id()
        } else {
            link.source_symbol_id()
        };
        if !self.seen.contains(neighbor) {
            if self.seen.len() > usize::from(query.maximum_nodes) {
                return Ok(false);
            }
            self.seen.insert(neighbor.clone());
            self.nodes
                .push(json!(graph.symbol(neighbor, budget)?.ok_or_else(invalid)?));
            self.queue.push_back((neighbor.clone(), depth + 1));
        }
        if self.edges.len() == 5000 {
            return Ok(false);
        }
        self.edges.push(graph.edge(link, budget)?);
        Ok(true)
    }
}

fn lexical(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    query: &str,
    limit: u16,
) -> Result<Value, RocksError> {
    let indexed = graph
        .lexical_statistics(budget)?
        .is_some_and(|statistics| statistics.scan_documents == 0);
    let (hits, truncated) = graph.search_code(query, limit, budget)?;
    let hits = hits
        .into_iter()
        .map(|hit| {
            json!({
                "document_id": hit.document_id(), "symbol_id": hit.symbol_id(),
                "file_id": hit.file_id(), "qualified_name": hit.qualified_name(),
                "path": hit.path(), "language": hit.language(),
                "kind": hit.document_kind(), "score": hit.score(),
            })
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"results": hits, "truncated": truncated, "channel": "bm25",
        "implementation": if indexed { "native_field_postings_v1" } else { "native_bounded_document_scan_v1" },
        "semantic": "not_available"}),
    )
}
