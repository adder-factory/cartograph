//! Exact imports, references, and grouped symbols preserve complete pre-limit filters.

use super::{corrupt, visit_references, workspace};
use crate::{
    FileInput, NativeDatabase, StorageError, SymbolInput,
    insights::{
        DEFAULT_INSIGHT_TIMEOUT, DependencyCoverageRow, ExternalImportRecord, GroupedSymbolPeer,
        GroupedSymbolPeers, GroupedSymbolQuery, ImportInsight, IndexedFileFingerprint,
        RenameReferenceSite, validate_limit,
    },
    retrieval::native::{retain, visit_files, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError},
};
use cartograph_domain::{FileId, NormalizedPath, ProjectId, SymbolId};
use std::collections::BTreeMap;

impl NativeDatabase {
    /// List current import references in canonical path/span order.
    /// # Errors
    /// Rejects invalid bounds, corrupt source identities, or exhausted native reads.
    pub async fn current_imports(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<ImportInsight>, StorageError> {
        validate_limit(limit)?;
        self.imports_native(project, usize::from(limit), false)
            .await
    }

    /// Return the complete bounded import relation before higher-level filtering.
    /// # Errors
    /// Rejects a result beyond the explicit row/byte bounds or inconsistent native facts.
    pub async fn current_import_evidence(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<ImportInsight>, StorageError> {
        self.imports_native(project, 250_000, true).await
    }

    async fn imports_native(
        &self,
        project: &ProjectId,
        limit: usize,
        complete: bool,
    ) -> Result<Vec<ImportInsight>, StorageError> {
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
            let anchors = file_anchors(graph, budget)?;
            let mut output = Vec::new();
            let mut bytes = 0_usize;
            visit_references(graph, budget, |reference, budget| {
                if reference.reference_kind != "imports" {
                    return Ok(());
                }
                let Some(anchor) = anchors.get(&reference.file_id) else {
                    return Ok(());
                };
                let file = graph.file(&reference.file_id, budget)?.ok_or(corrupt())?;
                let target = reference
                    .target_symbol_id
                    .as_ref()
                    .map(|id| graph.symbol(id, budget))
                    .transpose()?
                    .flatten();
                let target_path = target
                    .map(|symbol| graph.file(&symbol.file_id, budget))
                    .transpose()?
                    .flatten()
                    .map(|file| file.normalized_path);
                let row = ImportInsight {
                    path: file.normalized_path,
                    language: file.language,
                    owner_symbol_id: reference.owner_symbol_id.map(|id| id.to_string()),
                    module_specifier: reference.reference_name,
                    target_symbol_id: reference.target_symbol_id.map(|id| id.to_string()),
                    target_path,
                    start_byte: reference.start_byte,
                    end_byte: reference.end_byte,
                    confidence: reference.confidence,
                    provenance: reference.resolution_provenance,
                    site_count: u64::from(reference.site_count),
                    source_file_symbol_id: anchor.to_string(),
                };
                let size = row.path.len()
                    + row.language.len()
                    + row.module_specifier.len()
                    + row.provenance.len()
                    + row.target_path.as_ref().map_or(0, String::len)
                    + 512;
                if complete {
                    bytes = bytes.saturating_add(size);
                    if output.len() >= limit || bytes > 32 * 1_048_576 {
                        return Err(workspace());
                    }
                    output.push(row);
                } else {
                    retain(&mut output, row, limit, import_order);
                }
                Ok(())
            })?;
            output.sort_by(import_order);
            Ok(output)
        })
        .await
        .map(Option::unwrap_or_default)
    }

    /// Aggregate JavaScript-family module specifiers with exact represented site counts.
    /// # Errors
    /// Rejects oversized aggregate workspace, project mismatch, or corrupt references.
    pub async fn current_external_imports(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<ExternalImportRecord>, StorageError> {
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, |graph, budget| {
            let mut groups = BTreeMap::<(String, String, String), u64>::new();
            let mut bytes = 0_usize;
            visit_references(graph, budget, |reference, budget| {
                if reference.reference_kind != "imports" {
                    return Ok(());
                }
                let file = graph.file(&reference.file_id, budget)?.ok_or(corrupt())?;
                if !matches!(
                    file.language.as_str(),
                    "javascript" | "jsx" | "typescript" | "tsx"
                ) {
                    return Ok(());
                }
                let key = (
                    file.normalized_path,
                    file.language,
                    reference.reference_name,
                );
                if !groups.contains_key(&key) {
                    bytes = bytes.saturating_add(key.0.len() + key.1.len() + key.2.len() + 192);
                }
                *groups.entry(key).or_default() += u64::from(reference.site_count);
                if groups.len() > 250_000 || bytes > 32 * 1_048_576 {
                    return Err(workspace());
                }
                Ok(())
            })?;
            Ok(groups
                .into_iter()
                .map(
                    |((path, language, module_specifier), site_count)| ExternalImportRecord {
                        path,
                        language,
                        module_specifier,
                        site_count,
                    },
                )
                .collect())
        })
        .await
        .map(Option::unwrap_or_default)
    }

