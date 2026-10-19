//! Reverse graph traversal keeps minimum distance, deterministic admission, and totals.

use super::{corrupt, neighbors, workspace};
use crate::{
    GraphDirection, NativeDatabase, StorageError,
    artifacts::native::summaries::visit_documents,
    insights::{
        DEFAULT_INSIGHT_TIMEOUT, FileTestImpact, FileTestImpactQuery, FileTestImpactResult,
        StructuralCoverageRow, validate_limit,
    },
    retrieval::native::{retain, visit_files, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError},
};
use cartograph_domain::{DocumentKind, EdgeKind, FileId, SymbolId};
use std::collections::{BTreeMap, BTreeSet};

impl NativeDatabase {
    /// Traverse reverse dependencies from every current symbol in the changed files.
    /// # Errors
    /// Rejects invalid bounds/regexes, changed generations, or exhausted bounded work.
    pub async fn current_file_test_impact(
        &self,
        input: FileTestImpactQuery<'_>,
    ) -> Result<Option<FileTestImpactResult>, StorageError> {
        self.require_project(input.project_id)?;
        if input.paths.is_empty() || input.paths.len() > 128 {
            return Err(StorageError::InvalidInput {
                field: "test_impact_paths",
            });
        }
        if input.max_depth == 0 || input.max_depth > 50 {
            return Err(StorageError::InvalidInput {
                field: "test_impact_depth",
            });
        }
        validate_limit(input.max_nodes)?;
        validate_limit(input.limit)?;
        let pattern = test_pattern(input.test_path_regex)?;
        let Some(current) = self.current_generation_record(input.project_id).await? else {
            return Ok(None);
        };
        let generation = current.generation_id().clone();
        let paths = input
            .paths
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let (depth, maximum, limit) = (
            input.max_depth,
            usize::from(input.max_nodes),
            usize::from(input.limit),
        );
        self.graph(
            current.generation_id(),
            DEFAULT_INSIGHT_TIMEOUT,
            move |graph, budget| {
                let seeds = seed_files(graph, budget, &paths)?;
                let (reached, nodes_truncated) = walk(graph, budget, &seeds, depth, maximum)?;
                let ImpactFiles {
                    tests,
                    affected,
                    dependents,
                    barrels,
                    barrel_count,
                } = impact_files(graph, budget, reached, pattern.as_ref(), limit)?;
                let mut matched_inputs = seeds.into_values().collect::<Vec<_>>();
                matched_inputs.sort();
                Ok(Some(FileTestImpactResult {
                    generation_id: generation,
                    matched_inputs,
                    dependent_file_count: dependents,
                    affected_test_file_count: affected,
                    reached_barrel_count: barrel_count,
                    barrels_truncated: nodes_truncated || barrel_count > barrels.len() as u64,
                    reached_barrels: barrels,
                    nodes_truncated,
                    tests_truncated: nodes_truncated || affected > tests.len() as u64,
                    tests,
                }))
            },
        )
        .await
    }

    /// Rank current symbols by distinct incoming test files and referring symbols.
    /// # Errors
    /// Rejects invalid bounds, malformed graph records, or exhausted native work.
    pub async fn current_structural_coverage(
        &self,
        project: &cartograph_domain::ProjectId,
        limit: u16,
    ) -> Result<Vec<StructuralCoverageRow>, StorageError> {
        validate_limit(limit)?;
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
            let tests = test_files(graph, budget)?;
            let mut output = Vec::new();
            visit_symbols(graph, budget, |symbol, budget| {
                if matches!(symbol.symbol_kind.as_str(), "file" | "import" | "parameter") {
                    return Ok(());
                }
                let (incoming, test_sources) =
                    incoming_test_sources(graph, budget, &symbol.symbol_id, &tests)?;
                let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                retain(
                    &mut output,
                    (
                        StructuralCoverageRow {
                            symbol_id: symbol.symbol_id.to_string(),
                            path: file.normalized_path,
                            qualified_name: symbol.qualified_name,
                            symbol_kind: symbol.symbol_kind,
                            direct_test_files: test_sources,
                            incoming_edges: incoming,
                        },
                        symbol.start_line,
                    ),
                    usize::from(limit),
                    |a, b| {
                        b.0.direct_test_files
                            .cmp(&a.0.direct_test_files)
                            .then(b.0.incoming_edges.cmp(&a.0.incoming_edges))
                            .then(a.0.path.cmp(&b.0.path))
                            .then(a.1.cmp(&b.1))
                            .then(a.0.symbol_id.cmp(&b.0.symbol_id))
                    },
                );
                Ok(())
            })?;
            Ok(output.into_iter().map(|(row, _)| row).collect())
        })
        .await
        .map(Option::unwrap_or_default)
    }
}

fn test_pattern(input: Option<&str>) -> Result<Option<regex::Regex>, StorageError> {
    input
        .map(|pattern| {
            if pattern.is_empty() || pattern.len() > 4096 {
                return Err(StorageError::InvalidInput {
                    field: "test_impact_filter",
                });
            }
            regex::RegexBuilder::new(pattern)
                .size_limit(1_048_576)
                .dfa_size_limit(1_048_576)
                .build()
                .map_err(|_| StorageError::InvalidInput {
                    field: "test_impact_filter",
                })
        })
        .transpose()
}

