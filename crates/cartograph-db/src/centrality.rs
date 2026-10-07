use std::{collections::HashMap, mem::size_of, thread};

use cartograph_domain::{EdgeKind, GenerationId, ProjectId, SymbolId};
use num_traits::ToPrimitive as _;
use serde::Serialize;
use sqlx_core::{query::query, row::Row, sql_str::AssertSqlSafe};
use thiserror::Error;

use crate::{CartographDatabase, GenerationFacts, StorageError, database::quoted_schema};

const DEFAULT_SAMPLE_COUNT: usize = 200;
const DEFAULT_SEED: u32 = 0xc0de_2026;
const MAXIMUM_WORKERS: usize = 16;
const SCRATCH_MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const SCORE_PARTS_PER_BILLION: f64 = 1_000_000_000.0;
const PATH_COUNT_CEILING: f64 = 1.0e200;
const PAGERANK_DAMPING: f64 = 0.85;
const PAGERANK_ITERATIONS: usize = 40;
const PAGERANK_PARALLEL_EDGE_THRESHOLD: usize = 500_000;
const MULBERRY_INCREMENT: u32 = 0x6d2b_79f5;
const MULBERRY_FIRST_SHIFT: u32 = 15;
const MULBERRY_MIX_SHIFT: u32 = 7;
const MULBERRY_FINAL_SHIFT: u32 = 14;
const MULBERRY_MIX_MULTIPLIER: u32 = 61;

/// Fixed-size evidence describing one sampled Brandes computation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BetweennessReport {
    /// Symbols assigned a normalized score.
    pub nodes_scored: usize,
    /// `calls` and `references` edges admitted to the directed graph.
    pub edges_considered: usize,
    /// Deterministically selected source pivots.
    pub sample_count: usize,
    /// Native threads used for independent source-pivot shards.
    pub workers: usize,
}

/// Fixed-size evidence describing one deterministic `PageRank` computation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRankReport {
    /// Number of nodes scored.
    pub nodes_scored: usize,
    /// Number of edges considered.
    pub edges_considered: usize,
    /// Number of iterations.
    pub iterations: usize,
    /// Number of workers.
    pub workers: usize,
}

/// A bounded centrality computation could not produce trustworthy scores.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum BetweennessError {
    /// The caller cancelled before or immediately after the bounded worker pass.
    #[error("sampled Brandes betweenness was cancelled")]
    Cancelled,
    /// One scoped native worker panicked instead of returning its accumulator.
    #[error("sampled Brandes betweenness worker failed")]
    WorkerFailed,
    /// A graph dimension or normalized score could not be represented safely.
    #[error("sampled Brandes betweenness numeric conversion failed")]
    NumericOverflow,
}

/// A bounded `PageRank` computation could not produce trustworthy scores.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum PageRankError {
    /// The caller cancelled the bounded computation.
    #[error("PageRank computation was cancelled")]
    Cancelled,
    /// One scoped native worker panicked.
    #[error("PageRank worker failed")]
    WorkerFailed,
    /// A graph dimension or normalized score could not be represented safely.
    #[error("PageRank numeric conversion failed")]
    NumericOverflow,
}

/// One persisted structural-bridge score fenced to an immutable generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolBetweennessScore {
    /// Stable structural symbol identity.
    pub symbol_id: SymbolId,
    /// Normalized directed sampled Brandes score, or `None` for legacy rows
    /// that have not yet been recomputed.
    pub score: Option<f64>,
}

/// One persisted structural-importance score fenced to an immutable generation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolPageRankScore {
    /// Stable symbol ID for this record.
    pub symbol_id: SymbolId,
    /// Optional score, when available.
    pub score: Option<f64>,
}

trait CentralityRecord: Sized {
    const COLUMN: &'static str;
    const OPERATION: &'static str;
    const SCORE_FIELD: &'static str;

    fn new(symbol_id: SymbolId, score: Option<f64>) -> Self;
}

