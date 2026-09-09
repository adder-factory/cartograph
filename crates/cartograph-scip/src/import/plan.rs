use super::{
    BTreeMap, BTreeSet, DocumentSource, FileId, GenerationFacts, ImportAccumulator, ImportState,
    MAXIMUM_OVERLAY_ROWS, NormalizedPath, SCIP_UNRESOLVED_PROVENANCE, ScipError, ScipOverlayReport,
    SymbolBuildLookups, SymbolId, SymbolKind, build_edges_and_references, build_symbols,
    decode_scip_index, native_symbol_candidates, poll, prepare_documents, usize_to_u64,
};

/// A source-verified replacement plan shared by memory and bounded spill batches.
///
/// Native batches must all pass through `retain_native_facts` before the imported
/// facts are appended exactly once. The plan contains no publication capability.
pub struct ScipOverlayPlan {
    covered: BTreeSet<FileId>,
    removed: BTreeSet<SymbolId>,
    file_symbols: BTreeSet<SymbolId>,
    retained_targets: BTreeSet<SymbolId>,
    imported: GenerationFacts,
    report: ScipOverlayReport,
}

impl ScipOverlayPlan {
    /// Apply replacement rules to one native batch, preserving uncovered facts.
    /// # Errors
    /// Returns an error when cancelled; the caller must discard that batch.
    pub fn retain_native_facts<Cancel>(
        &self,
        facts: &mut GenerationFacts,
        mut cancelled: Cancel,
    ) -> Result<(), ScipError>
    where
        Cancel: FnMut() -> bool,
    {
        retain(&mut facts.symbols, &mut cancelled, |symbol| {
            !self.covered.contains(&symbol.file_id)
                || symbol.symbol_kind == SymbolKind::File.as_str()
        })?;
        retain(&mut facts.documents, &mut cancelled, |document| {
            document
                .file_id
                .as_ref()
                .is_none_or(|id| !self.covered.contains(id))
                || document
                    .symbol_id
                    .as_ref()
                    .is_some_and(|id| self.file_symbols.contains(id))
        })?;
        retain(&mut facts.references, &mut cancelled, |reference| {
            if self.covered.contains(&reference.file_id) {
                return false;
            }
            if reference
                .target_symbol_id
                .as_ref()
                .is_some_and(|id| self.target_removed(id))
            {
                reference.target_symbol_id = None;
                reference.confidence = 0.0;
                reference.resolution_provenance.clear();
                reference
                    .resolution_provenance
                    .push_str(SCIP_UNRESOLVED_PROVENANCE);
            }
            true
        })?;
        for site in &mut facts.numerical_sites {
            poll(&mut cancelled)?;
            if self.covered.contains(&site.file_id)
                && site
                    .owner_symbol_id
                    .as_ref()
                    .is_some_and(|id| self.target_removed(id))
            {
                site.owner_symbol_id = None;
            }
        }
        retain(&mut facts.edges, &mut cancelled, |edge| {
            !self.removed.contains(&edge.source_symbol_id)
                && !self.target_removed(&edge.target_symbol_id)
        })
    }

    fn target_removed(&self, id: &SymbolId) -> bool {
        self.removed.contains(id) && !self.retained_targets.contains(id)
    }

    /// Conservative retained-byte accounting for the imported facts and rule sets.
    /// # Errors
    /// Returns an error if cancelled or if the complete plan exceeds the bound.
    pub fn retained_bytes<Cancel>(
        &self,
        maximum: u64,
        mut cancelled: Cancel,
    ) -> Result<u64, ScipError>
    where
        Cancel: FnMut() -> bool,
    {
        let rules = self
            .covered
            .len()
            .saturating_add(self.removed.len())
            .saturating_add(self.file_symbols.len())
            .saturating_add(self.retained_targets.len());
        // Includes each owned UUID string and a conservative ordered-set node allowance.
        let bytes = usize_to_u64(rules)
            .checked_mul(256)
            .ok_or(ScipError::LimitExceeded)?;
        let remaining = maximum
            .checked_sub(bytes)
            .filter(|value| *value > 0)
            .ok_or(ScipError::LimitExceeded)?;
        let imported = self
            .imported
            .measure_retained_bytes(remaining, &mut cancelled)
            .map_err(|_| ScipError::LimitExceeded)?;
        poll(&mut cancelled)?;
        Ok(bytes.saturating_add(imported.retained_bytes()))
    }

    /// Accounting before the imported payload is consumed.
    #[must_use]
    pub const fn report(&self) -> ScipOverlayReport {
        self.report
    }

    /// Consume the plan after every native batch has been filtered.
    #[must_use]
    pub fn into_imported(self) -> (GenerationFacts, ScipOverlayReport) {
        (self.imported, self.report)
    }
}

fn retain<T, Cancel, Keep>(
    rows: &mut Vec<T>,
    cancelled: &mut Cancel,
    mut keep: Keep,
) -> Result<(), ScipError>
where
    Cancel: FnMut() -> bool,
    Keep: FnMut(&mut T) -> bool,
{
    poll(cancelled)?;
    let mut interrupted = false;
    rows.retain_mut(|row| {
        interrupted |= cancelled();
        interrupted || keep(row)
    });
    if interrupted {
        Err(ScipError::Cancelled)
    } else {
        Ok(())
    }
}

