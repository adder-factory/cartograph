use cartograph_indexer::ScipOverlayInput;
use cartograph_scip::{
    ScipDocument, ScipIndex, ScipOccurrence, ScipSymbolInformation, encode_scip_index,
};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit PostgreSQL 18 + pinned ParadeDB test database"]
async fn scip_spill_append_failure_rolls_back_and_preserves_current() {
    let directory =
        tempfile::tempdir().unwrap_or_else(|error| panic!("SCIP fault fixture: {error}"));
    write_cache_probe_project(directory.path(), CACHE_PROBE_ORIGINAL);
    let fixture = open_fixture().await;
    publish_cache_probe(&fixture, directory.path()).await;
    install_overlay_fault(&fixture).await;
    for fail in [true, false] {
        let before = current(&fixture).await;
        let staged = begin_generation(&fixture).await;
        let generation_id = staged.generation_id().clone();
        let supervisor =
            IndexerSupervisor::new(fixture.database.clone(), spill_parity_supervisor_config());
        let source = open_parity_source(directory.path(), "SCIP fault");
        let result = supervisor
            .run(
                request_with_duration(
                    target(&fixture.project, &generation_id),
                    SPILL_PARITY_LEASE_DURATION,
                ),
                move |context| async move {
                    let spill = context
                        .generation_spill(&staged, NativeGenerationSpillPolicy::default())
                        .map_err(|_| PipelineFailure::new(PipelineStage::Parse))?;
                    let native =
                        build_native_generation_spilled(&context.stages(), build(source), spill)
                            .await
                            .map_err(|error| native_pipeline_failure(&error))?;
                    assert_eq!(
                        native
                            .report()
                            .scip_overlay()
                            .map(cartograph_scip::ScipOverlayReport::imported_symbols),
                        Some(1)
                    );
                    let (digest, _) = native.into_parts();
                    context
                        .progress()
                        .begin_stage(PipelineStage::Copy)
                        .await
                        .map_err(|_| PipelineFailure::new(PipelineStage::Copy))?;
                    context
                        .prepare_spilled_generation(SpilledGenerationContents::new(staged, digest))
                        .await
                        .map_err(|_| PipelineFailure::new(PipelineStage::Copy))
                },
            )
            .await;
        assert_eq!(result.is_err(), fail, "SCIP append result: {result:?}");
        let after = current(&fixture).await;
        if fail {
            assert_eq!(after, before);
            assert_generation_state(&fixture, &generation_id, GenerationState::Failed).await;
            let observed = format!(
                r#"SELECT is_called FROM "{}"."scip_fault_observed""#,
                fixture.schema
            );
            let row = query(AssertSqlSafe(observed))
                .fetch_one(&fixture.pool)
                .await
                .unwrap_or_else(|error| panic!("SCIP fault observation: {error}"));
            assert!(
                row.try_get::<bool, _>(0).unwrap_or(false),
                "overlay row must reach the injected failure"
            );
            let drop_trigger = format!(
                r#"DROP TRIGGER fail_scip_overlay ON "{}"."native_generation_spill_symbols""#,
                fixture.schema
            );
            query(AssertSqlSafe(drop_trigger))
                .execute(&fixture.pool)
                .await
                .unwrap_or_else(|error| panic!("SCIP fault removal: {error}"));
        } else {
            assert_eq!(after, generation_id);
        }
        assert_spill_work_was_collected(&fixture, &generation_id).await;
    }
    fixture.close().await;
}

async fn current(fixture: &DatabaseFixture) -> GenerationId {
    fixture
        .database
        .project_snapshot_by_root(&format!("workspace/supervisor/{}", fixture.schema))
        .await
        .unwrap_or_else(|error| panic!("SCIP current snapshot: {error}"))
        .and_then(|snapshot| snapshot.current)
        .unwrap_or_else(|| panic!("SCIP current missing"))
        .generation_id
}

async fn install_overlay_fault(fixture: &DatabaseFixture) {
    for statement in [
        format!(
            r#"CREATE SEQUENCE "{}"."scip_fault_observed""#,
            fixture.schema
        ),
        format!(
            r#"CREATE FUNCTION "{schema}"."fail_scip_overlay"() RETURNS trigger LANGUAGE plpgsql AS $body$
            BEGIN
                IF NEW.qualified_name = 'overlay_probe' THEN
                    PERFORM nextval('"{schema}"."scip_fault_observed"');
                    RAISE EXCEPTION 'forced isolated SCIP overlay append failure';
                END IF;
                RETURN NEW;
            END $body$"#,
            schema = fixture.schema
        ),
        format!(
            r#"CREATE TRIGGER fail_scip_overlay BEFORE INSERT ON "{schema}"."native_generation_spill_symbols"
            FOR EACH ROW EXECUTE FUNCTION "{schema}"."fail_scip_overlay"()"#,
            schema = fixture.schema
        ),
    ] {
        query(AssertSqlSafe(statement))
            .execute(&fixture.pool)
            .await
            .unwrap_or_else(|error| panic!("SCIP fault install: {error}"));
    }
}

fn build(source: SourceRoot) -> NativeGenerationBuild {
    let key = "scip-rust cargo replay 1 overlay_probe().";
    let index = ScipIndex {
        tool_name: "spill-fault-fixture".to_owned(),
        tool_version: "1".to_owned(),
        project_root: String::new(),
        documents: vec![ScipDocument {
            relative_path: "src/cache_probe.rs".to_owned(),
            language: "rust".to_owned(),
            symbols: vec![ScipSymbolInformation {
                symbol: key.to_owned(),
                display_name: "overlay_probe".to_owned(),
                kind: 17,
                documentation: Vec::new(),
                relationships: Vec::new(),
                enclosing_symbol: String::new(),
                cartograph_edges: Vec::new(),
            }],
            occurrences: vec![ScipOccurrence {
                symbol: key.to_owned(),
                symbol_roles: 1,
                range: vec![0, 7, 19],
                enclosing_range: vec![0, 0, 32],
            }],
        }],
    };
    let bytes =
        encode_scip_index(&index).unwrap_or_else(|error| panic!("SCIP fault encode: {error}"));
    NativeGenerationBuild::new(source, native_spill_pipeline_config()).with_scip_overlay(
        ScipOverlayInput::new(bytes, 100)
            .unwrap_or_else(|error| panic!("SCIP fault input: {error}")),
    )
}
