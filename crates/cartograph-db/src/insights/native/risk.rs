//! Native file pressure and orphan analysis preserve source and ranking fences.

use super::{corrupt, neighbors, visit_references, workspace};
use crate::{
    FileHistoryMetrics, FileInput, GraphDirection, NativeDatabase, StorageError, SymbolInput,
    artifacts::native::summaries::visit_documents,
    insights::{
        DEFAULT_INSIGHT_TIMEOUT, DeadCodeCandidate, DeadCodeQuery, StructuralHotspot,
        StructuralHotspotCategory as Category, StructuralHotspotQuery,
        StructuralHotspotSort as Sort, validate_limit,
    },
    retrieval::native::{retain, simple_name, visit_files, visit_symbols},
    rocks::{GraphSnapshot, ReadBudget, RocksError},
};
use cartograph_domain::{DocumentKind, EdgeKind, FileId, ProjectId, Visibility};
use num_traits::ToPrimitive as _;
use std::collections::{BTreeMap, BTreeSet};

struct Risk {
    file: FileInput,
    symbols: u64,
    routes: u64,
    centrality: u32,
    incoming: u64,
    outgoing: u64,
    unresolved: u64,
    dependents: BTreeSet<FileId>,
    history: Option<FileHistoryMetrics>,
}

impl NativeDatabase {
    /// Rank current files under the default structural/change-risk policy.
    /// # Errors
    /// Rejects invalid bounds, malformed graph/history evidence, or exhausted native reads.
    pub async fn current_structural_hotspots(
        &self,
        project: &ProjectId,
        limit: u16,
    ) -> Result<Vec<StructuralHotspot>, StorageError> {
        self.query_structural_hotspots(project, StructuralHotspotQuery::new(limit)?)
            .await
    }

    /// Apply every churn, recency, category, and centrality filter before ranking.
    /// # Errors
    /// Rejects invalid bounds, arithmetic overflow, or exhausted native workspace.
    pub async fn query_structural_hotspots(
        &self,
        project: &ProjectId,
        request: StructuralHotspotQuery,
    ) -> Result<Vec<StructuralHotspot>, StorageError> {
        self.require_project(project)?;
        validate_limit(request.limit)?;
        self.state(None, 0, 0, move |transaction, budget| {
            if transaction.current()?.is_none() {
                return Ok(Vec::new());
            }
            let snapshot = transaction.graph_snapshot()?;
            let graph = snapshot.graph(budget)?;
            let mut files = risk_inventory(&graph, budget)?;
            let paths = files
                .iter()
                .map(|(id, row)| (row.file.normalized_path.clone(), id.clone()))
                .collect::<BTreeMap<_, _>>();
            crate::history::native::visit_metrics(transaction, budget, |path, metrics| {
                if let Some(row) = paths.get(path).and_then(|id| files.get_mut(id)) {
                    row.history = Some(metrics);
                }
                Ok(())
            })?;
            let now = jiff::Timestamp::now().as_second();
            let mut rows = Vec::new();
            let (mut maximum_pressure, mut maximum_churn) = (0_u64, 0_u64);
            for (_, row) in files {
                let row = hotspot(row)?;
                if !matches_hotspot(&row, &request, now) {
                    continue;
                }
                maximum_pressure = maximum_pressure.max(row.structural_risk);
                maximum_churn = maximum_churn.max(churn(&row)?);
                rows.push(row);
            }
            rank_hotspots(rows, maximum_pressure, maximum_churn, request)
        })
        .await
    }

    /// Select structurally unreferenced current implementations with conservative exclusions.
    /// # Errors
    /// Rejects invalid bounds, inconsistent source, or exhausted native graph reads.
    pub async fn current_dead_code(
        &self,
        project: &ProjectId,
        limit: u16,
        include_tests: bool,
    ) -> Result<Vec<DeadCodeCandidate>, StorageError> {
        self.query_current_dead_code(
            project,
            DeadCodeQuery::new(limit)?.with_include_tests(include_tests),
        )
        .await
    }

