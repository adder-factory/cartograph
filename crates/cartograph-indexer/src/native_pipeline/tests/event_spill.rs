//! Real extraction and both resolution branches retain identical event calls.

use super::*;

const FIXTURES: &[(&str, &str)] = &[
    (
        "ios/Emitter.m",
        "@implementation Emitter\n- (void)publish { [self sendEventWithName:@\"ready\" body:nil]; }\n- (void)unmatched { [self sendEventWithName:@\"missing\" body:nil]; }\n@end",
    ),
    ("src/handlers.ts", "export function onReady() {}"),
    (
        "src/a.ts",
        "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); import {onReady} from './handlers'; emitter.on('ready', onReady); emitter.on('ready', onReady);",
    ),
    (
        "src/b.ts",
        "import {NativeEventEmitter} from 'react-native'; const emitter = new NativeEventEmitter(); import {onReady} from './handlers'; emitter.once('ready', onReady); emitter.addListener('ready', onReady); emitter.on('unmatched', onReady);",
    ),
];

fn accumulator() -> NativeFactAccumulator {
    let mut facts = NativeFactAccumulator::new(TEST_GENERATION_BYTES);
    let limits = SourceLimits::new(TEST_SOURCE_BYTES)
        .unwrap_or_else(|error| panic!("event source limits: {error}"));
    for (path, source) in FIXTURES {
        let snapshot =
            SourceSnapshot::from_bytes_for_capability_validation(path, source.as_bytes(), limits)
                .unwrap_or_else(|error| panic!("event source: {error}"));
        let file = NativeExtractor::new_for_capability_validation(snapshot.language())
            .and_then(|mut extractor| extractor.extract(&snapshot))
            .unwrap_or_else(|error| panic!("event extraction: {error}"));
        facts
            .push(file)
            .unwrap_or_else(|_| panic!("event extraction capacity"));
    }
    facts
}

fn append_facts(target: &mut GenerationFacts, mut source: GenerationFacts) {
    target.files.append(&mut source.files);
    target.symbols.append(&mut source.symbols);
    target.edges.append(&mut source.edges);
    target.references.append(&mut source.references);
    target.numerical_sites.append(&mut source.numerical_sites);
    target.documents.append(&mut source.documents);
}

fn split_resolution(
    root: &std::path::Path,
    config: NativePipelineConfig,
    cancellation: &StageCancellation,
) -> CanonicalGenerationFacts {
    let extracted = accumulator();
    let source_root = SourceRoot::open(root).unwrap_or_else(|error| panic!("event root: {error}"));
    let mut budget = ResolveBudget::new(extracted.retained_bytes, TEST_SCOPE_BYTES)
        .unwrap_or_else(|_| panic!("event budget"));
    let index = build_resolution_index(
        &extracted,
        ResolutionIndexContext {
            source_root: &source_root,
            budget: &mut budget,
            cancelled: &mut || cancellation.is_cancelled(),
        },
    )
    .unwrap_or_else(|_| panic!("event index"));
    let mut facts = GenerationFacts::default();
    let mut handlers = native_event_calls::HandlerIndex::default();
    for (sequence, file) in extracted.files.into_iter().enumerate() {
        let resolved = resolve_file_facts(
            usize_to_u64(sequence),
            file,
            SpilledFileResolution {
                index: &index,
                config,
                cancellation,
            },
        )
        .unwrap_or_else(|_| panic!("event file resolution"));
        if !config.evidence_policy().retention.call_sites {
            assert_eq!(resolved.facts.references, Vec::new());
        }
        handlers
            .merge((resolved.event_handlers, &mut budget), &mut || {
                cancellation.is_cancelled()
            })
            .unwrap_or_else(|_| panic!("event handler merge"));
        append_facts(&mut facts, resolved.facts);
    }
    for kind in SpilledDerivedFactKind::ALL {
        let (derived, _) = derive_spilled_facts(
            &index,
            DerivedFactBound {
                cancellation,
                maximum_bytes: TEST_GENERATION_BYTES,
            },
            SpilledDerivedEvidence {
                kind,
                event_handlers: &handlers,
            },
        )
        .unwrap_or_else(|_| panic!("event spill derivation"));
        append_facts(&mut facts, derived);
    }
    let limits = generation_validation_limits(TEST_GENERATION_BYTES, PipelineStage::Reduce)
        .unwrap_or_else(|error| panic!("event validation limits: {error}"));
    validate_generation_facts(facts, limits, || false)
        .unwrap_or_else(|error| panic!("event canonicalization: {error}"))
        .0
}

