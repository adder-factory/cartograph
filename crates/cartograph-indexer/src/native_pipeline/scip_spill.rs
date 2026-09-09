use super::{
    FactBatchInput, FileDocumentIdentity, FileRecordInput, GenerationFacts, NativeFileFacts,
    NativeGenerationSpillFactBatch, NativePipelineConfig, NormalizedPath, Peekable, ResolveBudget,
    ResolveGenerationFailure, ScipOverlayInput, ScipOverlayPlan, SourceReadOptions, SourceRoot,
    SpilledFactTransaction, SpilledFileWalk, SpilledNativeFacts, SpilledResolutionState,
    SpilledStageContext, SpilledVisitObservation, StageCancellation, StageItemFailure,
    add_spill_fact_counts, append_spilled_centrality_facts, block_in_place,
    classify_spill_validation_error, generation_facts_are_empty, modeled_file_input_bytes,
    modeled_symbol_input_bytes, native_file_symbol_id, native_file_symbol_input, usize_to_u64,
    visit_spilled_native_files,
};

const MAXIMUM_IMPORT_BATCH_ROWS: usize = 1_024;
const MAXIMUM_IMPORT_BATCH_BYTES: u64 = 16 * 1_024 * 1_024;

struct OverlayBasis {
    facts: GenerationFacts,
    budget: ResolveBudget,
}

impl SpilledStageContext<'_> {
    pub(super) async fn prepare_overlay(
        self,
        source: &SpilledNativeFacts,
        root: &SourceRoot,
        overlay: Option<ScipOverlayInput>,
    ) -> Result<(Option<ScipOverlayPlan>, u64, NativePipelineConfig), ResolveGenerationFailure>
    {
        let context = self;
        let Some(overlay) = overlay else {
            return Ok((None, 0, context.config));
        };
        let maximum = context.config.limits.retained.max_generation_bytes;
        let paths = block_in_place(|| {
            cartograph_scip::scip_overlay_paths(&overlay.bytes, overlay.maximum_rows)
        })
        .map_err(|_| ResolveGenerationFailure::unclassified())?;
        let mut basis = OverlayBasis {
            facts: GenerationFacts::default(),
            budget: ResolveBudget::new(0, maximum)
                .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?,
        };
        visit_spilled_native_files(
            SpilledFileWalk {
                source,
                observation: SpilledVisitObservation {
                    cancellation: context.cancellation,
                    progress: context.progress,
                },
            },
            &mut basis,
            |basis, file| {
                if !paths.contains(&file.file.normalized_path) {
                    return Ok(());
                }
                append_basis(basis, file)
            },
        )
        .await
        .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
        let plan = block_in_place(|| {
            cartograph_scip::prepare_scip_overlay(
                cartograph_scip::ScipOverlayPreparation {
                    facts: &basis.facts,
                    bytes: &overlay.bytes,
                    maximum_rows: overlay.maximum_rows,
                },
                |raw_path| {
                    let path = NormalizedPath::parse(raw_path).ok()?;
                    let snapshot = root
                        .read_with_cancellation(
                            &path,
                            SourceReadOptions::new(context.config.limits.source_limits, || {
                                context.cancellation.is_cancelled()
                            }),
                        )
                        .ok()?;
                    Some(String::from(snapshot.into_source()).into_bytes())
                },
                || context.cancellation.is_cancelled(),
            )
        })
        .map_err(|_| ResolveGenerationFailure::unclassified())?;
        let retained =
            block_in_place(|| plan.retained_bytes(maximum, || context.cancellation.is_cancelled()))
                .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
        let mut config = context.config;
        // The persistent plan shares the existing resolve reservation. Reserve its
        // retained payload before admitting the compact resolver and centrality graph.
        config.limits.retained.max_generation_bytes = maximum
            .checked_sub(retained)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        let high_water = basis
            .budget
            .charged_bytes
            .checked_add(retained)
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        Ok((Some(plan), high_water, config))
    }
}