impl CentralityRecord for SymbolBetweennessScore {
    const COLUMN: &'static str = "betweenness";
    const OPERATION: &'static str = "symbol-betweenness-read";
    const SCORE_FIELD: &'static str = "betweenness";

    fn new(symbol_id: SymbolId, score: Option<f64>) -> Self {
        Self { symbol_id, score }
    }
}

impl CentralityRecord for SymbolPageRankScore {
    const COLUMN: &'static str = "pagerank";
    const OPERATION: &'static str = "symbol-pagerank-read";
    const SCORE_FIELD: &'static str = "pagerank";

    fn new(symbol_id: SymbolId, score: Option<f64>) -> Self {
        Self { symbol_id, score }
    }
}

impl CartographDatabase {
    /// Read persisted sampled Brandes scores for a small exact symbol set under
    /// the project's current-generation fence.
    /// # Errors
    ///
    /// Returns an error if the symbol set exceeds its bound, the expected
    /// generation is no longer current, or score rows cannot be read or decoded.
    pub async fn current_symbol_betweenness(
        &self,
        project_id: &ProjectId,
        expected_generation_id: &GenerationId,
        symbol_ids: &[SymbolId],
    ) -> Result<Vec<SymbolBetweennessScore>, StorageError> {
        self.current_centrality(project_id, expected_generation_id, symbol_ids)
            .await
    }

    /// Read persisted `PageRank` scores for a small exact current-generation symbol set.
    /// # Errors
    ///
    /// Returns an error if the symbol set exceeds its bound, the expected
    /// generation is no longer current, or score rows cannot be read or decoded.
    pub async fn current_symbol_pagerank(
        &self,
        project_id: &ProjectId,
        expected_generation_id: &GenerationId,
        symbol_ids: &[SymbolId],
    ) -> Result<Vec<SymbolPageRankScore>, StorageError> {
        self.current_centrality(project_id, expected_generation_id, symbol_ids)
            .await
    }

    async fn current_centrality<Record>(
        &self,
        project_id: &ProjectId,
        expected_generation_id: &GenerationId,
        symbol_ids: &[SymbolId],
    ) -> Result<Vec<Record>, StorageError>
    where
        Record: CentralityRecord,
    {
        if symbol_ids.len() > 500 {
            return Err(StorageError::InvalidInput {
                field: "symbol_ids",
            });
        }
        if symbol_ids.is_empty() {
            return Ok(Vec::new());
        }
        let schema = quoted_schema(&self.schema);
        let column = Record::COLUMN;
        let sql = format!(
            r#"SELECT symbols.symbol_id::text, symbols.{column}
                FROM {schema}."projects" AS projects
                JOIN {schema}."symbols" AS symbols
                  ON symbols.project_id = projects.project_id
                 AND symbols.generation_id = projects.current_generation_id
                WHERE projects.project_id = CAST($1 AS uuid)
                  AND projects.current_generation_id = CAST($2 AS uuid)
                  AND symbols.symbol_id = ANY(CAST($3 AS uuid[]))
                ORDER BY symbols.symbol_id"#
        );
        let ids = symbol_ids
            .iter()
            .map(|symbol_id| symbol_id.as_str().to_owned())
            .collect::<Vec<_>>();
        let rows = query(AssertSqlSafe(sql))
            .bind(project_id.as_str())
            .bind(expected_generation_id.as_str())
            .bind(ids)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| StorageError::DatabaseOperation {
                operation: Record::OPERATION,
            })?;
        if rows.len() != symbol_ids.len() {
            return Err(StorageError::CurrentGenerationChanged);
        }
        rows.iter().map(decode_centrality_row::<Record>).collect()
    }
}

