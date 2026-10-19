//! Read canonical fixed-point centrality without recomputation or SQL.

use crate::{
    NativeDatabase, StorageError, SymbolBetweennessScore, SymbolPageRankScore, rocks::RocksError,
};
use cartograph_domain::{GenerationId, ProjectId, SymbolId};
use std::{collections::BTreeSet, time::Duration};

impl NativeDatabase {
    /// Read sampled Brandes evidence from the expected complete generation.
    /// # Errors
    /// Rejects oversized symbol sets, missing symbols, and publication changes.
    pub async fn current_symbol_betweenness(
        &self,
        project: &ProjectId,
        expected: &GenerationId,
        symbols: &[SymbolId],
    ) -> Result<Vec<SymbolBetweennessScore>, StorageError> {
        self.native_centrality(
            project,
            expected,
            symbols,
            |symbol, value| SymbolBetweennessScore {
                symbol_id: symbol,
                score: value,
            },
            false,
        )
        .await
    }

    /// Read directed `PageRank` evidence from the expected complete generation.
    /// # Errors
    /// Rejects oversized symbol sets, missing symbols, and publication changes.
    pub async fn current_symbol_pagerank(
        &self,
        project: &ProjectId,
        expected: &GenerationId,
        symbols: &[SymbolId],
    ) -> Result<Vec<SymbolPageRankScore>, StorageError> {
        self.native_centrality(
            project,
            expected,
            symbols,
            |symbol, value| SymbolPageRankScore {
                symbol_id: symbol,
                score: value,
            },
            true,
        )
        .await
    }

    async fn native_centrality<T: Send + 'static>(
        &self,
        project: &ProjectId,
        expected: &GenerationId,
        symbols: &[SymbolId],
        record: fn(SymbolId, Option<f64>) -> T,
        pagerank: bool,
    ) -> Result<Vec<T>, StorageError> {
        self.require_project(project)?;
        if symbols.len() > 500 {
            return Err(StorageError::InvalidInput {
                field: "symbol_ids",
            });
        }
        let symbols = symbols.iter().cloned().collect::<BTreeSet<_>>();
        self.graph(expected, Duration::from_secs(30), move |graph, budget| {
            symbols
                .into_iter()
                .map(|id| {
                    let symbol = graph
                        .symbol(&id, budget)?
                        .ok_or(RocksError::GenerationChanged)?;
                    let score = if pagerank {
                        symbol.pagerank_ppb
                    } else {
                        symbol.betweenness_ppb
                    };
                    Ok(record(
                        id,
                        score.map(|value| f64::from(value) / 1_000_000_000.0),
                    ))
                })
                .collect()
        })
        .await
    }
}
