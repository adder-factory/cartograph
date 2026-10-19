//! Current-generation retrieval over typed native graph records.

use std::{cmp::Ordering, collections::BTreeSet};

use super::{
    CurrentEntryPointPage, CurrentEntryPointsLookup, CurrentFileLookup, CurrentFileRecord,
    CurrentFileSymbolsLookup, CurrentFilesLookup, CurrentGenerationRecord, CurrentGraphEdge,
    CurrentGraphLookup, CurrentReferenceRecord, CurrentSourceRangeLookup, CurrentSymbolRecord,
    CurrentSymbolSetLookup, DEFAULT_INTERACTIVE_READ_TIMEOUT, Duration, EntryPointBucket,
    ExactTextLookup, GenerationId, GraphDirection, MAX_EDGE_LIMIT, MAX_ENTRY_POINT_LIMIT,
    MAX_FILE_LIST_LIMIT, MAX_LOOKUP_LIMIT, NormalizedPath, ProjectId, StorageError, SymbolId,
    validate_exact_text, validate_limit, validate_symbol_set,
};
use crate::{
    FileInput, NativeDatabase, SymbolInput,
    rocks::{GraphLink, GraphSnapshot, ReadBudget, RocksError},
};

type NativeResult<T> = Result<T, RocksError>;