/// Decode one score only after the query's complete current-generation fence has passed.
fn decode_centrality_row<Record: CentralityRecord>(
    row: &sqlx_postgres::PgRow,
) -> Result<Record, StorageError> {
    let raw_id = row
        .try_get::<String, _>(0)
        .map_err(|_| StorageError::CorruptStoredValue { field: "symbol_id" })?;
    let symbol_id = SymbolId::parse(&raw_id)
        .map_err(|_| StorageError::CorruptStoredValue { field: "symbol_id" })?;
    let score = row
        .try_get::<Option<f64>, _>(1)
        .map_err(|_| StorageError::CorruptStoredValue {
            field: Record::SCORE_FIELD,
        })?;
    if score.is_some_and(|score| !score.is_finite() || !(0.0..=1.0).contains(&score)) {
        return Err(StorageError::CorruptStoredValue {
            field: Record::SCORE_FIELD,
        });
    }
    Ok(Record::new(symbol_id, score))
}

/// Compute directed `PageRank` over calls and references and attach a
/// generation-local score to every symbol. Target scoring is parallelized only
/// beyond the measured 500k-edge crossover; each target retains a stable
/// in-edge summation order, so worker scheduling cannot change the result.
/// # Errors
///
/// Returns an error if cancellation is requested, a numeric conversion
/// overflows, or a bounded worker fails or panics.
pub fn apply_page_rank<Cancel>(
    facts: &mut GenerationFacts,
    mut cancelled: Cancel,
) -> Result<PageRankReport, PageRankError>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(PageRankError::Cancelled);
    }
    let graph = PageRankGraph::from_facts(facts);
    let node_count = graph.out_degree.len();
    if node_count == 0 {
        return Ok(PageRankReport {
            nodes_scored: 0,
            edges_considered: 0,
            iterations: 0,
            workers: 0,
        });
    }
    let workers = page_rank_worker_count(node_count, graph.sources.len());
    let partitions = page_rank_partitions(&graph, workers);
    let node_count_f64 = node_count.to_f64().ok_or(PageRankError::NumericOverflow)?;
    let mut scores = vec![1.0 / node_count_f64; node_count];
    let baseline = (1.0 - PAGERANK_DAMPING) / node_count_f64;
    for _ in 0..PAGERANK_ITERATIONS {
        if cancelled() {
            return Err(PageRankError::Cancelled);
        }
        let dangling_sum = graph
            .dangling
            .iter()
            .map(|index| scores[*index])
            .sum::<f64>();
        let uniform = baseline + (PAGERANK_DAMPING * dangling_sum / node_count_f64);
        scores = page_rank_step(&PageRankStep {
            graph: &graph,
            partitions: &partitions,
            scores: &scores,
            uniform,
        })?;
    }
    if cancelled() {
        return Err(PageRankError::Cancelled);
    }
    for (symbol, score) in facts.symbols.iter_mut().zip(scores) {
        let normalized = score.clamp(0.0, 1.0);
        symbol.pagerank_ppb = Some(
            (normalized * SCORE_PARTS_PER_BILLION)
                .round()
                .to_u32()
                .ok_or(PageRankError::NumericOverflow)?,
        );
    }
    Ok(PageRankReport {
        nodes_scored: node_count,
        edges_considered: graph.sources.len(),
        iterations: PAGERANK_ITERATIONS,
        workers,
    })
}

struct PageRankGraph {
    in_offsets: Vec<usize>,
    sources: Vec<usize>,
    out_degree: Vec<usize>,
    dangling: Vec<usize>,
}

impl PageRankGraph {
    fn from_facts(facts: &GenerationFacts) -> Self {
        let by_id = facts
            .symbols
            .iter()
            .enumerate()
            .map(|(index, symbol)| (symbol.symbol_id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let admitted = facts
            .edges
            .iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::Calls | EdgeKind::References))
            .filter_map(|edge| {
                Some((
                    *by_id.get(edge.source_symbol_id.as_str())?,
                    *by_id.get(edge.target_symbol_id.as_str())?,
                ))
            })
            .collect::<Vec<_>>();
        let mut in_degree = vec![0_usize; facts.symbols.len()];
        let mut out_degree = vec![0_usize; facts.symbols.len()];
        for (source, target) in &admitted {
            out_degree[*source] = out_degree[*source].saturating_add(1);
            in_degree[*target] = in_degree[*target].saturating_add(1);
        }
        let mut in_offsets = Vec::with_capacity(facts.symbols.len().saturating_add(1));
        in_offsets.push(0_usize);
        for degree in in_degree {
            in_offsets.push(
                in_offsets
                    .last()
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(degree),
            );
        }
        let mut sources = vec![0_usize; admitted.len()];
        let mut cursors = in_offsets[..facts.symbols.len()].to_vec();
        for (source, target) in admitted {
            sources[cursors[target]] = source;
            cursors[target] = cursors[target].saturating_add(1);
        }
        let dangling = out_degree
            .iter()
            .enumerate()
            .filter_map(|(index, degree)| (*degree == 0).then_some(index))
            .collect();
        Self {
            in_offsets,
            sources,
            out_degree,
            dangling,
        }
    }

