//! File inventory and dependencies over the current native graph.

use std::collections::BTreeMap;

use super::{
    FILE_QUERY_TIMEOUT, FileAggregateResult, FileDependencyDirection, FileDependencyQuery,
    FileDependencyResult, FileDependencyRow, FileDirectoryAggregate, FileLanguageAggregate,
    FileSurfaceQuery, FileSurfaceResult, FileSurfaceRow, MAX_FILE_ROWS, ProjectId, StorageError,
    validate_limit,
};
use crate::{
    EdgeInput, FileInput, GraphDirection, NativeDatabase,
    retrieval::native::{in_directory, retain, visit_files, visit_symbols},
    rocks::{GraphLink, GraphSnapshot, ReadBudget, RocksError},
};
use cartograph_domain::{FileId, GenerationId, SymbolId};

type FileRows = BTreeMap<FileId, (FileInput, u64)>;

impl NativeDatabase {
    /// Read status inventory totals, one path-ordered file, and all language totals.
    /// The inventory and language rollup share one generation and symbol scan.
    /// # Errors
    /// Rejects project mismatch, a replaced generation, or exhausted native bounds.
    pub async fn current_status_inventory(
        &self,
        project: &ProjectId,
        generation: &GenerationId,
    ) -> Result<(FileSurfaceResult, Vec<FileLanguageAggregate>), StorageError> {
        self.require_project(project)?;
        self.graph(generation, FILE_QUERY_TIMEOUT, |graph, budget| {
            let mut output = FileSurfaceResult {
                files: Vec::new(),
                total_files: 0,
                total_bytes: 0,
                total_symbols: 0,
                truncated: false,
            };
            let mut languages = BTreeMap::new();
            let mut workspace = 0;
            for (_, (file, symbols)) in file_rows(graph, budget)? {
                output.total_files += 1;
                output.total_bytes = output
                    .total_bytes
                    .checked_add(file.byte_size)
                    .ok_or(RocksError::Format)?;
                output.total_symbols = output
                    .total_symbols
                    .checked_add(symbols)
                    .ok_or(RocksError::Format)?;
                add_aggregate(
                    &mut languages,
                    &file.language,
                    symbols,
                    file.byte_size,
                    &mut workspace,
                )?;
                retain(
                    &mut output.files,
                    FileSurfaceRow {
                        path: file.normalized_path,
                        language: file.language,
                        byte_size: file.byte_size,
                        parse_status: file.parse_status.as_str().to_owned(),
                        symbol_count: symbols,
                    },
                    1,
                    |a, b| a.path.cmp(&b.path),
                );
            }
            output.truncated = output.total_files > output.files.len() as u64;
            let mut languages = languages
                .into_iter()
                .map(
                    |(language, (files, symbols, bytes))| FileLanguageAggregate {
                        language,
                        files,
                        symbols,
                        bytes,
                    },
                )
                .collect::<Vec<_>>();
            languages.sort_by(|a, b| b.files.cmp(&a.files).then(a.language.cmp(&b.language)));
            Ok((output, languages))
        })
        .await
    }

    /// Return complete inventory totals and a bounded path-ordered page.
    /// # Errors
    /// Rejects invalid project, filters, limits, or exhausted native reads.
    pub async fn current_file_surface(
        &self,
        project: &ProjectId,
        request: &FileSurfaceQuery,
    ) -> Result<FileSurfaceResult, StorageError> {
        self.require_project(project)?;
        validate_limit(request.limit, MAX_FILE_ROWS, "file_surface_limit")?;
        let regex = path_regex(request)?;
        let request = request.clone();
        let Some(generation) = self.current_generation_record(project).await? else {
            return Ok(FileSurfaceResult {
                files: Vec::new(),
                total_files: 0,
                total_bytes: 0,
                total_symbols: 0,
                truncated: false,
            });
        };
        self.graph(
            generation.generation_id(),
            FILE_QUERY_TIMEOUT,
            move |graph, budget| {
                let mut output = FileSurfaceResult {
                    files: Vec::new(),
                    total_files: 0,
                    total_bytes: 0,
                    total_symbols: 0,
                    truncated: false,
                };
                for (_, (file, symbols)) in file_rows(graph, budget)? {
                    if !matches_file(&file, &request, regex.as_ref()) {
                        continue;
                    }
                    output.total_files += 1;
                    output.total_bytes = output
                        .total_bytes
                        .checked_add(file.byte_size)
                        .ok_or(RocksError::Format)?;
                    output.total_symbols = output
                        .total_symbols
                        .checked_add(symbols)
                        .ok_or(RocksError::Format)?;
                    retain(
                        &mut output.files,
                        FileSurfaceRow {
                            path: file.normalized_path,
                            language: file.language,
                            byte_size: file.byte_size,
                            parse_status: file.parse_status.as_str().to_owned(),
                            symbol_count: symbols,
                        },
                        usize::from(request.limit),
                        |a, b| a.path.cmp(&b.path),
                    );
                }
                output.truncated = output.total_files > output.files.len() as u64;
                Ok(output)
            },
        )
        .await
    }