async fn split_generation(
    root: &std::path::Path,
    config: NativePipelineConfig,
) -> CanonicalGenerationFacts {
    let (runner, tasks, cancellation) = test_stage_runner(SERIAL_WORKERS, TEST_SCOPE_BYTES).await;
    let deadline = Instant::now() + TEST_TIMEOUT;
    let input = StageEnvelope::new(
        StageItemMeta::new(
            StageSequence::new(0),
            (),
            StageItemBudget::new(TEST_GENERATION_BYTES, 0, deadline),
        ),
        root.to_path_buf(),
    );
    let result = runner
        .execute(StageExecution::new(
            StageRunConfig::new(
                PipelineStage::Resolve,
                StageCapacity::new(SERIAL_WORKERS, 0),
                StageDeadlinePolicy::new(deadline, CLEANUP_GRACE),
            ),
            StageWorkload::new(
                [input],
                move |item: StageWorkItem<(), std::path::PathBuf>| async move {
                    let cancellation = item.cancellation();
                    let (_, (), root) = item.into_parts();
                    Ok::<_, StageItemFailure>(split_resolution(&root, config, &cancellation))
                },
            ),
            StageFold::new(
                None,
                |facts: &mut Option<CanonicalGenerationFacts>,
                 output: StageOutput<(), CanonicalGenerationFacts>| {
                    *facts = Some(output.into_parts().1);
                    Ok(())
                },
            ),
        ))
        .await
        .unwrap_or_else(|error| panic!("event resolution stage: {error}"));
    drop(cancellation);
    let report = tasks
        .close_abort_and_reap(Instant::now() + TEST_TIMEOUT)
        .await;
    assert!(report.all_joined);
    result.unwrap_or_else(|| panic!("missing event generation"))
}

fn assert_event_call(facts: &CanonicalGenerationFacts) {
    let source = capability_symbol(facts, "ios/Emitter.m", "Emitter::publish");
    let target = capability_symbol(facts, "src/handlers.ts", "onReady");
    let calls = facts
        .edges()
        .iter()
        .filter(|edge| edge.provenance == native_event_calls::PROVENANCE)
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].source_symbol_id, source.symbol_id);
    assert_eq!(calls[0].target_symbol_id, target.symbol_id);
    assert_eq!(calls[0].kind, EdgeKind::Calls);
    assert_eq!(
        calls[0].confidence,
        native_bridge_details::CONVENTION_CONFIDENCE
    );
    assert_eq!(calls[0].site_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_and_spill_resolution_keep_deduplicated_event_calls_without_sites() {
    let directory = tempdir().unwrap_or_else(|error| panic!("event directory: {error}"));
    for (path, source) in FIXTURES {
        let target = directory.path().join(path);
        fs::create_dir_all(target.parent().unwrap_or(directory.path()))
            .unwrap_or_else(|error| panic!("event parent: {error}"));
        fs::write(target, source).unwrap_or_else(|error| panic!("event fixture: {error}"));
    }
    for retain_sites in [true, false] {
        let policy = config(SERIAL_WORKERS)
            .with_page_rank(false)
            .with_betweenness(false)
            .with_call_sites(retain_sites);
        let memory = build_with_config(directory.path(), SERIAL_WORKERS, policy).await;
        let spilled = split_generation(directory.path(), policy).await;
        assert_event_call(memory.facts());
        assert_event_call(&spilled);
        assert_eq!(memory.facts().digest(), spilled.digest());
        assert_eq!(memory.facts().edges(), spilled.edges());
        assert_eq!(memory.facts().references(), spilled.references());
        if !retain_sites {
            assert_eq!(spilled.references(), &[]);
        }
    }
}
