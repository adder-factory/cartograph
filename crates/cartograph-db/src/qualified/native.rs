//! Field-qualified predicates run before bounded native result selection.

use super::{
    DEFAULT_QUALIFIED_SEARCH_TIMEOUT, QualifiedCentralityComparator, QualifiedSymbolHit,
    QualifiedSymbolPage, QualifiedSymbolQuery, QualifiedSymbolSort, validate_query,
};
use crate::{
    FileInput, GraphDirection, NativeDatabase, StorageError, SymbolInput,
    retrieval::native::{in_directory, retain, simple_name, symbol_record, visit_symbols_named},
    rocks::{GraphLink, GraphSnapshot, ReadBudget, RocksError},
};
use cartograph_domain::{EdgeKind, GenerationId};
use std::{cmp::Ordering, time::Duration};

struct Filters {
    text: String,
    kinds: Vec<String>,
    languages: Vec<String>,
    prefixes: Vec<String>,
    paths: Vec<String>,
    names: Vec<String>,
    signatures: Vec<String>,
    callers: Vec<String>,
    callees: Vec<String>,
    dependencies: Vec<String>,
    centrality: Option<(QualifiedCentralityComparator, f64)>,
    sort: QualifiedSymbolSort,
    limit: u16,
}

impl From<QualifiedSymbolQuery<'_>> for Filters {
    fn from(query: QualifiedSymbolQuery<'_>) -> Self {
        Self {
            text: query.text.to_lowercase(),
            kinds: query.kinds.to_vec(),
            languages: query.languages.to_vec(),
            prefixes: query.path_prefixes.to_vec(),
            paths: query
                .path_filters
                .iter()
                .map(|v| v.to_lowercase())
                .collect(),
            names: query
                .name_filters
                .iter()
                .map(|v| v.to_lowercase())
                .collect(),
            signatures: query
                .signature_filters
                .iter()
                .map(|v| v.to_lowercase())
                .collect(),
            callers: query.callers_of.to_vec(),
            callees: query.callees_of.to_vec(),
            dependencies: query.depends_on.to_vec(),
            centrality: query.centrality,
            sort: query.sort,
            limit: query.limit,
        }
    }
}

impl NativeDatabase {
    /// Apply all exact, field, graph, and centrality filters before the limit.
    /// # Errors
    /// Rejects invalid filters, changed publication, or finite native read limits.
    pub async fn qualified_current_symbols(
        &self,
        input: QualifiedSymbolQuery<'_>,
    ) -> Result<QualifiedSymbolPage, StorageError> {
        self.qualified_current_symbols_bounded(input, DEFAULT_QUALIFIED_SEARCH_TIMEOUT)
            .await
    }

    /// Execute field-qualified search under an explicit native deadline.
    /// # Errors
    /// Rejects invalid input, changed generation, or exhausted native reads.
    pub async fn qualified_current_symbols_bounded(
        &self,
        input: QualifiedSymbolQuery<'_>,
        timeout: Duration,
    ) -> Result<QualifiedSymbolPage, StorageError> {
        self.require_project(input.project_id)?;
        validate_query(&input)?;
        let expected = input.expected_generation_id.clone();
        let filters = Filters::from(input);
        self.graph(
            input.expected_generation_id,
            timeout,
            move |graph, budget| select(graph, budget, &expected, &filters),
        )
        .await
    }
}

fn select(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    generation: &GenerationId,
    filters: &Filters,
) -> Result<QualifiedSymbolPage, RocksError> {
    let mut page = QualifiedSymbolPage {
        total: 0,
        hits: Vec::new(),
    };
    visit_symbols_named(graph, budget, Some(&filters.text), |symbol, budget| {
        if !matches_symbol(&symbol, filters) {
            return Ok(());
        }
        let file = graph
            .file(&symbol.file_id, budget)?
            .ok_or(RocksError::Format)?;
        if !matches_file(&file, filters) || !matches_graph(graph, budget, &symbol, filters)? {
            return Ok(());
        }
        let centrality = symbol
            .pagerank_ppb
            .map(|value| f64::from(value) / 1_000_000_000.0);
        page.total = page.total.checked_add(1).ok_or(RocksError::Format)?;
        retain(
            &mut page.hits,
            QualifiedSymbolHit {
                symbol: symbol_record(generation, symbol, graph, budget)?,
                centrality,
                lexical_score: None,
            },
            usize::from(filters.limit),
            |a, b| compare(a, b, filters.sort),
        );
        Ok(())
    })?;
    Ok(page)
}