/// Decode a bounded artifact and return only normalized candidate project paths.
/// # Errors
/// Returns an error on malformed input or an exceeded artifact/row bound.
pub fn scip_overlay_paths(
    bytes: &[u8],
    maximum_rows: usize,
) -> Result<BTreeSet<String>, ScipError> {
    Ok(decode_bounded(bytes, maximum_rows)?
        .documents
        .into_iter()
        .filter_map(|document| NormalizedPath::parse(&document.relative_path).ok())
        .map(NormalizedPath::into_string)
        .collect())
}

fn decode_bounded(bytes: &[u8], maximum_rows: usize) -> Result<crate::ScipIndex, ScipError> {
    if maximum_rows == 0 || maximum_rows > MAXIMUM_OVERLAY_ROWS {
        return Err(ScipError::LimitExceeded);
    }
    let index = decode_scip_index(bytes)?;
    let rows = index
        .documents
        .iter()
        .try_fold(index.documents.len(), |total, document| {
            let rows = total
                .checked_add(document.symbols.len())
                .and_then(|value| value.checked_add(document.occurrences.len()))
                .ok_or(ScipError::LimitExceeded)?;
            document.symbols.iter().try_fold(rows, |rows, symbol| {
                rows.checked_add(symbol.relationships.len())
                    .and_then(|value| value.checked_add(symbol.cartograph_edges.len()))
                    .ok_or(ScipError::LimitExceeded)
            })
        })?;
    if rows > maximum_rows {
        return Err(ScipError::LimitExceeded);
    }
    Ok(index)
}

/// Immutable native evidence and bounds used to prepare a SCIP replacement plan.
#[derive(Clone, Copy)]
pub struct ScipOverlayPreparation<'input> {
    /// Native files and symbols whose exact current source will be verified.
    pub facts: &'input GenerationFacts,
    /// Encoded SCIP artifact, bounded before decoding.
    pub bytes: &'input [u8],
    /// Maximum admitted SCIP rows.
    pub maximum_rows: usize,
}

/// Prepare replacement rules using native files/symbols and exact current source bytes.
/// The basis may contain only files named by `scip_overlay_paths`; other relation
/// tables are unnecessary. Covered file symbols must be included in the basis.
/// # Errors
/// Returns an error on cancellation, invalid bounds, or invalid symbol/range identities.
pub fn prepare_scip_overlay<ReadSource, Cancel>(
    input: ScipOverlayPreparation<'_>,
    mut read_source: ReadSource,
    mut cancelled: Cancel,
) -> Result<ScipOverlayPlan, ScipError>
where
    ReadSource: FnMut(&str) -> Option<Vec<u8>>,
    Cancel: FnMut() -> bool,
{
    let ScipOverlayPreparation {
        facts,
        bytes,
        maximum_rows,
    } = input;
    poll(&mut cancelled)?;
    let index = decode_bounded(bytes, maximum_rows)?;
    let file_by_path = facts
        .files
        .iter()
        .map(|file| (file.normalized_path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let file_symbol_by_id = facts
        .symbols
        .iter()
        .filter(|symbol| symbol.symbol_kind == SymbolKind::File.as_str())
        .map(|symbol| (symbol.file_id.clone(), symbol.symbol_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let native_candidates = native_symbol_candidates(facts);
    let (prepared, skipped) = prepare_documents(
        &index.documents,
        &file_by_path,
        &mut DocumentSource {
            read_source: &mut read_source,
            cancelled: &mut cancelled,
        },
    )?;
    let mut imported = ImportAccumulator::new(skipped);
    let mut state = ImportState {
        imported: &mut imported,
        cancelled: &mut cancelled,
    };
    build_symbols(
        &prepared,
        SymbolBuildLookups {
            file_symbol_by_id: &file_symbol_by_id,
            native_candidates: &native_candidates,
        },
        &mut state,
    )?;
    build_edges_and_references(&prepared, &file_symbol_by_id, &mut state)?;
    poll(&mut cancelled)?;
    let covered = prepared
        .iter()
        .map(|document| document.file_id.clone())
        .collect::<BTreeSet<_>>();
    let removed = facts
        .symbols
        .iter()
        .filter(|symbol| {
            covered.contains(&symbol.file_id) && symbol.symbol_kind != SymbolKind::File.as_str()
        })
        .map(|symbol| symbol.symbol_id.clone())
        .collect::<BTreeSet<_>>();
    let file_symbols = file_symbol_by_id.into_values().collect::<BTreeSet<_>>();
    let retained_targets = imported
        .symbols
        .iter()
        .map(|symbol| symbol.symbol_id.clone())
        .chain(file_symbols.iter().cloned())
        .collect();
    imported.report.covered_documents = usize_to_u64(covered.len());
    imported.report.replaced_native_symbols = usize_to_u64(removed.len());
    imported.report.imported_symbols = usize_to_u64(imported.symbols.len());
    imported.report.imported_references = usize_to_u64(imported.references.len());
    Ok(ScipOverlayPlan {
        covered,
        removed,
        file_symbols,
        retained_targets,
        report: imported.report,
        imported: GenerationFacts {
            symbols: imported.symbols,
            documents: imported.documents,
            references: imported.references,
            edges: imported.edges.into_values().collect(),
            ..GenerationFacts::default()
        },
    })
}