fn seed_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    paths: &BTreeSet<String>,
) -> Result<BTreeMap<FileId, String>, RocksError> {
    let mut files = BTreeMap::new();
    visit_files(graph, budget, |file, _| {
        if paths.contains(&file.normalized_path) {
            files.insert(file.file_id, file.normalized_path);
        }
        Ok(())
    })?;
    Ok(files)
}
fn walk(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    seeds: &BTreeMap<FileId, String>,
    max_depth: u8,
    maximum: usize,
) -> Result<(BTreeMap<SymbolId, u8>, bool), RocksError> {
    let mut roots = BTreeSet::new();
    visit_symbols(graph, budget, |symbol, _| {
        if seeds.contains_key(&symbol.file_id) {
            admit_smallest(&mut roots, symbol.symbol_id, maximum + 1);
        }
        Ok(())
    })?;
    let mut truncated = roots.len() > maximum;
    let mut reached = roots
        .iter()
        .take(maximum)
        .cloned()
        .map(|id| (id, 0))
        .collect::<BTreeMap<_, _>>();
    let mut frontier = roots.into_iter().take(maximum).collect::<BTreeSet<_>>();
    for depth in 1..=max_depth {
        if truncated || frontier.is_empty() {
            break;
        }
        let slots = maximum.saturating_sub(reached.len());
        let mut next = BTreeSet::new();
        for id in frontier {
            neighbors(graph, &id, GraphDirection::Incoming, budget, |link, _| {
                if link.kind() != EdgeKind::Contains
                    && !reached.contains_key(link.source_symbol_id())
                {
                    admit_smallest(&mut next, link.source_symbol_id().clone(), slots + 1);
                }
                Ok(())
            })?;
        }
        truncated = next.len() > slots;
        frontier = next.into_iter().take(slots).collect();
        for id in &frontier {
            reached.insert(id.clone(), depth);
        }
    }
    Ok((reached, truncated))
}
fn admit_smallest(set: &mut BTreeSet<SymbolId>, id: SymbolId, maximum: usize) {
    set.insert(id);
    if set.len() > maximum {
        set.pop_last();
    }
}
pub(super) fn test_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeSet<FileId>, RocksError> {
    let mut files = BTreeSet::new();
    visit_documents(graph, budget, |document, _| {
        if document.kind == DocumentKind::Test
            && let Some(id) = document.file_id
        {
            files.insert(id);
        }
        if files.len() > 131_072 {
            return Err(workspace());
        }
        Ok(())
    })?;
    Ok(files)
}
fn barrel(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|name| {
        matches!(
            name,
            "index.ts"
                | "index.tsx"
                | "index.js"
                | "index.jsx"
                | "index.mjs"
                | "index.cjs"
                | "index.mts"
                | "index.cts"
        )
    })
}

struct ImpactFiles {
    tests: Vec<FileTestImpact>,
    affected: u64,
    dependents: u64,
    barrels: Vec<String>,
    barrel_count: u64,
}
fn impact_files(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    reached: BTreeMap<SymbolId, u8>,
    pattern: Option<&regex::Regex>,
    limit: usize,
) -> Result<ImpactFiles, RocksError> {
    let mut files = BTreeMap::<FileId, (String, u8)>::new();
    for (id, distance) in reached {
        let symbol = graph.symbol(&id, budget)?.ok_or(corrupt())?;
        let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
        files
            .entry(file.file_id)
            .and_modify(|(_, old)| *old = (*old).min(distance))
            .or_insert((file.normalized_path, distance));
    }
    let test_files = test_files(graph, budget)?;
    let mut tests = Vec::new();
    let mut affected = 0_u64;
    let mut dependents = 0_u64;
    let mut barrels = Vec::new();
    let mut barrel_count = 0_u64;
    for (id, (path, distance)) in files {
        if distance > 0 {
            dependents += 1;
        }
        if distance > 0 && barrel(&path) {
            barrel_count += 1;
            retain(&mut barrels, path.clone(), 100, String::cmp);
        }
        if pattern.map_or_else(
            || test_files.contains(&id),
            |pattern| pattern.is_match(&path),
        ) {
            affected += 1;
            retain(
                &mut tests,
                FileTestImpact {
                    path,
                    distance,
                    reason: "current_generation_dependency_graph",
                },
                limit,
                |a, b| a.distance.cmp(&b.distance).then(a.path.cmp(&b.path)),
            );
        }
    }
    Ok(ImpactFiles {
        tests,
        affected,
        dependents,
        barrels,
        barrel_count,
    })
}
fn incoming_test_sources(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    symbol: &SymbolId,
    tests: &BTreeSet<FileId>,
) -> Result<(u64, u64), RocksError> {
    let mut incoming = BTreeSet::new();
    let mut test_sources = BTreeSet::new();
    neighbors(
        graph,
        symbol,
        GraphDirection::Incoming,
        budget,
        |link, budget| {
            if link.kind() == EdgeKind::Contains {
                return Ok(());
            }
            incoming.insert(link.source_symbol_id().clone());
            let source = graph
                .symbol(link.source_symbol_id(), budget)?
                .ok_or(corrupt())?;
            if tests.contains(&source.file_id) {
                test_sources.insert(source.file_id);
            }
            if incoming.len() > 131_072 || test_sources.len() > 131_072 {
                return Err(workspace());
            }
            Ok(())
        },
    )?;
    Ok((incoming.len() as u64, test_sources.len() as u64))
}