fn append_basis(basis: &mut OverlayBasis, file: NativeFileFacts) -> Result<(), StageItemFailure> {
    let identity = FileDocumentIdentity {
        file_id: file.file.file_id.clone(),
        path: file.file.normalized_path.clone(),
        language: file.file.language.clone(),
    };
    let file_symbol_id = native_file_symbol_id(&file.file.file_id);
    let file_symbol = native_file_symbol_input(&FileRecordInput {
        file: &file.file,
        identity: &identity,
        file_symbol_id: &file_symbol_id,
        line_count: file.line_count,
        test_search_text: String::new(),
        test_search_truncated: false,
    })?;
    basis.budget.charge(
        file.file
            .byte_size
            .saturating_add(modeled_file_input_bytes(&file.file))
            .saturating_add(modeled_symbol_input_bytes(&file_symbol)),
    )?;
    basis
        .facts
        .files
        .try_reserve(1)
        .map_err(|_| StageItemFailure)?;
    basis
        .facts
        .symbols
        .try_reserve(file.symbols.len().saturating_add(1))
        .map_err(|_| StageItemFailure)?;
    basis.facts.files.push(file.file);
    basis.facts.symbols.push(file_symbol);
    for symbol in file.symbols {
        basis
            .budget
            .charge(modeled_symbol_input_bytes(&symbol.input))?;
        basis.facts.symbols.push(symbol.input);
    }
    Ok(())
}

pub(super) fn filter_native(
    plan: Option<&ScipOverlayPlan>,
    facts: &mut GenerationFacts,
    cancellation: &StageCancellation,
) -> Result<(), ResolveGenerationFailure> {
    if let Some(plan) = plan {
        block_in_place(|| plan.retain_native_facts(facts, || cancellation.is_cancelled()))
            .map_err(|_| ResolveGenerationFailure::unclassified())?;
    }
    Ok(())
}

impl SpilledResolutionState {
    pub(super) async fn append_imported(
        &mut self,
        source: &SpilledNativeFacts,
        mut sequence: u64,
        context: SpilledStageContext<'_>,
    ) -> Result<(), ResolveGenerationFailure> {
        let Some(plan) = self.overlay.take() else {
            return Ok(());
        };
        let (mut imported, _) = plan.into_imported();
        if !context.config.evidence_policy().retention.call_sites {
            imported.references.clear();
        }
        let mut symbols = imported.symbols.into_iter().peekable();
        let mut edges = imported.edges.into_iter().peekable();
        let mut references = imported.references.into_iter().peekable();
        let mut documents = imported.documents.into_iter().peekable();
        let mut transaction = SpilledFactTransaction::new(&source.spill, context.progress);
        loop {
            if context.cancellation.is_cancelled() {
                return Err(ResolveGenerationFailure::unclassified());
            }
            let facts = block_in_place(|| {
                let mut bytes = 0_u64;
                Ok::<_, ResolveGenerationFailure>(GenerationFacts {
                    symbols: take_rows(&mut symbols, &mut bytes)?,
                    edges: take_rows(&mut edges, &mut bytes)?,
                    references: take_rows(&mut references, &mut bytes)?,
                    documents: take_rows(&mut documents, &mut bytes)?,
                    ..GenerationFacts::default()
                })
            })?;
            if generation_facts_are_empty(&facts) {
                break;
            }
            if self.centrality_enabled {
                append_spilled_centrality_facts(
                    &mut self.centrality,
                    &facts,
                    &mut self.centrality_budget,
                )
                .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
            }
            let batch = block_in_place(|| {
                NativeGenerationSpillFactBatch::new(
                    FactBatchInput {
                        sequence,
                        facts,
                        limits: self.validation_limits,
                    },
                    || context.cancellation.is_cancelled(),
                )
            })
            .map_err(classify_spill_validation_error)?;
            add_spill_fact_counts(&mut self.counts, batch.counts())?;
            transaction.push(batch).await?;
            sequence = sequence
                .checked_add(1)
                .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        }
        transaction.flush().await
    }
}

fn take_rows<T: serde::Serialize>(
    rows: &mut Peekable<std::vec::IntoIter<T>>,
    bytes: &mut u64,
) -> Result<Vec<T>, ResolveGenerationFailure> {
    let mut batch = Vec::new();
    while batch.len() < MAXIMUM_IMPORT_BATCH_ROWS {
        let Some(row) = rows.peek() else {
            break;
        };
        let encoded =
            serde_json::to_vec(row).map_err(|_| ResolveGenerationFailure::unclassified())?;
        let next = bytes
            .checked_add(usize_to_u64(encoded.len()))
            .ok_or_else(ResolveGenerationFailure::generation_capacity_exceeded)?;
        if next > MAXIMUM_IMPORT_BATCH_BYTES {
            if *bytes == 0 {
                return Err(ResolveGenerationFailure::generation_capacity_exceeded());
            }
            break;
        }
        *bytes = next;
        batch
            .try_reserve(1)
            .map_err(|_| ResolveGenerationFailure::generation_capacity_exceeded())?;
        batch.push(
            rows.next()
                .ok_or_else(ResolveGenerationFailure::unclassified)?,
        );
    }
    Ok(batch)
}