fn matches_symbol(symbol: &SymbolInput, filters: &Filters) -> bool {
    (filters.text.is_empty()
        || simple_name(&symbol.qualified_name).to_lowercase() == filters.text
        || symbol.qualified_name.to_lowercase() == filters.text)
        && (filters.kinds.is_empty() || filters.kinds.contains(&symbol.symbol_kind))
        && substrings(simple_name(&symbol.qualified_name), &filters.names)
        && substrings(&symbol.signature, &filters.signatures)
        && centrality_matches(symbol.pagerank_ppb, filters.centrality)
}

fn matches_file(file: &FileInput, filters: &Filters) -> bool {
    (filters.languages.is_empty() || filters.languages.contains(&file.language))
        && (filters.prefixes.is_empty()
            || filters
                .prefixes
                .iter()
                .any(|prefix| in_directory(&file.normalized_path, prefix)))
        && substrings(&file.normalized_path, &filters.paths)
}

fn substrings(value: &str, filters: &[String]) -> bool {
    filters.is_empty() || {
        let value = value.to_lowercase();
        filters.iter().any(|filter| value.contains(filter))
    }
}

fn centrality_matches(
    score: Option<u32>,
    predicate: Option<(QualifiedCentralityComparator, f64)>,
) -> bool {
    let Some((comparator, threshold)) = predicate else {
        return true;
    };
    let score = score.map(|score| f64::from(score) / 1_000_000_000.0);
    match comparator {
        QualifiedCentralityComparator::GreaterThan => score.is_some_and(|value| value > threshold),
        QualifiedCentralityComparator::GreaterThanOrEqual => {
            score.is_some_and(|value| value >= threshold)
        }
        QualifiedCentralityComparator::LessThan => score.is_none_or(|value| value < threshold),
        QualifiedCentralityComparator::LessThanOrEqual => {
            score.is_none_or(|value| value <= threshold)
        }
    }
}

fn matches_graph(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    symbol: &SymbolInput,
    filters: &Filters,
) -> Result<bool, RocksError> {
    for (names, incoming, dependency) in [
        (&filters.callers, false, false),
        (&filters.callees, true, false),
        (&filters.dependencies, false, true),
    ] {
        if !names.is_empty() && !adjacent_name(graph, budget, symbol, names, incoming, dependency)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn adjacent_name(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    symbol: &SymbolInput,
    names: &[String],
    incoming: bool,
    dependency: bool,
) -> Result<bool, RocksError> {
    let direction = if incoming {
        GraphDirection::Incoming
    } else {
        GraphDirection::Outgoing
    };
    let mut cursor = None;
    loop {
        let page = graph.neighbors(&symbol.symbol_id, direction, cursor.as_ref(), 256, budget)?;
        for link in page.links {
            if adjacent_link_matches(graph, budget, &link, names, incoming, dependency)? {
                return Ok(true);
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(false);
        }
    }
}

fn adjacent_link_matches(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    link: &GraphLink,
    names: &[String],
    incoming: bool,
    dependency: bool,
) -> Result<bool, RocksError> {
    if !name_relationship(link.kind(), dependency) {
        return Ok(false);
    }
    let id = if incoming {
        link.source_symbol_id()
    } else {
        link.target_symbol_id()
    };
    let adjacent = graph.symbol(id, budget)?.ok_or(RocksError::Format)?;
    Ok(matches_adjacent_name(&adjacent, names))
}

fn name_relationship(kind: EdgeKind, dependency: bool) -> bool {
    kind == EdgeKind::Calls
        || (dependency
            && matches!(
                kind,
                EdgeKind::Imports
                    | EdgeKind::Extends
                    | EdgeKind::Implements
                    | EdgeKind::TypeOf
                    | EdgeKind::Returns
                    | EdgeKind::Instantiates
                    | EdgeKind::Overrides
            ))
}

fn matches_adjacent_name(symbol: &SymbolInput, names: &[String]) -> bool {
    names
        .iter()
        .any(|name| name == &symbol.qualified_name || name == simple_name(&symbol.qualified_name))
}

fn compare(a: &QualifiedSymbolHit, b: &QualifiedSymbolHit, sort: QualifiedSymbolSort) -> Ordering {
    let centrality = if sort == QualifiedSymbolSort::Centrality {
        b.centrality
            .partial_cmp(&a.centrality)
            .unwrap_or(Ordering::Equal)
    } else {
        Ordering::Equal
    };
    centrality
        .then(simple_name(a.symbol.qualified_name()).cmp(simple_name(b.symbol.qualified_name())))
        .then(a.symbol.path().cmp(b.symbol.path()))
        .then(a.symbol.start_line().cmp(&b.symbol.start_line()))
        .then(a.symbol.symbol_id().cmp(b.symbol.symbol_id()))
}