    fn target_score(
        &self,
        target: usize,
        scores: &[f64],
        uniform: f64,
    ) -> Result<f64, PageRankError> {
        let incoming = (self.in_offsets[target]..self.in_offsets[target + 1])
            .map(|offset| {
                let source = self.sources[offset];
                self.out_degree[source]
                    .to_f64()
                    .map(|degree| scores[source] / degree)
                    .ok_or(PageRankError::NumericOverflow)
            })
            .sum::<Result<f64, PageRankError>>()?;
        Ok(uniform + PAGERANK_DAMPING * incoming)
    }
}

fn page_rank_worker_count(node_count: usize, edge_count: usize) -> usize {
    if edge_count < PAGERANK_PARALLEL_EDGE_THRESHOLD || node_count < 2 {
        return 1;
    }
    let available = thread::available_parallelism().map_or(1, usize::from);
    let bytes_per_target = size_of::<usize>().saturating_add(size_of::<f64>());
    let memory_workers =
        (SCRATCH_MEMORY_BUDGET_BYTES / node_count.saturating_mul(bytes_per_target).max(1)).max(1);
    available
        .min(MAXIMUM_WORKERS)
        .min(memory_workers)
        .min(node_count)
        .max(1)
}

fn page_rank_partitions(graph: &PageRankGraph, workers: usize) -> Vec<Vec<usize>> {
    if workers == 1 {
        return vec![(0..graph.out_degree.len()).collect()];
    }
    let mut targets = (0..graph.out_degree.len()).collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        let left_edges = graph.in_offsets[*left + 1] - graph.in_offsets[*left];
        let right_edges = graph.in_offsets[*right + 1] - graph.in_offsets[*right];
        right_edges.cmp(&left_edges).then_with(|| left.cmp(right))
    });
    let mut partitions = vec![Vec::new(); workers];
    let mut loads = vec![0_usize; workers];
    for target in targets {
        let worker = loads
            .iter()
            .enumerate()
            .min_by_key(|(index, load)| (**load, *index))
            .map(|(index, _)| index)
            .unwrap_or_default();
        loads[worker] =
            loads[worker].saturating_add(graph.in_offsets[target + 1] - graph.in_offsets[target]);
        partitions[worker].push(target);
    }
    partitions
}

struct PageRankStep<'graph> {
    graph: &'graph PageRankGraph,
    partitions: &'graph [Vec<usize>],
    scores: &'graph [f64],
    uniform: f64,
}

fn page_rank_step(step: &PageRankStep<'_>) -> Result<Vec<f64>, PageRankError> {
    let graph = step.graph;
    let partitions = step.partitions;
    let scores = step.scores;
    let uniform = step.uniform;
    if partitions.len() == 1 {
        return (0..graph.out_degree.len())
            .map(|target| graph.target_score(target, scores, uniform))
            .collect();
    }
    thread::scope(|scope| {
        let handles = partitions
            .iter()
            .map(|targets| {
                scope.spawn(move || {
                    targets
                        .iter()
                        .map(|target| graph.target_score(*target, scores, uniform))
                        .collect::<Result<Vec<_>, PageRankError>>()
                })
            })
            .collect::<Vec<_>>();
        let mut next = vec![0.0; graph.out_degree.len()];
        for (targets, handle) in partitions.iter().zip(handles) {
            let values = handle.join().map_err(|_| PageRankError::WorkerFailed)??;
            for (target, value) in targets.iter().zip(values) {
                next[*target] = value;
            }
        }
        Ok(next)
    })
}