    /// Aggregate filtered files before applying the directory response bound.
    /// # Errors
    /// Rejects invalid filters, limits, project identity, or exhausted reads.
    pub async fn current_file_aggregates(
        &self,
        project: &ProjectId,
        request: &FileSurfaceQuery,
        directory_limit: u16,
    ) -> Result<FileAggregateResult, StorageError> {
        self.require_project(project)?;
        validate_limit(directory_limit, MAX_FILE_ROWS, "file_aggregate_limit")?;
        let regex = path_regex(request)?;
        let request = request.clone();
        let Some(generation) = self.current_generation_record(project).await? else {
            return Ok(FileAggregateResult {
                languages: Vec::new(),
                directories: Vec::new(),
                total_directories: 0,
                directories_truncated: false,
            });
        };
        self.graph(
            generation.generation_id(),
            FILE_QUERY_TIMEOUT,
            move |graph, budget| {
                let mut languages = BTreeMap::<String, (u64, u64, u64)>::new();
                let mut directories = BTreeMap::<String, (u64, u64, u64)>::new();
                let mut workspace = 0_usize;
                for (_, (file, symbols)) in file_rows(graph, budget)? {
                    if !matches_file(&file, &request, regex.as_ref()) {
                        continue;
                    }
                    add_aggregate(
                        &mut languages,
                        &file.language,
                        symbols,
                        file.byte_size,
                        &mut workspace,
                    )?;
                    if !file.normalized_path.contains('/') {
                        add_aggregate(
                            &mut directories,
                            ".",
                            symbols,
                            file.byte_size,
                            &mut workspace,
                        )?;
                    }
                    let mut prefix = file.normalized_path.as_str();
                    while let Some((parent, _)) = prefix.rsplit_once('/') {
                        add_aggregate(
                            &mut directories,
                            parent,
                            symbols,
                            file.byte_size,
                            &mut workspace,
                        )?;
                        prefix = parent;
                    }
                }
                let mut languages = languages
                    .into_iter()
                    .map(
                        |(language, (files, symbols, bytes))| FileLanguageAggregate {
                            language,
                            files,
                            symbols,
                            bytes,
                        },
                    )
                    .collect::<Vec<_>>();
                languages.sort_by(|a, b| b.files.cmp(&a.files).then(a.language.cmp(&b.language)));
                let total_directories = directories.len() as u64;
                let mut directories = directories
                    .into_iter()
                    .map(|(path, (files, symbols, bytes))| FileDirectoryAggregate {
                        path,
                        files,
                        symbols,
                        bytes,
                    })
                    .collect::<Vec<_>>();
                directories.sort_by(|a, b| a.path.cmp(&b.path));
                directories.truncate(usize::from(directory_limit));
                Ok(FileAggregateResult {
                    languages,
                    directories,
                    total_directories,
                    directories_truncated: total_directories > u64::from(directory_limit),
                })
            },
        )
        .await
    }