    /// Aggregate reference resolution by language and relationship family before limiting.
    /// # Errors
    /// Rejects invalid limits, corrupt references, or bounded grouping exhaustion.
    pub async fn current_dependency_coverage(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<DependencyCoverageRow>, StorageError> {
        validate_limit(limit)?;
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
            let mut groups = BTreeMap::<(String, String), DependencyCoverageRow>::new();
            visit_references(graph, budget, |reference, budget| {
                let file = graph.file(&reference.file_id, budget)?.ok_or(corrupt())?;
                let key = (file.language, reference.reference_kind);
                let group = groups.entry(key.clone()).or_insert(DependencyCoverageRow {
                    language: key.0,
                    reference_kind: key.1,
                    references: 0,
                    resolved: 0,
                    unresolved: 0,
                    represented_sites: 0,
                });
                group.references += 1;
                group.resolved += u64::from(reference.target_symbol_id.is_some());
                group.unresolved += u64::from(reference.target_symbol_id.is_none());
                group.represented_sites += u64::from(reference.site_count);
                if groups.len() > 8192 {
                    return Err(workspace());
                }
                Ok(())
            })?;
            let mut rows = groups.into_values().collect::<Vec<_>>();
            rows.sort_by(|a, b| {
                b.unresolved
                    .cmp(&a.unresolved)
                    .then(a.language.cmp(&b.language))
                    .then(a.reference_kind.cmp(&b.reference_kind))
            });
            rows.truncate(usize::from(limit));
            Ok(rows)
        })
        .await
        .map(Option::unwrap_or_default)
    }

    /// List exact current rename references with the display limit applied last.
    /// # Errors
    /// Rejects invalid bounds, corrupt spans, or exhausted native read limits.
    pub async fn current_rename_sites(
        &self,
        project: &ProjectId,
        id: &SymbolId,
        limit: u16,
    ) -> Result<Vec<RenameReferenceSite>, StorageError> {
        validate_limit(limit)?;
        self.rename_native(project, id, usize::from(limit), false)
            .await
    }

    /// Return every bounded reference before textual rename de-duplication.
    /// # Errors
    /// Rejects complete relations exceeding the explicit row/byte bound.
    pub async fn current_rename_reference_evidence(
        &self,
        project: &ProjectId,
        id: &SymbolId,
    ) -> Result<Vec<RenameReferenceSite>, StorageError> {
        self.rename_native(project, id, 250_000, true).await
    }

    async fn rename_native(
        &self,
        project: &ProjectId,
        id: &SymbolId,
        limit: usize,
        complete: bool,
    ) -> Result<Vec<RenameReferenceSite>, StorageError> {
        let id = id.clone();
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
            let mut rows = Vec::new();
            let mut bytes = 0_usize;
            visit_references(graph, budget, |reference, budget| {
                if reference.target_symbol_id.as_ref() != Some(&id) {
                    return Ok(());
                }
                let file = graph.file(&reference.file_id, budget)?.ok_or(corrupt())?;
                let row = RenameReferenceSite {
                    path: file.normalized_path,
                    owner_symbol_id: reference.owner_symbol_id.map(|id| id.to_string()),
                    start_byte: reference.start_byte,
                    end_byte: reference.end_byte,
                    reference_kind: reference.reference_kind,
                    confidence: reference.confidence,
                    provenance: reference.resolution_provenance,
                };
                if complete {
                    bytes = bytes.saturating_add(
                        row.path.len() + row.provenance.len() + row.reference_kind.len() + 256,
                    );
                    if rows.len() >= limit || bytes > 32 * 1_048_576 {
                        return Err(workspace());
                    }
                    rows.push(row);
                } else {
                    retain(&mut rows, row, limit, rename_order);
                }
                Ok(())
            })?;
            rows.sort_by(rename_order);
            Ok(rows)
        })
        .await
        .map(Option::unwrap_or_default)
    }

    /// Count logical reference rows, preserving their independent site multiplicity.
    /// # Errors
    /// Rejects project mismatch or bounded native read failure.
    pub async fn current_rename_site_count(
        &self,
        project: &ProjectId,
        id: &SymbolId,
    ) -> Result<u64, StorageError> {
        let id = id.clone();
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
            let mut count = 0_u64;
            visit_references(graph, budget, |reference, _| {
                count += u64::from(reference.target_symbol_id.as_ref() == Some(&id));
                Ok(())
            })?;
            Ok(count)
        })
        .await
        .map(Option::unwrap_or_default)
    }

    /// Return the exact current source manifest used by live drift classification.
    /// # Errors
    /// Rejects malformed paths, oversized manifests, or exhausted native read bounds.
    pub async fn current_file_fingerprints(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<IndexedFileFingerprint>, StorageError> {
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, |graph, budget| {
            let mut rows = Vec::new();
            let mut bytes = 0_usize;
            visit_files(graph, budget, |file, _| {
                bytes = bytes.saturating_add(file.normalized_path.len() + 192);
                if rows.len() >= 250_000 || bytes > 32 * 1_048_576 {
                    return Err(workspace());
                }
                rows.push(IndexedFileFingerprint {
                    path: NormalizedPath::parse(&file.normalized_path).map_err(|_| corrupt())?,
                    content_hash: file.content_hash,
                });
                Ok(())
            })?;
            rows.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(rows)
        })
        .await
        .map(Option::unwrap_or_default)
    }

    /// Count complete path groups independently before selecting their bounded peers.
    /// # Errors
    /// Rejects project mismatch, inconsistent facts, or bounded native read exhaustion.
    pub async fn current_grouped_path_symbols(
        &self,
        project: &ProjectId,
        request: GroupedSymbolQuery,
    ) -> Result<Vec<GroupedSymbolPeers>, StorageError> {
        let empty = request
            .groups
            .iter()
            .map(|group| GroupedSymbolPeers {
                key: group.key.clone(),
                total: 0,
                peers: Vec::new(),
                truncated: false,
            })
            .collect::<Vec<_>>();
        let output = self
            .current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
                let mut groups = request
                    .groups
                    .iter()
                    .map(|group| GroupedSymbolPeers {
                        key: group.key.clone(),
                        total: 0,
                        peers: Vec::new(),
                        truncated: false,
                    })
                    .collect::<Vec<_>>();
                let mut bytes = 0_usize;
                visit_symbols(graph, budget, |symbol, budget| {
                    if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter")
                        || request.excluded_symbol_id.as_ref() == Some(&symbol.symbol_id)
                    {
                        return Ok(());
                    }
                    let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                    for (input, output) in request.groups.iter().zip(&mut groups) {
                        if !input
                            .paths
                            .iter()
                            .any(|path| path.as_str() == file.normalized_path)
                        {
                            continue;
                        }
                        retain_peer(
                            output,
                            &symbol,
                            &file,
                            usize::from(request.per_group_limit),
                            &mut bytes,
                        )?;
                    }
                    Ok(())
                })?;
                for group in &mut groups {
                    group.truncated = group.total > group.peers.len() as u64;
                }
                Ok(groups)
            })
            .await?;
        Ok(output.unwrap_or(empty))
    }
}