/// Compute normalized directed sampled Brandes betweenness in parallel and
/// attach the derived score to every symbol in one generation payload.
///
/// Only `calls` and `references` participate, matching v1's structural-
/// centrality graph. Source sampling is deterministic and independent worker
/// accumulators are reduced in shard order, so a generation never races on
/// shared floating-point state.
/// # Errors
///
/// Returns an error if cancellation is requested, graph-size arithmetic
/// overflows, or a bounded worker fails or panics.
pub fn apply_sampled_betweenness<Cancel>(
    facts: &mut GenerationFacts,
    mut cancelled: Cancel,
) -> Result<BetweennessReport, BetweennessError>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(BetweennessError::Cancelled);
    }
    let graph = BetweennessGraph::from_facts(facts);
    let node_count = graph.out_offsets.len().saturating_sub(1);
    if node_count <= 2 {
        for symbol in &mut facts.symbols {
            symbol.betweenness_ppb = Some(0);
        }
        return Ok(BetweennessReport {
            nodes_scored: node_count,
            edges_considered: graph.targets.len(),
            sample_count: 0,
            workers: 0,
        });
    }
    let sources = sampled_sources(node_count, DEFAULT_SAMPLE_COUNT, DEFAULT_SEED);
    let worker_count = bounded_worker_count(node_count, graph.targets.len(), sources.len());
    let scores = compute_parallel(&graph, &sources, worker_count)?;
    if cancelled() {
        return Err(BetweennessError::Cancelled);
    }
    let sample_count = sources.len();
    let exact = sample_count == node_count;
    let scale = if exact {
        1.0
    } else {
        node_count
            .to_f64()
            .zip(sample_count.to_f64())
            .map(|(nodes, samples)| nodes / samples)
            .ok_or(BetweennessError::NumericOverflow)?
    };
    let divisor = (node_count - 1)
        .to_f64()
        .zip((node_count - 2).to_f64())
        .map(|(left, right)| left * right)
        .ok_or(BetweennessError::NumericOverflow)?;
    for (symbol, raw_score) in facts.symbols.iter_mut().zip(scores) {
        let normalized = ((raw_score * scale) / divisor).clamp(0.0, 1.0);
        let ppb = (normalized * SCORE_PARTS_PER_BILLION)
            .round()
            .to_u32()
            .ok_or(BetweennessError::NumericOverflow)?;
        symbol.betweenness_ppb = Some(ppb);
    }
    Ok(BetweennessReport {
        nodes_scored: node_count,
        edges_considered: graph.targets.len(),
        sample_count,
        workers: worker_count,
    })
}

struct BetweennessGraph {
    out_offsets: Vec<usize>,
    targets: Vec<usize>,
}

impl BetweennessGraph {
    fn from_facts(facts: &GenerationFacts) -> Self {
        let by_id = facts
            .symbols
            .iter()
            .enumerate()
            .map(|(index, symbol)| (symbol.symbol_id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let admitted = facts
            .edges
            .iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::Calls | EdgeKind::References))
            .filter_map(|edge| {
                Some((
                    *by_id.get(edge.source_symbol_id.as_str())?,
                    *by_id.get(edge.target_symbol_id.as_str())?,
                ))
            })
            .collect::<Vec<_>>();
        let mut degrees = vec![0_usize; facts.symbols.len()];
        for (source, _) in &admitted {
            degrees[*source] = degrees[*source].saturating_add(1);
        }
        let mut out_offsets = Vec::<usize>::with_capacity(facts.symbols.len().saturating_add(1));
        out_offsets.push(0);
        for degree in degrees {
            let next = out_offsets
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(degree);
            out_offsets.push(next);
        }
        let mut targets = vec![0_usize; admitted.len()];
        let mut cursors = out_offsets[..facts.symbols.len()].to_vec();
        for (source, target) in admitted {
            let cursor = &mut cursors[source];
            targets[*cursor] = target;
            *cursor = cursor.saturating_add(1);
        }
        Self {
            out_offsets,
            targets,
        }
    }
}

