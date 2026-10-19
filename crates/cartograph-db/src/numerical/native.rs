//! Numerical source evidence remains scoped to one immutable native graph.

use super::{
    CurrentGenerationLookup, DEFAULT_NUMERICAL_READ_TIMEOUT, NormalizedPath, NumericalSitePage,
    NumericalSiteQuery, NumericalSiteRecord, NumericalSiteStats, StorageError,
};
use crate::{
    FileInput, NativeDatabase, NumericalSiteInput,
    retrieval::native::{in_directory, retain, visit_files},
    rocks::{GraphSnapshot, ReadBudget, RocksError},
};
use std::collections::{BTreeMap, BTreeSet};

impl NativeDatabase {
    /// Query numerical sites after applying every hard filter to the full generation.
    /// # Errors
    /// Rejects a different project, changed publication, or exhausted read budgets.
    pub async fn current_numerical_sites(
        &self,
        lookup: CurrentGenerationLookup<'_>,
        request: &NumericalSiteQuery,
    ) -> Result<NumericalSitePage, StorageError> {
        self.require_project(lookup.project_id())?;
        let generation = lookup.expected_generation_id().clone();
        let request = request.clone();
        self.graph(
            lookup.expected_generation_id(),
            DEFAULT_NUMERICAL_READ_TIMEOUT,
            move |graph, budget| {
                let mut sites = Vec::new();
                let mut total = 0_u64;
                visit_sites(graph, budget, |site, budget| {
                    if !matches_site(&site, &request) {
                        return Ok(());
                    }
                    let file = graph
                        .file(&site.file_id, budget)?
                        .ok_or(RocksError::Format)?;
                    if request
                        .path_prefix
                        .as_ref()
                        .is_some_and(|path| !in_directory(&file.normalized_path, path.as_str()))
                    {
                        return Ok(());
                    }
                    total = total.checked_add(1).ok_or(RocksError::Format)?;
                    let owner = site
                        .owner_symbol_id
                        .as_ref()
                        .map(|id| graph.symbol(id, budget))
                        .transpose()?
                        .flatten();
                    let row = NumericalSiteRecord {
                        generation_id: generation.clone(),
                        numerical_site_id: site.site_id,
                        file_id: site.file_id,
                        path: NormalizedPath::parse(&file.normalized_path)
                            .map_err(|_| RocksError::Format)?,
                        language: file.language,
                        owner_symbol_id: site.owner_symbol_id,
                        owner_qualified_name: owner.map(|owner| owner.qualified_name),
                        start_byte: site.start_byte,
                        end_byte: site.end_byte,
                        start_line: site.start_line,
                        end_line: site.end_line,
                        operation: site.operation,
                        hazard: site.hazard,
                        precision: site.precision,
                        expression_digest: site.expression_digest,
                        confidence_ppm: site.confidence_ppm,
                        provenance: site.provenance,
                        evidence_level: site.evidence_level,
                        unknowns: site
                            .unknowns
                            .split(',')
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    };
                    retain(&mut sites, row, usize::from(request.limit), |a, b| {
                        (a.hazard == "none_observed")
                            .cmp(&(b.hazard == "none_observed"))
                            .then(b.confidence_ppm.cmp(&a.confidence_ppm))
                            .then(a.path.cmp(&b.path))
                            .then(a.start_line.cmp(&b.start_line))
                            .then(a.start_byte.cmp(&b.start_byte))
                            .then(a.numerical_site_id.cmp(&b.numerical_site_id))
                    });
                    Ok(())
                })?;
                Ok(NumericalSitePage {
                    generation_id: generation,
                    total,
                    truncated: total > sites.len() as u64,
                    sites,
                })
            },
        )
        .await
    }