fn file_anchors(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<FileId, SymbolId>, RocksError> {
    let mut anchors = BTreeMap::new();
    visit_symbols(graph, budget, |symbol, _| {
        if symbol.symbol_kind == "file" {
            anchors.insert(symbol.file_id, symbol.symbol_id);
        }
        if anchors.len() > 131_072 {
            return Err(workspace());
        }
        Ok(())
    })?;
    Ok(anchors)
}

fn retain_peer(
    output: &mut GroupedSymbolPeers,
    symbol: &SymbolInput,
    file: &FileInput,
    limit: usize,
    bytes: &mut usize,
) -> Result<(), RocksError> {
    output.total += 1;
    let before = output.peers.iter().map(peer_bytes).sum::<usize>();
    retain(
        &mut output.peers,
        GroupedSymbolPeer {
            symbol_id: symbol.symbol_id.to_string(),
            path: file.normalized_path.clone(),
            language: file.language.clone(),
            symbol_kind: symbol.symbol_kind.clone(),
            qualified_name: symbol.qualified_name.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
        },
        limit,
        |a, b| {
            a.path
                .cmp(&b.path)
                .then(a.start_line.cmp(&b.start_line))
                .then(a.symbol_id.cmp(&b.symbol_id))
        },
    );
    *bytes = bytes
        .checked_sub(before)
        .and_then(|bytes| bytes.checked_add(output.peers.iter().map(peer_bytes).sum::<usize>()))
        .ok_or_else(workspace)?;
    if *bytes > 32 * 1_048_576 {
        return Err(workspace());
    }
    Ok(())
}

fn peer_bytes(peer: &GroupedSymbolPeer) -> usize {
    peer.path.len() + peer.language.len() + peer.symbol_kind.len() + peer.qualified_name.len() + 256
}

fn import_order(a: &ImportInsight, b: &ImportInsight) -> std::cmp::Ordering {
    a.path
        .cmp(&b.path)
        .then(a.start_byte.cmp(&b.start_byte))
        .then(a.end_byte.cmp(&b.end_byte))
        .then(a.module_specifier.cmp(&b.module_specifier))
}
fn rename_order(a: &RenameReferenceSite, b: &RenameReferenceSite) -> std::cmp::Ordering {
    a.path
        .cmp(&b.path)
        .then(a.start_byte.cmp(&b.start_byte))
        .then(a.end_byte.cmp(&b.end_byte))
        .then(a.reference_kind.cmp(&b.reference_kind))
}