fn bounded_worker_count(node_count: usize, edge_count: usize, source_count: usize) -> usize {
    let available = thread::available_parallelism().map_or(1, usize::from);
    let bytes_per_worker = node_count
        .saturating_mul(size_of::<f64>() * 3 + size_of::<i32>() + size_of::<usize>() * 4)
        .saturating_add(edge_count.saturating_mul(size_of::<usize>() * 2))
        .max(1);
    let memory_workers = (SCRATCH_MEMORY_BUDGET_BYTES / bytes_per_worker).max(1);
    available
        .min(MAXIMUM_WORKERS)
        .min(memory_workers)
        .min(source_count)
        .max(1)
}

fn compute_parallel(
    graph: &BetweennessGraph,
    sources: &[usize],
    workers: usize,
) -> Result<Vec<f64>, BetweennessError> {
    let chunk_size = sources.len().div_ceil(workers);
    thread::scope(|scope| {
        let handles = sources
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || compute_source_shard(graph, chunk)))
            .collect::<Vec<_>>();
        let mut combined = vec![0.0_f64; graph.out_offsets.len().saturating_sub(1)];
        for handle in handles {
            let shard = handle.join().map_err(|_| BetweennessError::WorkerFailed)?;
            for (total, contribution) in combined.iter_mut().zip(shard) {
                *total += contribution;
            }
        }
        Ok(combined)
    })
}

fn compute_source_shard(graph: &BetweennessGraph, sources: &[usize]) -> Vec<f64> {
    let node_count = graph.out_offsets.len().saturating_sub(1);
    let predecessor_capacity = graph.targets.len().max(1);
    let mut scratch = BrandesScratch {
        centrality: vec![0.0; node_count],
        distance: vec![-1; node_count],
        path_count: vec![0.0; node_count],
        dependency: vec![0.0; node_count],
        stack: vec![0; node_count],
        queue: vec![0; node_count],
        predecessor_head: vec![usize::MAX; node_count],
        predecessor_next: vec![usize::MAX; predecessor_capacity],
        predecessor_node: vec![0; predecessor_capacity],
        visited_count: 0,
    };
    for source in sources {
        scratch.accumulate(graph, *source);
    }
    scratch.centrality
}

struct BrandesScratch {
    centrality: Vec<f64>,
    distance: Vec<i32>,
    path_count: Vec<f64>,
    dependency: Vec<f64>,
    stack: Vec<usize>,
    queue: Vec<usize>,
    predecessor_head: Vec<usize>,
    predecessor_next: Vec<usize>,
    predecessor_node: Vec<usize>,
    visited_count: usize,
}