impl NativeDatabase {
    /// Observe the complete current generation of this project.
    /// # Errors
    /// Rejects a different project or a bounded native read failure.
    pub async fn current_generation_record(
        &self,
        project: &ProjectId,
    ) -> Result<Option<CurrentGenerationRecord>, StorageError> {
        self.current_generation_record_bounded(project, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Observe current publication under a finite request deadline.
    /// # Errors
    /// Rejects invalid project identity, deadline, or stored metadata.
    pub async fn current_generation_record_bounded(
        &self,
        project: &ProjectId,
        timeout: Duration,
    ) -> Result<Option<CurrentGenerationRecord>, StorageError> {
        self.require_project(project)?;
        let request = crate::native_database::request_for(timeout)
            .map_err(crate::native_database::storage_error)?;
        self.store
            .state(
                None,
                ReadBudget::new(8, 4096, timeout).map_err(crate::native_database::storage_error)?,
                0,
                0,
                &request,
                |transaction, _| {
                    Ok(transaction
                        .current()?
                        .map(|revision| CurrentGenerationRecord {
                            generation_id: revision.generation_id,
                            sequence: revision.sequence,
                        }))
                },
            )
            .await
            .map_err(crate::native_database::storage_error)
    }

    /// Resolve a canonical path in exactly the observed current generation.
    /// # Errors
    /// Rejects a changed publication or failed bounded read.
    pub async fn exact_current_file_by_path(
        &self,
        request: CurrentFileLookup<'_>,
    ) -> Result<Option<CurrentFileRecord>, StorageError> {
        self.exact_current_file_by_path_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve a canonical path under a finite deadline.
    /// # Errors
    /// Rejects a changed publication, invalid project, or exhausted read budget.
    pub async fn exact_current_file_by_path_bounded(
        &self,
        request: CurrentFileLookup<'_>,
        timeout: Duration,
    ) -> Result<Option<CurrentFileRecord>, StorageError> {
        self.require_project(request.project_id)?;
        let generation = request.expected_generation_id.clone();
        let path = request.path.clone();
        self.graph(
            request.expected_generation_id,
            timeout,
            move |graph, budget| {
                let mut found = None;
                visit_files(graph, budget, |file, _| {
                    if file.normalized_path == path.as_str() {
                        found = Some(file_record(&generation, file)?);
                    }
                    Ok(())
                })?;
                Ok(found)
            },
        )
        .await
    }

    /// Return a deterministic bounded file inventory.
    /// # Errors
    /// Rejects invalid bounds, project identity, or a changed publication.
    pub async fn current_files(
        &self,
        request: CurrentFilesLookup<'_>,
    ) -> Result<Vec<CurrentFileRecord>, StorageError> {
        self.current_files_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Return a path-ordered file inventory under a finite deadline.
    /// # Errors
    /// Rejects invalid bounds, changed generation, or exhausted native reads.
    pub async fn current_files_bounded(
        &self,
        request: CurrentFilesLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentFileRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, MAX_FILE_LIST_LIMIT)?;
        let generation = request.expected_generation_id.clone();
        let directory = request.directory.cloned();
        let language = request.language;
        let limit = usize::from(request.limit);
        self.graph(
            request.expected_generation_id,
            timeout,
            move |graph, budget| {
                let mut rows = Vec::new();
                visit_files(graph, budget, |file, _| {
                    if directory.as_ref().is_none_or(|directory| {
                        in_directory(&file.normalized_path, directory.as_str())
                    }) && language.is_none_or(|language| language.as_str() == file.language)
                    {
                        retain(&mut rows, file_record(&generation, file)?, limit, |a, b| {
                            a.path.cmp(&b.path).then(a.file_id.cmp(&b.file_id))
                        });
                    }
                    Ok(())
                })?;
                Ok(rows)
            },
        )
        .await
    }

    /// Resolve exact qualified or simple declaration names.
    /// # Errors
    /// Rejects invalid text, invalid limits, or a changed publication.
    pub async fn exact_current_symbols_by_name(
        &self,
        request: ExactTextLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.exact_current_symbols_by_name_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve exact names under a finite read deadline.
    /// # Errors
    /// Rejects invalid text, limits, project identity, or native read failure.
    pub async fn exact_current_symbols_by_name_bounded(
        &self,
        request: ExactTextLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, MAX_LOOKUP_LIMIT)?;
        let text = validate_exact_text(request.text, "name")?.to_owned();
        self.select_symbols(
            request.expected_generation_id,
            request.limit,
            timeout,
            Some(text.clone()),
            move |symbol, _, _| {
                Ok(symbol.qualified_name == text || simple_name(&symbol.qualified_name) == text)
            },
        )
        .await
    }

    /// Resolve declarations owned by one indexed file.
    /// # Errors
    /// Rejects invalid bounds, project identity, or a changed publication.
    pub async fn current_symbols_by_file(
        &self,
        request: CurrentFileSymbolsLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.current_symbols_by_file_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve declarations owned by one file under a finite deadline.
    /// # Errors
    /// Rejects invalid bounds or failed native evidence reads.
    pub async fn current_symbols_by_file_bounded(
        &self,
        request: CurrentFileSymbolsLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, MAX_LOOKUP_LIMIT)?;
        let file = request.file_id.clone();
        self.select_symbols(
            request.expected_generation_id,
            request.limit,
            timeout,
            None,
            move |symbol, _, _| Ok(symbol.file_id == file),
        )
        .await
    }

    /// Resolve declarations overlapping a source-line range.
    /// # Errors
    /// Rejects invalid ranges, bounds, or a changed generation.
    pub async fn current_symbols_at_range(
        &self,
        request: CurrentSourceRangeLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.current_symbols_at_range_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve a source range under a finite deadline.
    /// # Errors
    /// Rejects invalid ranges, bounds, or native read failure.
    pub async fn current_symbols_at_range_bounded(
        &self,
        request: CurrentSourceRangeLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, MAX_LOOKUP_LIMIT)?;
        super::validate_source_range(request.start_line, request.end_line)?;
        let (path, start, end) = (request.path.clone(), request.start_line, request.end_line);
        self.select_symbols(
            request.expected_generation_id,
            request.limit,
            timeout,
            None,
            move |symbol, graph, budget| {
                if symbol.start_line > end || symbol.end_line < start {
                    return Ok(false);
                }
                Ok(graph
                    .file(&symbol.file_id, budget)?
                    .is_some_and(|file| file.normalized_path == path.as_str()))
            },
        )
        .await
    }

    /// Hydrate an explicit set of symbol identities from the current graph.
    /// # Errors
    /// Rejects invalid symbol-set bounds or a changed publication.
    pub async fn current_symbols_by_ids(
        &self,
        request: CurrentSymbolSetLookup<'_>,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.current_symbols_by_ids_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Hydrate an explicit symbol set under a finite deadline.
    /// # Errors
    /// Rejects invalid project, symbol-set bounds, or exhausted native reads.
    pub async fn current_symbols_by_ids_bounded(
        &self,
        request: CurrentSymbolSetLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_symbol_set(request.symbol_ids)?;
        let ids = request.symbol_ids.iter().cloned().collect::<BTreeSet<_>>();
        let generation = request.expected_generation_id.clone();
        self.graph(
            request.expected_generation_id,
            timeout,
            move |graph, budget| {
                let mut rows = Vec::new();
                for id in ids {
                    if let Some(symbol) = graph.symbol(&id, budget)? {
                        rows.push(symbol_record(&generation, symbol, graph, budget)?);
                    }
                }
                rows.sort_by(symbol_order);
                Ok(rows)
            },
        )
        .await
    }

    async fn select_symbols<F>(
        &self,
        generation: &GenerationId,
        limit: u16,
        timeout: Duration,
        name: Option<String>,
        predicate: F,
    ) -> Result<Vec<CurrentSymbolRecord>, StorageError>
    where
        F: Fn(&SymbolInput, &GraphSnapshot<'_, '_>, &mut ReadBudget) -> NativeResult<bool>
            + Send
            + 'static,
    {
        let expected = generation.clone();
        self.graph(generation, timeout, move |graph, budget| {
            let mut rows = Vec::new();
            visit_symbols_named(graph, budget, name.as_deref(), |symbol, budget| {
                if predicate(&symbol, graph, budget)? {
                    retain(
                        &mut rows,
                        symbol_record(&expected, symbol, graph, budget)?,
                        usize::from(limit),
                        symbol_order,
                    );
                }
                Ok(())
            })?;
            Ok(rows)
        })
        .await
    }

    /// Resolve retained exact reference text, including unresolved sites.
    /// # Errors
    /// Rejects invalid text, limits, or a changed current generation.
    pub async fn exact_current_references_by_name(
        &self,
        request: ExactTextLookup<'_>,
    ) -> Result<Vec<CurrentReferenceRecord>, StorageError> {
        self.exact_current_references_by_name_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Resolve exact reference text under a finite deadline.
    /// # Errors
    /// Rejects invalid input, changed generation, or exhausted native reads.
    pub async fn exact_current_references_by_name_bounded(
        &self,
        request: ExactTextLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentReferenceRecord>, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, MAX_LOOKUP_LIMIT)?;
        let text = validate_exact_text(request.text, "reference_name")?.to_owned();
        let generation = request.expected_generation_id.clone();
        let limit = usize::from(request.limit);
        self.graph(
            request.expected_generation_id,
            timeout,
            move |graph, budget| {
                let mut cursor = None;
                let mut rows = Vec::new();
                let mut ordinal = 0_u64;
                loop {
                    let (references, next) =
                        graph.reference_records(cursor.as_ref(), 256, budget)?;
                    for reference in references {
                        ordinal = ordinal.checked_add(1).ok_or(RocksError::Format)?;
                        if reference.reference_name != text {
                            continue;
                        }
                        let file = graph
                            .file(&reference.file_id, budget)?
                            .ok_or(RocksError::Format)?;
                        let row = CurrentReferenceRecord {
                            reference_id: ordinal,
                            generation_id: generation.clone(),
                            file_id: reference.file_id,
                            path: NormalizedPath::parse(&file.normalized_path)
                                .map_err(|_| RocksError::Format)?,
                            owner_symbol_id: reference.owner_symbol_id,
                            target_symbol_id: reference.target_symbol_id,
                            reference_name: reference.reference_name,
                            reference_kind: reference.reference_kind,
                            start_byte: reference.start_byte,
                            end_byte: reference.end_byte,
                            confidence: reference.confidence,
                            provenance: reference.resolution_provenance,
                            site_count: reference.site_count,
                            span_precision: reference.span_precision,
                        };
                        retain(&mut rows, row, limit, |a, b| {
                            a.path
                                .cmp(&b.path)
                                .then(a.start_byte.cmp(&b.start_byte))
                                .then(a.end_byte.cmp(&b.end_byte))
                                .then(a.provenance.cmp(&b.provenance))
                        });
                    }
                    cursor = next;
                    if cursor.is_none() {
                        break;
                    }
                }
                Ok(rows)
            },
        )
        .await
    }

    /// Discover a bounded category of source entry points with exact cardinality.
    /// # Errors
    /// Rejects invalid project, result bounds, or a changed publication.
    pub async fn current_entry_points(
        &self,
        request: CurrentEntryPointsLookup<'_>,
    ) -> Result<CurrentEntryPointPage, StorageError> {
        self.current_entry_points_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Discover entry points under a finite native read budget.
    /// # Errors
    /// Rejects invalid limits, changed generation, or exhausted native reads.
    pub async fn current_entry_points_bounded(
        &self,
        request: CurrentEntryPointsLookup<'_>,
        timeout: Duration,
    ) -> Result<CurrentEntryPointPage, StorageError> {
        self.require_project(request.project_id)?;
        validate_limit(request.limit, MAX_ENTRY_POINT_LIMIT)?;
        let generation = request.expected_generation_id.clone();
        let (bucket, limit) = (request.bucket, usize::from(request.limit));
        self.graph(
            request.expected_generation_id,
            timeout,
            move |graph, budget| {
                let mcp = if bucket == EntryPointBucket::McpTools {
                    mcp_documents(graph, budget)?
                } else {
                    BTreeSet::new()
                };
                let mut symbols = Vec::new();
                let mut total = 0_u64;
                visit_symbols(graph, budget, |symbol, budget| {
                    let file = graph
                        .file(&symbol.file_id, budget)?
                        .ok_or(RocksError::Format)?;
                    if test_path(&file.normalized_path) {
                        return Ok(());
                    }
                    if entry_point_matches(graph, budget, bucket, &symbol, &file, &mcp)? {
                        total = total.checked_add(1).ok_or(RocksError::Format)?;
                        retain(
                            &mut symbols,
                            symbol_record(&generation, symbol, graph, budget)?,
                            limit,
                            symbol_order,
                        );
                    }
                    Ok(())
                })?;
                Ok(CurrentEntryPointPage {
                    bucket,
                    total,
                    truncated: total > symbols.len() as u64,
                    symbols,
                })
            },
        )
        .await
    }

    /// Expand a frontier through packed adjacency and retain a fair bounded page.
    /// # Errors
    /// Rejects invalid frontier/limits or changed current publication.
    pub async fn current_graph_edges(
        &self,
        request: CurrentGraphLookup<'_>,
    ) -> Result<Vec<CurrentGraphEdge>, StorageError> {
        self.current_graph_edges_bounded(request, DEFAULT_INTERACTIVE_READ_TIMEOUT)
            .await
    }

    /// Expand a frontier under a finite deadline, interleaving roots by rank.
    /// # Errors
    /// Rejects invalid input or exhausted native reads.
    pub async fn current_graph_edges_bounded(
        &self,
        request: CurrentGraphLookup<'_>,
        timeout: Duration,
    ) -> Result<Vec<CurrentGraphEdge>, StorageError> {
        self.require_project(request.project_id)?;
        validate_symbol_set(request.frontier)?;
        validate_limit(request.limit, MAX_EDGE_LIMIT)?;
        let frontier = request.frontier.iter().cloned().collect::<BTreeSet<_>>();
        let generation = request.expected_generation_id.clone();
        let (direction, include_tests, limit) = (
            request.direction,
            request.include_test_targets,
            usize::from(request.limit),
        );
        self.graph(
            request.expected_generation_id,
            timeout,
            move |graph, budget| {
                let mut ranked = Vec::new();
                for origin in frontier {
                    let edges = graph_edges_from(
                        graph,
                        budget,
                        &generation,
                        &origin,
                        direction,
                        include_tests,
                        limit,
                    )?;
                    for (rank, edge) in edges.into_iter().enumerate() {
                        retain(&mut ranked, (rank, edge), limit, |a, b| {
                            a.0.cmp(&b.0).then_with(|| edge_order(&a.1, &b.1))
                        });
                    }
                }
                Ok(ranked.into_iter().map(|(_, edge)| edge).collect())
            },
        )
        .await
    }
}

fn entry_point_matches(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    bucket: EntryPointBucket,
    symbol: &SymbolInput,
    file: &FileInput,
    mcp: &BTreeSet<SymbolId>,
) -> NativeResult<bool> {
    let name = simple_name(&symbol.qualified_name);
    let cli = file.normalized_path.split('/').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "bin" | "cli" | "command" | "commands"
        )
    });
    let exportable = matches!(
        symbol.symbol_kind.as_str(),
        "function"
            | "method"
            | "class"
            | "struct"
            | "interface"
            | "trait"
            | "enum"
            | "type_alias"
            | "constant"
            | "variable"
            | "module"
            | "namespace"
            | "component"
            | "resource"
    );
    Ok(match bucket {
        EntryPointBucket::Routes => {
            symbol.symbol_kind == "route" && !name.to_ascii_lowercase().starts_with("cmd ")
        }
        EntryPointBucket::CliCommands => {
            symbol.symbol_kind == "route" && name.to_ascii_lowercase().starts_with("cmd ")
        }
        EntryPointBucket::McpTools => {
            symbol.export.exported
                && symbol.symbol_kind == "constant"
                && name.ends_with("_TOOL")
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
                && mcp.contains(&symbol.symbol_id)
        }
        EntryPointBucket::CliFiles => symbol.export.exported && exportable && cli,
        EntryPointBucket::PublicExports => {
            symbol.export.exported
                && exportable
                && !cli
                && !has_consumer(graph, &symbol.symbol_id, budget)?
        }
    })
}

fn graph_edges_from(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    generation: &GenerationId,
    origin: &SymbolId,
    direction: GraphDirection,
    include_tests: bool,
    limit: usize,
) -> NativeResult<Vec<CurrentGraphEdge>> {
    let mut cursor = None;
    let mut edges = Vec::new();
    loop {
        let page = graph.neighbors(origin, direction, cursor.as_ref(), 256, budget)?;
        for link in page.links {
            if !include_tests && test_target(graph, budget, &link, direction)? {
                continue;
            }
            let edge = graph.edge(&link, budget)?;
            retain(
                &mut edges,
                CurrentGraphEdge {
                    generation_id: generation.clone(),
                    source_symbol_id: edge.source_symbol_id,
                    target_symbol_id: edge.target_symbol_id,
                    edge_kind: edge.kind.as_str().to_owned(),
                    confidence: edge.confidence,
                    provenance: edge.provenance,
                    site_count: edge.site_count,
                },
                limit,
                edge_order,
            );
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(edges);
        }
    }
}

fn test_target(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    link: &GraphLink,
    direction: GraphDirection,
) -> NativeResult<bool> {
    let adjacent = match direction {
        GraphDirection::Outgoing => link.target_symbol_id(),
        GraphDirection::Incoming => link.source_symbol_id(),
    };
    let Some(symbol) = graph.symbol(adjacent, budget)? else {
        return Ok(false);
    };
    Ok(graph
        .file(&symbol.file_id, budget)?
        .is_some_and(|file| test_path(&file.normalized_path)))
}

fn edge_order(a: &CurrentGraphEdge, b: &CurrentGraphEdge) -> Ordering {
    b.confidence
        .total_cmp(&a.confidence)
        .then(b.site_count.cmp(&a.site_count))
        .then(a.edge_kind.cmp(&b.edge_kind))
        .then(a.provenance.cmp(&b.provenance))
        .then(a.source_symbol_id.cmp(&b.source_symbol_id))
        .then(a.target_symbol_id.cmp(&b.target_symbol_id))
}

pub(crate) fn retain<T>(
    rows: &mut Vec<T>,
    row: T,
    limit: usize,
    order: impl Fn(&T, &T) -> Ordering,
) {
    let position = rows.partition_point(|existing| order(existing, &row) != Ordering::Greater);
    if position < limit {
        rows.insert(position, row);
        rows.truncate(limit);
    }
}

fn symbol_order(a: &CurrentSymbolRecord, b: &CurrentSymbolRecord) -> Ordering {
    a.path
        .cmp(&b.path)
        .then(a.start_line.cmp(&b.start_line))
        .then(a.symbol_id.cmp(&b.symbol_id))
}

pub(crate) fn visit_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(FileInput, &mut ReadBudget) -> NativeResult<()>,
) -> NativeResult<()> {
    let mut cursor = None;
    loop {
        let (rows, next) = graph.files(cursor.as_ref(), 256, budget)?;
        for row in rows {
            visit(row, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

pub(crate) fn visit_symbols_named(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    name: Option<&str>,
    mut visit: impl FnMut(SymbolInput, &mut ReadBudget) -> NativeResult<()>,
) -> NativeResult<()> {
    let Some(name) = name.filter(|name| !name.is_empty() && graph.info().name_index_version == 1)
    else {
        return visit_symbols(graph, budget, visit);
    };
    let mut cursor = None;
    loop {
        let (rows, next) = graph.symbols_by_name(name, cursor.as_ref(), 256, budget)?;
        for row in rows {
            visit(row, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

pub(crate) fn visit_symbols(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(SymbolInput, &mut ReadBudget) -> NativeResult<()>,
) -> NativeResult<()> {
    let mut cursor = None;
    loop {
        let (rows, next) = graph.symbols(cursor.as_ref(), 256, budget)?;
        for row in rows {
            visit(row, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

fn file_record(generation: &GenerationId, file: FileInput) -> NativeResult<CurrentFileRecord> {
    Ok(CurrentFileRecord {
        generation_id: generation.clone(),
        file_id: file.file_id,
        path: NormalizedPath::parse(&file.normalized_path).map_err(|_| RocksError::Format)?,
        language: file.language,
        content_hash: file.content_hash,
    })
}

pub(crate) fn symbol_record(
    generation: &GenerationId,
    symbol: SymbolInput,
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> NativeResult<CurrentSymbolRecord> {
    let file = graph
        .file(&symbol.file_id, budget)?
        .ok_or(RocksError::Format)?;
    Ok(CurrentSymbolRecord {
        generation_id: generation.clone(),
        symbol_id: symbol.symbol_id,
        file_id: symbol.file_id,
        path: NormalizedPath::parse(&file.normalized_path).map_err(|_| RocksError::Format)?,
        language: file.language,
        symbol_kind: symbol.symbol_kind,
        qualified_name: symbol.qualified_name,
        signature: symbol.signature,
        start_line: symbol.start_line,
        end_line: symbol.end_line,
        visibility: symbol.visibility,
        export: symbol.export,
        execution: symbol.execution,
        declaration_only: symbol.declaration_only,
    })
}

pub(crate) fn simple_name(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).next().unwrap_or(name)
}

pub(crate) fn in_directory(path: &str, directory: &str) -> bool {
    path == directory
        || path
            .strip_prefix(directory)
            .is_some_and(|tail| tail.starts_with('/'))
}

pub(crate) fn test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.split('/').any(|part| {
        matches!(
            part,
            "__tests__"
                | "__mocks__"
                | "test"
                | "tests"
                | "spec"
                | "specs"
                | "integration"
                | "testing"
                | "testlib"
                | "fixture"
                | "fixtures"
                | "test-bed"
                | "test-beds"
        )
    }) || [".test.", ".spec.", "_test.", "_spec."]
        .iter()
        .any(|part| lower.contains(part))
        || lower
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("test_") || name.starts_with("test-"))
        || path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(stem, _)| {
                ["Test", "Tests", "TestCase", "Spec"].iter().any(|suffix| {
                    stem.strip_suffix(suffix).is_some_and(|prefix| {
                        prefix
                            .as_bytes()
                            .last()
                            .is_some_and(u8::is_ascii_alphabetic)
                    })
                })
            })
}

fn has_consumer(
    graph: &GraphSnapshot<'_, '_>,
    symbol: &SymbolId,
    budget: &mut ReadBudget,
) -> NativeResult<bool> {
    let mut cursor = None;
    loop {
        let page = graph.neighbors(
            symbol,
            GraphDirection::Incoming,
            cursor.as_ref(),
            256,
            budget,
        )?;
        if page.links.iter().any(|link| {
            matches!(
                link.kind(),
                cartograph_domain::EdgeKind::Calls
                    | cartograph_domain::EdgeKind::References
                    | cartograph_domain::EdgeKind::Instantiates
                    | cartograph_domain::EdgeKind::TypeOf
                    | cartograph_domain::EdgeKind::Returns
                    | cartograph_domain::EdgeKind::Extends
                    | cartograph_domain::EdgeKind::Implements
            )
        }) {
            return Ok(true);
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(false);
        }
    }
}

fn mcp_documents(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> NativeResult<BTreeSet<SymbolId>> {
    let mut found = BTreeSet::new();
    let mut cursor = None;
    loop {
        let (rows, next) = graph.documents(cursor.as_ref(), 256, budget)?;
        for document in rows {
            if document.code().to_ascii_lowercase().contains("cartograph_")
                && let Some(id) = document.symbol_id()
            {
                found.insert(id.clone());
            }
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(found);
        }
    }
}