    /// Aggregate cross-file relationships with independent direction limits.
    /// # Errors
    /// Rejects invalid project, changed generation, or exhausted native reads.
    pub async fn current_file_dependencies(
        &self,
        project: &ProjectId,
        request: &FileDependencyQuery,
    ) -> Result<FileDependencyResult, StorageError> {
        self.require_project(project)?;
        let request = request.clone();
        let Some(generation) = self.current_generation_record(project).await? else {
            return Ok(dependency_result(Vec::new(), 0));
        };
        self.graph(
            generation.generation_id(),
            FILE_QUERY_TIMEOUT,
            move |graph, budget| {
                let (files, anchor) = dependency_files(graph, budget, request.path.as_str())?;
                let Some(anchor) = anchor else {
                    return Ok(dependency_result(Vec::new(), request.per_direction_limit));
                };
                let symbols = file_symbols(graph, budget, &anchor)?;
                let mut grouped = BTreeMap::<(bool, String), FileDependencyRow>::new();
                for incoming in [false, true] {
                    if (incoming && request.direction == FileDependencyDirection::Dependencies)
                        || (!incoming && request.direction == FileDependencyDirection::Dependents)
                    {
                        continue;
                    }
                    for symbol in &symbols {
                        adjacent_files(
                            graph,
                            budget,
                            symbol,
                            incoming,
                            &anchor,
                            &files,
                            &mut grouped,
                        )?;
                    }
                }
                Ok(dependency_result(
                    grouped.into_values().collect(),
                    request.per_direction_limit,
                ))
            },
        )
        .await
    }
}

fn dependency_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    path: &str,
) -> Result<(BTreeMap<FileId, FileInput>, Option<FileId>), RocksError> {
    let mut files = BTreeMap::new();
    let mut workspace = 0_usize;
    let mut anchor = None;
    visit_files(graph, budget, |file, _| {
        workspace =
            workspace.saturating_add(file.normalized_path.len() + file.language.len() + 256);
        if workspace > 32 * 1_048_576 {
            return Err(RocksError::Limit {
                resource: "file_workspace_bytes",
            });
        }
        if file.normalized_path == path {
            anchor = Some(file.file_id.clone());
        }
        files.insert(file.file_id.clone(), file);
        Ok(())
    })?;
    Ok((files, anchor))
}

fn file_symbols(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    anchor: &FileId,
) -> Result<Vec<SymbolId>, RocksError> {
    let mut symbols = Vec::new();
    visit_symbols(graph, budget, |symbol, _| {
        if &symbol.file_id != anchor {
            return Ok(());
        }
        if symbols.len() >= 250_000 {
            return Err(RocksError::Limit {
                resource: "file_dependency_symbols",
            });
        }
        symbols.push(symbol.symbol_id);
        Ok(())
    })?;
    Ok(symbols)
}