impl BrandesScratch {
    fn accumulate(&mut self, graph: &BetweennessGraph, source: usize) {
        for index in 0..self.visited_count {
            let node = self.queue[index];
            self.distance[node] = -1;
            self.path_count[node] = 0.0;
            self.dependency[node] = 0.0;
            self.predecessor_head[node] = usize::MAX;
        }
        self.distance[source] = 0;
        self.path_count[source] = 1.0;
        let mut predecessor_top = 0;
        let mut stack_top = 0;
        let mut queue_head = 0;
        let mut queue_tail = 1;
        self.queue[0] = source;
        while queue_head < queue_tail {
            let node = self.queue[queue_head];
            queue_head += 1;
            self.stack[stack_top] = node;
            stack_top += 1;
            let next_distance = self.distance[node].saturating_add(1);
            for target_index in graph.out_offsets[node]..graph.out_offsets[node + 1] {
                let target = graph.targets[target_index];
                if self.distance[target] < 0 {
                    self.distance[target] = next_distance;
                    self.queue[queue_tail] = target;
                    queue_tail += 1;
                }
                if self.distance[target] == next_distance {
                    self.path_count[target] =
                        (self.path_count[target] + self.path_count[node]).min(PATH_COUNT_CEILING);
                    self.predecessor_node[predecessor_top] = node;
                    self.predecessor_next[predecessor_top] = self.predecessor_head[target];
                    self.predecessor_head[target] = predecessor_top;
                    predecessor_top += 1;
                }
            }
        }
        while stack_top > 0 {
            stack_top -= 1;
            let node = self.stack[stack_top];
            let coefficient = (1.0 + self.dependency[node]) / self.path_count[node];
            let mut predecessor = self.predecessor_head[node];
            while predecessor != usize::MAX {
                let previous = self.predecessor_node[predecessor];
                self.dependency[previous] += self.path_count[previous] * coefficient;
                predecessor = self.predecessor_next[predecessor];
            }
            if node != source {
                self.centrality[node] += self.dependency[node];
            }
        }
        self.visited_count = queue_tail;
    }
}

fn sampled_sources(node_count: usize, requested: usize, seed: u32) -> Vec<usize> {
    if requested == 0 || requested >= node_count {
        return (0..node_count).collect();
    }
    let mut permutation = (0..node_count).collect::<Vec<_>>();
    let mut rng = Mulberry32(seed);
    for index in 0..requested {
        let remaining = node_count - index;
        let remaining_u64 = u64::try_from(remaining).unwrap_or(u64::MAX);
        let scaled = u128::from(rng.next_u32()) * u128::from(remaining_u64);
        let offset_u64 = u64::try_from(scaled >> u32::BITS).unwrap_or(u64::MAX);
        let offset = usize::try_from(offset_u64)
            .unwrap_or(remaining.saturating_sub(1))
            .min(remaining.saturating_sub(1));
        let selected = index + offset;
        permutation.swap(index, selected.min(node_count - 1));
    }
    permutation.truncate(requested);
    permutation
}

struct Mulberry32(u32);

impl Mulberry32 {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(MULBERRY_INCREMENT);
        let mut value = self.0;
        value = (value ^ (value >> MULBERRY_FIRST_SHIFT)).wrapping_mul(value | 1);
        value ^= value.wrapping_add(
            (value ^ (value >> MULBERRY_MIX_SHIFT)).wrapping_mul(value | MULBERRY_MIX_MULTIPLIER),
        );
        value ^ (value >> MULBERRY_FINAL_SHIFT)
    }
}

#[cfg(test)]
mod tests {
    use cartograph_domain::{ContentDigest, FileId, SymbolId};

    use super::*;
    use crate::{EdgeInput, SymbolInput};

    const TEST_UUID_BYTES: usize = 16;
    const TEST_UUID_LAYOUT: (usize, usize, u8, u8, u8, u8) = (6, 8, 0x0f, 0x80, 0x3f, 0x80);

    #[test]
    fn directed_chain_marks_only_the_structural_bridge() {
        let mut facts = graph_facts(&[(0, 1), (1, 2)]);
        let report = apply_sampled_betweenness(&mut facts, || false)
            .unwrap_or_else(|error| panic!("centrality failed: {error}"));

        assert_eq!(report.nodes_scored, 3);
        assert_eq!(report.edges_considered, 2);
        assert_eq!(facts.symbols[0].betweenness_ppb, Some(0));
        assert_eq!(facts.symbols[1].betweenness_ppb, Some(500_000_000));
        assert_eq!(facts.symbols[2].betweenness_ppb, Some(0));
    }