    /// Apply orphan exclusions before selecting and hydrating the bounded candidate page.
    /// # Errors
    /// Rejects invalid bounds, corrupt topology, or exhausted native workspace.
    pub async fn query_current_dead_code(
        &self,
        project: &ProjectId,
        request: DeadCodeQuery,
    ) -> Result<Vec<DeadCodeCandidate>, StorageError> {
        validate_limit(request.limit)?;
        self.current_graph(project, DEFAULT_INSIGHT_TIMEOUT, move |graph, budget| {
            let mut selected = Vec::new();
            visit_symbols(graph, budget, |symbol, budget| {
                if symbol.export.exported
                    || !matches!(
                        symbol.symbol_kind.as_str(),
                        "function" | "method" | "class" | "struct" | "component" | "resource"
                    )
                {
                    return Ok(());
                }
                let file = graph.file(&symbol.file_id, budget)?.ok_or(corrupt())?;
                if excluded(&symbol, &file, request) || claimed(graph, &symbol, budget)? {
                    return Ok(());
                }
                retain(
                    &mut selected,
                    (symbol, file),
                    usize::from(request.limit),
                    |a, b| {
                        b.0.pagerank_ppb
                            .cmp(&a.0.pagerank_ppb)
                            .then(a.1.normalized_path.cmp(&b.1.normalized_path))
                            .then(a.0.start_line.cmp(&b.0.start_line))
                            .then(a.0.symbol_id.cmp(&b.0.symbol_id))
                    },
                );
                Ok(())
            })?;
            hydrate_orphans(graph, budget, selected)
        })
        .await
        .map(Option::unwrap_or_default)
    }
}

fn matches_hotspot(row: &StructuralHotspot, request: &StructuralHotspotQuery, now: i64) -> bool {
    row.commit_count >= request.minimum_commits
        && row.centrality >= request.minimum_centrality
        && request.recency_days.is_none_or(|days| {
            row.last_touched_unix_seconds
                .and_then(|value| i64::try_from(value).ok())
                .is_some_and(|value| value >= now.saturating_sub(i64::from(days) * 86400))
        })
}

fn rank_hotspots(
    rows: Vec<StructuralHotspot>,
    maximum_pressure: u64,
    maximum_churn: u64,
    request: StructuralHotspotQuery,
) -> Result<Vec<StructuralHotspot>, RocksError> {
    let mut selected = Vec::new();
    for mut row in rows {
        row.structural_pressure = ratio(row.structural_risk, maximum_pressure)?;
        row.churn_score = ratio(churn(&row)?, maximum_churn)?;
        let base = row.centrality * 0.7 + row.structural_pressure * 0.3;
        row.risk_score = base * row.churn_score;
        row.maintenance_score = row.churn_score * (1.0 - row.centrality);
        row.brittle_score = base / (1.0 + row.churn_score);
        if request.category == Category::Brittle && row.external_dependents == 0 {
            continue;
        }
        retain(&mut selected, row, usize::from(request.limit), |a, b| {
            hotspot_order(a, b, request)
        });
    }
    Ok(selected)
}