    /// Aggregate numerical coverage and bounded category breakdowns.
    /// # Errors
    /// Rejects changed publication, excessive grouping workspace, or failed reads.
    pub async fn current_numerical_site_stats(
        &self,
        lookup: CurrentGenerationLookup<'_>,
    ) -> Result<NumericalSiteStats, StorageError> {
        self.require_project(lookup.project_id())?;
        let generation = lookup.expected_generation_id().clone();
        self.graph(
            lookup.expected_generation_id(),
            DEFAULT_NUMERICAL_READ_TIMEOUT,
            move |graph, budget| {
                let mut stats = empty_stats(generation);
                let (mut files, mut symbols) = (BTreeSet::new(), BTreeSet::new());
                visit_files(graph, budget, |file, _| {
                    record_file_coverage(&mut stats, &file);
                    Ok(())
                })?;
                visit_sites(graph, budget, |site, budget| {
                    stats.total_sites += 1;
                    stats.hazard_sites += u64::from(site.hazard != "none_observed");
                    stats.unknown_precision_sites += u64::from(site.precision == "unknown");
                    stats.sites_with_unknowns += u64::from(!site.unknowns.is_empty());
                    files.insert(site.file_id.clone());
                    if let Some(owner) = site.owner_symbol_id {
                        symbols.insert(owner);
                    }
                    let file = graph
                        .file(&site.file_id, budget)?
                        .ok_or(RocksError::Format)?;
                    for (map, key) in [
                        (&mut stats.by_operation, site.operation),
                        (&mut stats.by_hazard, site.hazard),
                        (&mut stats.by_precision, site.precision),
                        (&mut stats.by_evidence_level, site.evidence_level),
                        (&mut stats.by_provenance, site.provenance),
                        (&mut stats.by_language, file.language),
                    ] {
                        count(map, key)?;
                    }
                    Ok(())
                })?;
                stats.files_with_sites = files.len() as u64;
                stats.symbols_with_sites = symbols.len() as u64;
                for map in [
                    &mut stats.by_operation,
                    &mut stats.by_hazard,
                    &mut stats.by_precision,
                    &mut stats.by_evidence_level,
                    &mut stats.by_provenance,
                    &mut stats.by_language,
                ] {
                    if map.len() > 64 {
                        stats.breakdowns_truncated = true;
                        let mut values = std::mem::take(map).into_iter().collect::<Vec<_>>();
                        values.sort_by(|(a_key, a), (b_key, b)| b.cmp(a).then(a_key.cmp(b_key)));
                        values.truncate(64);
                        *map = values.into_iter().collect();
                    }
                }
                Ok(stats)
            },
        )
        .await
    }
}

fn record_file_coverage(stats: &mut NumericalSiteStats, file: &FileInput) {
    if file.language != "rust" {
        return;
    }
    stats.supported_files += 1;
    if matches!(
        file.parse_status,
        cartograph_domain::FileParseStatus::Parsed | cartograph_domain::FileParseStatus::Partial
    ) {
        stats.analyzed_files += 1;
    } else {
        stats.unanalyzed_files += 1;
    }
}

fn visit_sites(
    graph: &GraphSnapshot<'_, '_>,
    budget: &mut ReadBudget,
    mut visit: impl FnMut(NumericalSiteInput, &mut ReadBudget) -> Result<(), RocksError>,
) -> Result<(), RocksError> {
    let mut cursor = None;
    loop {
        let (rows, next) = graph.numerical_records(cursor.as_ref(), 256, budget)?;
        for row in rows {
            visit(row, budget)?;
        }
        cursor = next;
        if cursor.is_none() {
            return Ok(());
        }
    }
}

fn matches_site(site: &NumericalSiteInput, request: &NumericalSiteQuery) -> bool {
    request
        .owner_symbol_id
        .as_ref()
        .is_none_or(|id| site.owner_symbol_id.as_ref() == Some(id))
        && request
            .operation
            .as_ref()
            .is_none_or(|value| value == &site.operation)
        && request
            .hazard
            .as_ref()
            .is_none_or(|value| value == &site.hazard)
        && request
            .precision
            .as_ref()
            .is_none_or(|value| value == &site.precision)
        && request
            .evidence_level
            .as_ref()
            .is_none_or(|value| value == &site.evidence_level)
        && site.confidence_ppm >= request.minimum_confidence_ppm
        && (!request.hazards_only || site.hazard != "none_observed")
}

fn empty_stats(generation: cartograph_domain::GenerationId) -> NumericalSiteStats {
    NumericalSiteStats {
        generation_id: generation,
        total_sites: 0,
        hazard_sites: 0,
        unknown_precision_sites: 0,
        sites_with_unknowns: 0,
        supported_files: 0,
        analyzed_files: 0,
        unanalyzed_files: 0,
        files_with_sites: 0,
        symbols_with_sites: 0,
        by_operation: BTreeMap::new(),
        by_hazard: BTreeMap::new(),
        by_precision: BTreeMap::new(),
        by_evidence_level: BTreeMap::new(),
        by_provenance: BTreeMap::new(),
        by_language: BTreeMap::new(),
        breakdowns_truncated: false,
    }
}

fn count(map: &mut BTreeMap<String, u64>, key: String) -> Result<(), RocksError> {
    if map.len() >= 8192 && !map.contains_key(&key) {
        return Err(RocksError::Limit {
            resource: "numerical_breakdown_groups",
        });
    }
    let count = map.entry(key).or_default();
    *count = count.checked_add(1).ok_or(RocksError::Format)?;
    Ok(())
}