fn file_rows(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<FileRows, RocksError> {
    let mut files = BTreeMap::new();
    let mut workspace = 0_usize;
    visit_files(graph, budget, |file, _| {
        workspace =
            workspace.saturating_add(file.normalized_path.len() + file.language.len() + 256);
        if workspace > 32 * 1_048_576 {
            return Err(RocksError::Limit {
                resource: "file_workspace_bytes",
            });
        }
        files.insert(file.file_id.clone(), (file, 0_u64));
        Ok(())
    })?;
    visit_symbols(graph, budget, |symbol, _| {
        if symbol.symbol_kind != "file" {
            let (_, count) = files.get_mut(&symbol.file_id).ok_or(RocksError::Format)?;
            *count = count.checked_add(1).ok_or(RocksError::Format)?;
        }
        Ok(())
    })?;
    Ok(files)
}

fn path_regex(request: &FileSurfaceQuery) -> Result<Option<regex::Regex>, StorageError> {
    request
        .path_regex
        .as_ref()
        .map(|pattern| {
            regex::RegexBuilder::new(pattern)
                .size_limit(1_048_576)
                .dfa_size_limit(1_048_576)
                .build()
                .map_err(|_| StorageError::InvalidInput {
                    field: "file_path_regex",
                })
        })
        .transpose()
}

fn matches_file(
    file: &FileInput,
    request: &FileSurfaceQuery,
    regex: Option<&regex::Regex>,
) -> bool {
    request
        .directory
        .as_ref()
        .is_none_or(|directory| in_directory(&file.normalized_path, directory.as_str()))
        && request
            .language
            .is_none_or(|language| file.language == language.as_str())
        && regex.is_none_or(|regex| regex.is_match(&file.normalized_path))
}

fn add_aggregate(
    map: &mut BTreeMap<String, (u64, u64, u64)>,
    key: &str,
    symbols: u64,
    bytes: u64,
    workspace: &mut usize,
) -> Result<(), RocksError> {
    if !map.contains_key(key) {
        *workspace = workspace.saturating_add(key.len() + 128);
    }
    if *workspace > 32 * 1_048_576 {
        return Err(RocksError::Limit {
            resource: "file_workspace_bytes",
        });
    }
    let value = map.entry(key.to_owned()).or_default();
    value.0 = value.0.checked_add(1).ok_or(RocksError::Format)?;
    value.1 = value.1.checked_add(symbols).ok_or(RocksError::Format)?;
    value.2 = value.2.checked_add(bytes).ok_or(RocksError::Format)?;
    Ok(())
}

fn adjacent_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    symbol: &SymbolId,
    incoming: bool,
    anchor: &FileId,
    files: &BTreeMap<FileId, FileInput>,
    grouped: &mut BTreeMap<(bool, String), FileDependencyRow>,
) -> Result<(), RocksError> {
    let direction = if incoming {
        GraphDirection::Incoming
    } else {
        GraphDirection::Outgoing
    };
    let mut cursor = None;
    loop {
        let page = graph.neighbors(symbol, direction, cursor.as_ref(), 256, budget)?;
        for link in page.links {
            record_adjacent_dependency(graph, budget, &link, incoming, anchor, files, grouped)?;
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

fn record_adjacent_dependency(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    link: &GraphLink,
    incoming: bool,
    anchor: &FileId,
    files: &BTreeMap<FileId, FileInput>,
    grouped: &mut BTreeMap<(bool, String), FileDependencyRow>,
) -> Result<(), RocksError> {
    if link.kind() == cartograph_domain::EdgeKind::Contains {
        return Ok(());
    }
    let adjacent = if incoming {
        link.source_symbol_id()
    } else {
        link.target_symbol_id()
    };
    let symbol = graph.symbol(adjacent, budget)?.ok_or(RocksError::Format)?;
    if &symbol.file_id == anchor {
        return Ok(());
    }
    let file = files.get(&symbol.file_id).ok_or(RocksError::Format)?;
    let edge = graph.edge(link, budget)?;
    record_dependency(grouped, incoming, file, &edge)
}

fn record_dependency(
    grouped: &mut BTreeMap<(bool, String), FileDependencyRow>,
    incoming: bool,
    file: &FileInput,
    edge: &EdgeInput,
) -> Result<(), RocksError> {
    let row = grouped
        .entry((incoming, file.normalized_path.clone()))
        .or_insert_with(|| FileDependencyRow {
            direction: if incoming {
                "dependents"
            } else {
                "dependencies"
            }
            .to_owned(),
            path: file.normalized_path.clone(),
            language: file.language.clone(),
            edge_count: 0,
            site_count: 0,
            edge_kinds: Vec::new(),
        });
    row.edge_count = row.edge_count.checked_add(1).ok_or(RocksError::Format)?;
    row.site_count = row
        .site_count
        .checked_add(u64::from(edge.site_count))
        .ok_or(RocksError::Format)?;
    if !row.edge_kinds.iter().any(|kind| kind == edge.kind.as_str()) {
        row.edge_kinds.push(edge.kind.as_str().to_owned());
        row.edge_kinds.sort();
    }
    Ok(())
}

fn dependency_result(rows: Vec<FileDependencyRow>, limit: u16) -> FileDependencyResult {
    let mut counts = BTreeMap::from([("dependencies".to_owned(), 0), ("dependents".to_owned(), 0)]);
    let mut output = Vec::new();
    let (dependencies, dependents): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| row.direction == "dependencies");
    for (direction, mut selected) in [("dependencies", dependencies), ("dependents", dependents)] {
        counts.insert(direction.to_owned(), selected.len() as u64);
        selected.sort_by(|a, b| {
            b.site_count
                .cmp(&a.site_count)
                .then(b.edge_count.cmp(&a.edge_count))
                .then(a.path.cmp(&b.path))
        });
        selected.truncate(usize::from(limit));
        output.extend(selected);
    }
    let truncated = counts
        .iter()
        .map(|(direction, count)| (direction.clone(), *count > u64::from(limit)))
        .collect();
    FileDependencyResult {
        rows: output,
        counts,
        truncated,
    }
}