fn risk_inventory(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
) -> Result<BTreeMap<FileId, Risk>, RocksError> {
    let mut files = BTreeMap::new();
    let mut bytes = 0_usize;
    visit_files(graph, budget, |file, _| {
        bytes = bytes.saturating_add(file.normalized_path.len() * 2 + file.language.len() + 768);
        if bytes > 32 * 1_048_576 {
            return Err(workspace());
        }
        files.insert(
            file.file_id.clone(),
            Risk {
                file,
                symbols: 0,
                routes: 0,
                centrality: 0,
                incoming: 0,
                outgoing: 0,
                unresolved: 0,
                dependents: BTreeSet::new(),
                history: None,
            },
        );
        Ok(())
    })?;
    visit_symbols(graph, budget, |symbol, budget| {
        let row = files.get_mut(&symbol.file_id).ok_or(corrupt())?;
        row.symbols += u64::from(symbol.symbol_kind != "file");
        row.routes += u64::from(symbol.symbol_kind == "route");
        row.centrality = row.centrality.max(symbol.pagerank_ppb.unwrap_or(0));
        neighbors(
            graph,
            &symbol.symbol_id,
            GraphDirection::Outgoing,
            budget,
            |link, budget| {
                if link.kind() == EdgeKind::Contains {
                    return Ok(());
                }
                let count = u64::from(graph.edge(&link, budget)?.site_count);
                let target = graph
                    .symbol(link.target_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                let source = files.get_mut(&symbol.file_id).ok_or(corrupt())?;
                source.outgoing = source.outgoing.checked_add(count).ok_or(corrupt())?;
                let target = files.get_mut(&target.file_id).ok_or(corrupt())?;
                target.incoming = target.incoming.checked_add(count).ok_or(corrupt())?;
                if target.file.file_id != symbol.file_id
                    && target.dependents.insert(symbol.file_id.clone())
                {
                    bytes = bytes.saturating_add(128);
                }
                if bytes > 48 * 1_048_576 {
                    return Err(workspace());
                }
                Ok(())
            },
        )?;
        Ok(())
    })?;
    visit_references(graph, budget, |reference, _| {
        if reference.target_symbol_id.is_none() {
            let row = files.get_mut(&reference.file_id).ok_or(corrupt())?;
            row.unresolved = row
                .unresolved
                .checked_add(u64::from(reference.site_count))
                .ok_or(corrupt())?;
        }
        Ok(())
    })?;
    Ok(files)
}

fn hotspot(row: Risk) -> Result<StructuralHotspot, RocksError> {
    let available = row.history.is_some();
    let history = row.history.unwrap_or(FileHistoryMetrics {
        commit_count: 0,
        author_count: 0,
        insertions: 0,
        deletions: 0,
        last_touched_at: None,
    });
    let structural = row
        .incoming
        .checked_mul(3)
        .and_then(|value| {
            row.outgoing
                .checked_mul(2)
                .and_then(|out| value.checked_add(out))
        })
        .and_then(|value| value.checked_add(row.unresolved))
        .and_then(|value| {
            row.routes
                .checked_mul(4)
                .and_then(|routes| value.checked_add(routes))
        })
        .ok_or(corrupt())?;
    let composite = structural
        .checked_mul(100)
        .and_then(|value| value.checked_add(history.commit_count.min(1000) * 10))
        .and_then(|value| {
            history
                .insertions
                .checked_add(history.deletions)
                .and_then(|lines| value.checked_add((lines / 10).min(1000)))
        })
        .ok_or(corrupt())?;
    let touched = history
        .last_touched_at
        .map(|seconds| {
            i64::try_from(seconds)
                .map_err(|_| corrupt())
                .and_then(|seconds| jiff::Timestamp::from_second(seconds).map_err(|_| corrupt()))
                .map(|timestamp| timestamp.to_string())
        })
        .transpose()?;
    Ok(StructuralHotspot {
        path: row.file.normalized_path,
        language: row.file.language,
        symbols: row.symbols,
        incoming_edges: row.incoming,
        outgoing_edges: row.outgoing,
        unresolved_references: row.unresolved,
        routes: row.routes,
        structural_risk: structural,
        commit_count: history.commit_count,
        author_count: history.author_count,
        insertions: history.insertions,
        deletions: history.deletions,
        last_touched_at: touched,
        last_touched_unix_seconds: history.last_touched_at,
        history_available: available,
        composite_risk: composite,
        centrality: f64::from(row.centrality) / 1e9,
        structural_pressure: 0.0,
        churn_score: 0.0,
        risk_score: 0.0,
        maintenance_score: 0.0,
        brittle_score: 0.0,
        external_dependents: row.dependents.len() as u64,
    })
}
fn churn(row: &StructuralHotspot) -> Result<u64, RocksError> {
    row.commit_count
        .checked_mul(10)
        .and_then(|value| value.checked_add(row.insertions))
        .and_then(|value| value.checked_add(row.deletions))
        .ok_or(corrupt())
}
fn ratio(value: u64, maximum: u64) -> Result<f64, RocksError> {
    Ok(if maximum == 0 {
        0.0
    } else {
        value.to_f64().ok_or(corrupt())? / maximum.to_f64().ok_or(corrupt())?
    })
}
fn hotspot_order(
    a: &StructuralHotspot,
    b: &StructuralHotspot,
    request: StructuralHotspotQuery,
) -> std::cmp::Ordering {
    let (first_a, first_b, second_a, second_b) = match (request.category, request.sort) {
        (Category::Risk, Sort::Centrality) => {
            (a.centrality, b.centrality, a.risk_score, b.risk_score)
        }
        (Category::Risk, Sort::Churn) => (a.churn_score, b.churn_score, a.risk_score, b.risk_score),
        (Category::Risk, Sort::Risk) => (a.risk_score, b.risk_score, a.centrality, b.centrality),
        (Category::Maintenance, _) => (
            a.maintenance_score,
            b.maintenance_score,
            a.churn_score,
            b.churn_score,
        ),
        (Category::Brittle, _) => (a.brittle_score, b.brittle_score, a.centrality, b.centrality),
    };
    first_b
        .total_cmp(&first_a)
        .then(second_b.total_cmp(&second_a))
        .then(a.path.cmp(&b.path))
}

fn excluded(symbol: &SymbolInput, file: &FileInput, request: DeadCodeQuery) -> bool {
    let segments = file
        .normalized_path
        .to_ascii_lowercase()
        .split('/')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let test = crate::retrieval::native::test_path(&file.normalized_path)
        || segments.iter().any(|part| {
            matches!(
                part.as_str(),
                "script"
                    | "scripts"
                    | "bench"
                    | "benches"
                    | "benchmark"
                    | "benchmarks"
                    | "example"
                    | "examples"
                    | "sample"
                    | "samples"
                    | "demo"
                    | "demos"
            )
        })
        || (file.language == "rust"
            && symbol
                .qualified_name
                .split("::")
                .any(|part| part == "tests"));
    let fixture = segments.iter().any(|part| {
        matches!(
            part.as_str(),
            "fixture" | "fixtures" | "test-bed" | "test-beds" | "__mocks__" | "mock" | "mocks"
        )
    });
    let name = simple_name(&symbol.qualified_name);
    (!request.include_tests && test)
        || (request.exclude_fixtures && fixture)
        || (file.language == "rust"
            && name == "main"
            && (matches!(
                file.normalized_path.rsplit('/').next(),
                Some("build.rs" | "main.rs")
            ) || file.normalized_path.contains("/bin/")))
        || (file.normalized_path.starts_with("src/installer/targets/")
            && symbol.symbol_kind == "method"
            && matches!(
                name,
                "supportsLocation"
                    | "detect"
                    | "install"
                    | "uninstall"
                    | "printConfig"
                    | "describePaths"
            ))
}
fn claimed(
    graph: &GraphSnapshot<'_, '_>,
    symbol: &SymbolInput,
    budget: &mut ReadBudget,
) -> Result<bool, RocksError> {
    let mut claimed = false;
    neighbors(
        graph,
        &symbol.symbol_id,
        GraphDirection::Incoming,
        budget,
        |link, budget| {
            if link.source_symbol_id() != &symbol.symbol_id
                && matches!(
                    link.kind().as_str(),
                    "calls"
                        | "references"
                        | "instantiates"
                        | "tests"
                        | "exports"
                        | "implements"
                        | "extends"
                        | "overrides"
                        | "decorates"
                        | "type_of"
                        | "returns"
                        | "field_access"
                )
            {
                claimed = true;
            }
            if link.kind() == EdgeKind::Contains
                && symbol.symbol_kind == "method"
                && !matches!(
                    symbol.visibility,
                    Some(Visibility::Private | Visibility::Protected)
                )
            {
                let parent = graph
                    .symbol(link.source_symbol_id(), budget)?
                    .ok_or(corrupt())?;
                if parent.export.exported
                    && matches!(
                        parent.symbol_kind.as_str(),
                        "class" | "struct" | "trait" | "protocol" | "interface"
                    )
                {
                    claimed = true;
                }
            }
            Ok(())
        },
    )?;
    Ok(claimed)
}

fn hydrate_orphans(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    selected: Vec<(SymbolInput, FileInput)>,
) -> Result<Vec<DeadCodeCandidate>, RocksError> {
    let ids = selected
        .iter()
        .map(|(symbol, _)| symbol.symbol_id.clone())
        .collect::<BTreeSet<_>>();
    let mut code = BTreeMap::new();
    let mut bytes = 0_usize;
    visit_documents(graph, budget, |document, _| {
        if matches!(document.kind, DocumentKind::Symbol | DocumentKind::Test)
            && let Some(id) = document.symbol_id
            && ids.contains(&id)
            && !code.contains_key(&id)
        {
            bytes = bytes.saturating_add(document.code.len() + 128);
            if bytes > 32 * 1_048_576 {
                return Err(workspace());
            }
            code.insert(id, document.code);
        }
        Ok(())
    })?;
    let mut output = Vec::new();
    for (symbol, file) in selected {
        let mut outgoing = 0_u64;
        let mut dispatch = false;
        neighbors(
            graph,
            &symbol.symbol_id,
            GraphDirection::Outgoing,
            budget,
            |link, _| {
                outgoing += u64::from(!matches!(link.kind().as_str(), "contains" | "def_use"));
                Ok(())
            },
        )?;
        neighbors(
            graph,
            &symbol.symbol_id,
            GraphDirection::Incoming,
            budget,
            |link, budget| {
                if link.kind() == EdgeKind::Contains {
                    neighbors(
                        graph,
                        link.source_symbol_id(),
                        GraphDirection::Outgoing,
                        budget,
                        |parent, _| {
                            dispatch |= matches!(parent.kind().as_str(), "implements" | "extends");
                            Ok(())
                        },
                    )?;
                }
                Ok(())
            },
        )?;
        let safe_code = code.remove(&symbol.symbol_id).unwrap_or_default();
        output.push((
            DeadCodeCandidate {
                symbol_id: symbol.symbol_id.to_string(),
                path: file.normalized_path,
                language: file.language,
                symbol_kind: symbol.symbol_kind,
                qualified_name: symbol.qualified_name,
                start_line: symbol.start_line,
                incoming_edges: 0,
                outgoing_edges: outgoing,
                safe_code,
                interface_dispatch_risk: dispatch,
                reason: "no_incoming_structural_evidence",
            },
            symbol.pagerank_ppb,
        ));
    }
    output.sort_by(|a, b| {
        b.0.outgoing_edges
            .cmp(&a.0.outgoing_edges)
            .then(b.1.cmp(&a.1))
            .then(a.0.path.cmp(&b.0.path))
            .then(a.0.start_line.cmp(&b.0.start_line))
            .then(a.0.symbol_id.cmp(&b.0.symbol_id))
    });
    Ok(output.into_iter().map(|(row, _)| row).collect())
}