    #[test]
    fn directed_chain_pagerank_rewards_referenced_structural_destinations() {
        let mut facts = graph_facts(&[(0, 1), (1, 2)]);
        let report = apply_page_rank(&mut facts, || false)
            .unwrap_or_else(|error| panic!("PageRank failed: {error}"));

        assert_eq!(report.nodes_scored, 3);
        assert_eq!(report.edges_considered, 2);
        assert_eq!(report.iterations, PAGERANK_ITERATIONS);
        let scores = facts
            .symbols
            .iter()
            .map(|symbol| symbol.pagerank_ppb.unwrap_or_default())
            .collect::<Vec<_>>();
        assert!(scores[2] > scores[1] && scores[1] > scores[0]);
        let total = scores.into_iter().map(u64::from).sum::<u64>();
        assert!(total.abs_diff(1_000_000_000) <= 3);
    }

    #[test]
    fn containment_edges_do_not_inflate_pagerank() {
        let mut facts = graph_facts(&[(0, 1), (1, 2)]);
        facts.edges.iter_mut().for_each(|edge| {
            edge.kind = EdgeKind::Contains;
        });
        apply_page_rank(&mut facts, || false)
            .unwrap_or_else(|error| panic!("PageRank failed: {error}"));
        let scores = facts
            .symbols
            .iter()
            .map(|symbol| symbol.pagerank_ppb)
            .collect::<Vec<_>>();
        assert_eq!(scores, vec![Some(333_333_333); 3]);
    }

    #[test]
    fn containment_edges_do_not_inflate_structural_centrality() {
        let mut facts = graph_facts(&[(0, 1), (1, 2)]);
        facts.edges[0].kind = EdgeKind::Contains;
        let report = apply_sampled_betweenness(&mut facts, || false)
            .unwrap_or_else(|error| panic!("centrality failed: {error}"));

        assert_eq!(report.edges_considered, 1);
        assert!(
            facts
                .symbols
                .iter()
                .all(|symbol| symbol.betweenness_ppb == Some(0))
        );
    }

    fn graph_facts(edges: &[(usize, usize)]) -> GenerationFacts {
        let symbols = (0..3).map(symbol).collect::<Vec<_>>();
        let edges = edges
            .iter()
            .map(|(source, target)| EdgeInput {
                source_symbol_id: symbols[*source].symbol_id.clone(),
                target_symbol_id: symbols[*target].symbol_id.clone(),
                kind: EdgeKind::Calls,
                confidence: 1.0,
                provenance: "centrality-test".to_owned(),
                site_count: 1,
            })
            .collect();
        GenerationFacts {
            symbols,
            edges,
            ..GenerationFacts::default()
        }
    }

    fn symbol(index: usize) -> SymbolInput {
        let index_u8 = u8::try_from(index)
            .unwrap_or_else(|error| panic!("test symbol index exceeded u8: {error}"));
        let index_u32 = u32::try_from(index)
            .unwrap_or_else(|error| panic!("test symbol index exceeded u32: {error}"));
        let index_u64 = u64::try_from(index)
            .unwrap_or_else(|error| panic!("test symbol index exceeded u64: {error}"));
        SymbolInput {
            symbol_id: SymbolId::from_uuid_v8(id_bytes(index_u8)),
            file_id: FileId::from_uuid_v8(id_bytes(20_u8.saturating_add(index_u8))),
            symbol_kind: "function".to_owned(),
            qualified_name: format!("symbol_{index}"),
            signature: String::new(),
            start_byte: index_u64,
            end_byte: index_u64.saturating_add(1),
            start_line: index_u32.saturating_add(1),
            end_line: index_u32.saturating_add(1),
            structural_digest: ContentDigest::from_bytes([index_u8; 32]),
            visibility: None,
            export: cartograph_domain::SymbolExportFlags::default(),
            execution: cartograph_domain::SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        }
    }

    fn id_bytes(seed: u8) -> [u8; TEST_UUID_BYTES] {
        let (version_byte, variant_byte, version_mask, version_bits, variant_mask, variant_bits) =
            TEST_UUID_LAYOUT;
        let mut bytes = [seed; TEST_UUID_BYTES];
        bytes[version_byte] = (bytes[version_byte] & version_mask) | version_bits;
        bytes[variant_byte] = (bytes[variant_byte] & variant_mask) | variant_bits;
        bytes
    }
}
