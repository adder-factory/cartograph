use super::*;
use crate::{
    AgentArtifactContent, CanonicalGenerationFacts, EmbeddingBatchUpsertInput,
    EmbeddingBatchUpsertRequest, EmbeddingModelRegistration, EmbeddingModelRegistrationInput,
    EmbeddingNormalization, EmbeddingUpsertRow, FileInput, GenerationFacts,
    GenerationValidationLimits, NewAgentArtifact, PendingEmbeddingPageInput,
    PendingEmbeddingPageRequest, SearchDocumentInput, SymbolInput,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, DocumentKind, FileId, FileParseStatus, SymbolExecutionFlags, SymbolExportFlags,
    SymbolId,
};
use std::{error::Error, time::Duration};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
fn digest(byte: u8) -> ContentDigest {
    ContentDigest::from_bytes([byte; 32])
}
fn id(number: u64) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&number.to_be_bytes());
    bytes
}
fn generation(number: u64) -> GenerationId {
    GenerationId::from_uuid_v8(id(number))
}
fn symbol(number: u64) -> SymbolId {
    SymbolId::from_uuid_v8(id(number))
}
fn request() -> TestResult<StorageRequest> {
    Ok(StorageRequest::new(Duration::from_secs(30))?)
}
fn facts(count: u64) -> TestResult<CanonicalGenerationFacts> {
    let file_id = FileId::from_uuid_v8([1; 16]);
    let mut facts = GenerationFacts {
        files: vec![FileInput {
            file_id: file_id.clone(),
            normalized_path: "src/lib.rs".into(),
            language: "rust".into(),
            content_hash: digest(3),
            byte_size: 1,
            parse_status: FileParseStatus::Parsed,
        }],
        ..GenerationFacts::default()
    };
    for number in 1..=count {
        facts.symbols.push(SymbolInput {
            symbol_id: symbol(number),
            file_id: file_id.clone(),
            symbol_kind: "function".into(),
            qualified_name: format!("function_{number}"),
            signature: "fn function()".into(),
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            end_line: 1,
            structural_digest: digest(2),
            visibility: None,
            export: SymbolExportFlags::default(),
            execution: SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        });
        facts.documents.push(SearchDocumentInput {
            document_id: DocumentId::from_uuid_v8(id(number)),
            file_id: Some(file_id.clone()),
            symbol_id: Some(symbol(number)),
            path: "src/lib.rs".into(),
            language: "rust".into(),
            kind: DocumentKind::Symbol,
            qualified_name: format!("function_{number}"),
            code: format!("fn function_{number}() {{}}"),
            natural_text: String::new(),
            metadata: serde_json::json!({}),
        });
    }
    Ok(validate_generation_facts(
        facts,
        GenerationValidationLimits::new(16 * 1_048_576, 64 * 1_048_576)?,
        || false,
    )?
    .0)
}
async fn publish(store: &AsyncProjectStore, number: u64, count: u64) -> TestResult {
    let input = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(number),
            source_revision: digest(9),
            base: if number == 1 {
                GraphBase::Empty
            } else {
                GraphBase::Current(generation(number - 1))
            },
        },
        facts(count)?,
        GraphPolicy::default(),
        &request()?,
    )?;
    store.publish_graph(input, &request()?).await?;
    Ok(())
}
async fn collect_at(
    database: &NativeDatabase,
    seconds: i64,
) -> TestResult<NativeDerivedPruneReport> {
    Ok(database
        .state(None, 4 * 1_048_576, 1024, move |tx, budget| {
            collect(tx, budget, seconds)
        })
        .await?)
}
async fn count(
    database: &NativeDatabase,
    namespace: Ns,
    prefix: &'static [u8],
) -> TestResult<usize> {
    Ok(database
        .state(None, 0, 0, move |tx, budget| {
            let mut cursor = None;
            let mut count = 0;
            loop {
                let page = tx.scan(namespace, prefix, cursor.as_deref(), 64, budget)?;
                count += page.rows.len();
                cursor = page.next_after.map(|key| key.as_bytes().to_vec());
                if cursor.is_none() {
                    return Ok(count);
                }
            }
        })
        .await?)
}

async fn seed_cold_rows(
    database: &NativeDatabase,
    project: &cartograph_domain::ProjectId,
) -> TestResult {
    for number in 1..=131 {
        database
            .create_agent_artifact(
                project,
                &NewAgentArtifact::new(
                    AgentArtifactKind::Summary,
                    AgentArtifactScope::Symbol,
                    AgentArtifactContent::new(symbol(number).to_string(), "source-fenced summary"),
                )?
                .with_generation(generation(1))
                .with_source_digest(digest(2)),
            )
            .await?;
    }
    database
        .create_agent_artifact(
            project,
            &NewAgentArtifact::new(
                AgentArtifactKind::Note,
                AgentArtifactScope::Symbol,
                AgentArtifactContent::new(
                    symbol(2).to_string(),
                    "retain user-authored note after deletion",
                ),
            )?,
        )
        .await?;
    let model = database
        .register_embedding_model(
            EmbeddingModelRegistration::new(EmbeddingModelRegistrationInput {
                model_id: ModelId::from_uuid_v8([1; 16]),
                fingerprint: digest(8),
                provider: "fixture".into(),
                model_name: "fixture".into(),
                dimension: 2,
                normalization: EmbeddingNormalization::None,
            })?,
            Duration::from_secs(30),
        )
        .await?
        .selector()
        .clone();
    loop {
        let page = database
            .pending_current_embedding_documents(PendingEmbeddingPageRequest::new(
                PendingEmbeddingPageInput {
                    project_id: project.clone(),
                    model: model.clone(),
                    maximum_documents: 64,
                    maximum_source_bytes: 1_048_576,
                    statement_timeout: Duration::from_secs(30),
                },
            )?)
            .await?;
        if page.documents().is_empty() {
            break;
        }
        let rows = page
            .documents()
            .iter()
            .map(|document| {
                EmbeddingUpsertRow::new(
                    document.document_id().clone(),
                    document.source_digest().clone(),
                    vec![1.0, 0.0],
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        database
            .upsert_current_document_embeddings(EmbeddingBatchUpsertRequest::new(
                EmbeddingBatchUpsertInput {
                    project_id: project.clone(),
                    generation_id: generation(1),
                    model: model.clone(),
                    rows,
                    statement_timeout: Duration::from_secs(30),
                },
            )?)
            .await?;
    }
    database
        .state(None, 1_048_576, 132, |tx, _| {
            for number in 1..=131 {
                tx.put(
                    Ns::Derived,
                    format!("q/{}/{number}", generation(1)).as_bytes(),
                    b"old-demand".to_vec(),
                )?;
            }
            tx.put(
                Ns::Derived,
                format!("q/{}/current", generation(2)).as_bytes(),
                b"current-demand".to_vec(),
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_derived_rows_drain_in_bounded_pages_across_reopen_without_deleting_live_evidence()
-> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let identity = digest(42);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    let database = NativeDatabase::new(store.clone(), &identity);
    let project = database.project_id().clone();
    publish(&store, 1, 131).await?;
    seed_cold_rows(&database, &project).await?;
    publish(&store, 2, 1).await?;
    let mut removed = 0;
    let mut grace = 0;
    for _ in 0..4 {
        let report = collect_at(&database, 1000).await?;
        assert!(report.scanned <= 192);
        removed += report.queue_rows_removed;
        grace += report.grace_started;
        assert_eq!(report.vectors_removed, 0);
        assert_eq!(report.artifacts_removed, 0);
    }
    assert_eq!(removed, 131);
    assert_eq!(grace, 260);
    assert_eq!(count(&database, Ns::Derived, b"q/").await?, 1);
    assert_eq!(count(&database, Ns::Embeddings, b"v/").await?, 131);
    for _ in 0..4 {
        let report = collect_at(&database, 1599).await?;
        assert_eq!(report.vectors_removed, 0);
        assert_eq!(report.artifacts_removed, 0);
    }
    runtime.shutdown(&request()?).await?;
    let reopened_runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let reopened = NativeDatabase::new(
        AsyncProjectStore::open(
            &reopened_runtime,
            root.path().to_owned(),
            identity.clone(),
            StorePolicy::default(),
            &request()?,
        )
        .await?,
        &identity,
    );
    let mut vectors = 0;
    let mut artifacts = 0;
    for _ in 0..5 {
        let report = collect_at(&reopened, 1600).await?;
        assert!(report.vectors_removed <= 64);
        assert!(report.artifacts_removed <= 64);
        vectors += report.vectors_removed;
        artifacts += report.artifacts_removed;
    }
    assert_eq!(vectors, 130);
    assert_eq!(artifacts, 130);
    assert_eq!(count(&reopened, Ns::Embeddings, b"v/").await?, 1);
    assert_eq!(count(&reopened, Ns::Artifacts, b"a/").await?, 2);
    assert_eq!(count(&reopened, Ns::Metadata, b"gc/orphan/").await?, 0);
    assert_eq!(count(&reopened, Ns::Derived, b"q/").await?, 1);
    assert_eq!(
        reopened
            .embedding_storage_audit(&project, Duration::from_secs(30))
            .await?
            .current_embeddings,
        1
    );
    let stopped = request()?;
    stopped.cancel();
    assert!(
        reopened
            .prune_derived_state_with_request(&stopped)
            .await
            .is_err()
    );
    reopened_runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacing_an_orphan_restarts_its_full_grace_period() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        digest(42),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    publish(&store, 1, 0).await?;
    let database = NativeDatabase::new(store, &digest(42));
    let operation = |value: &'static [u8], timestamp| {
        move |tx: &mut StateTransaction<'_>, budget: &mut ReadBudget| {
            let mut report = NativeDerivedPruneReport::default();
            let removed = cold(
                tx,
                budget,
                OrphanCandidate {
                    family: "fixture",
                    key: b"v/example",
                    bytes: value,
                    incompatible: true,
                },
                timestamp,
                &mut report,
            )?;
            Ok((removed, report.grace_started))
        }
    };
    assert_eq!(
        database
            .state(None, 1024, 4, operation(b"old", 1000))
            .await?,
        (false, 1)
    );
    assert_eq!(
        database
            .state(None, 1024, 4, operation(b"new", 1600))
            .await?,
        (false, 1)
    );
    assert_eq!(
        database
            .state(None, 1024, 4, operation(b"new", 2199))
            .await?,
        (false, 0)
    );
    assert_eq!(
        database
            .state(None, 1024, 4, operation(b"new", 2200))
            .await?,
        (true, 0)
    );
    runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_artifact_deletion_removes_its_orphan_marker_across_reopen() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let identity = digest(42);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    publish(&store, 1, 0).await?;
    let database = NativeDatabase::new(store, &identity);
    let artifact = database
        .create_agent_artifact(
            database.project_id(),
            &NewAgentArtifact::new(
                AgentArtifactKind::Summary,
                AgentArtifactScope::File,
                AgentArtifactContent::new("missing.rs", "orphan summary"),
            )?,
        )
        .await?;
    assert_eq!(collect_at(&database, 1000).await?.grace_started, 1);
    assert_eq!(count(&database, Ns::Metadata, b"gc/orphan/").await?, 1);
    assert!(
        database
            .delete_agent_artifact_by_id(database.project_id(), artifact.id())
            .await?
    );
    assert_eq!(count(&database, Ns::Metadata, b"gc/orphan/").await?, 0);
    runtime.shutdown(&request()?).await?;
    let reopened_runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let reopened = NativeDatabase::new(
        AsyncProjectStore::open(
            &reopened_runtime,
            root.path().to_owned(),
            identity.clone(),
            StorePolicy::default(),
            &request()?,
        )
        .await?,
        &identity,
    );
    assert_eq!(collect_at(&reopened, 2000).await?.artifacts_removed, 0);
    assert_eq!(count(&reopened, Ns::Metadata, b"gc/orphan/").await?, 0);
    reopened_runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_artifact_identity_cannot_delete_a_different_user_note() -> TestResult {
    let root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let identity = digest(42);
    let store = AsyncProjectStore::open(
        &runtime,
        root.path().to_owned(),
        identity.clone(),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    publish(&store, 1, 0).await?;
    let database = NativeDatabase::new(store, &identity);
    let summary = database
        .create_agent_artifact(
            database.project_id(),
            &NewAgentArtifact::new(
                AgentArtifactKind::Summary,
                AgentArtifactScope::File,
                AgentArtifactContent::new("missing.rs", "cold summary"),
            )?,
        )
        .await?;
    let note = database
        .create_agent_artifact(
            database.project_id(),
            &NewAgentArtifact::new(
                AgentArtifactKind::Note,
                AgentArtifactScope::Project,
                AgentArtifactContent::new("project", "precious user note"),
            )?,
        )
        .await?;
    let mut forged = serde_json::to_value(&summary)?;
    forged["id"] = serde_json::json!(note.id());
    let key = format!("a/{:016x}", summary.id());
    let forged = serde_json::to_vec(&forged)?;
    database
        .state(None, 1_048_576, 4, move |tx, _| {
            tx.put(Ns::Artifacts, key.as_bytes(), forged)
        })
        .await?;
    assert!(collect_at(&database, 1000).await.is_err());
    assert!(collect_at(&database, 2000).await.is_err());
    assert_eq!(count(&database, Ns::Artifacts, b"a/").await?, 2);
    assert_eq!(count(&database, Ns::Metadata, b"gc/orphan/").await?, 0);
    assert!(
        database
            .delete_agent_artifact_by_id(database.project_id(), note.id())
            .await?
    );
    runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_native_request_reaches_blocked_feature_work_and_prevents_late_commit() -> TestResult
{
    for deadline in [false, true] {
        let root = tempfile::tempdir()?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let identity = digest(42);
        let database = NativeDatabase::new(
            AsyncProjectStore::open(
                &runtime,
                root.path().to_owned(),
                identity.clone(),
                StorePolicy::default(),
                &request()?,
            )
            .await?,
            &identity,
        );
        let control = StorageRequest::new(if deadline {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(10)
        })?;
        let operation_control = control.clone();
        let operation_database = database.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let work = tokio::spawn(async move {
            NativeDatabase::with_request(
                operation_control,
                operation_database.state_with_timeout(
                    None,
                    1024,
                    4,
                    Duration::from_secs(60),
                    move |tx, budget| {
                        tx.put(Ns::Metadata, b"late-commit", b"must roll back".to_vec())?;
                        let _ = entered.send(());
                        released.recv_timeout(Duration::from_secs(5)).map_err(|_| {
                            RocksError::Operation {
                                operation: "fixture-gate",
                            }
                        })?;
                        budget.checkpoint()?;
                        Ok(())
                    },
                ),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started).await??;
        if deadline {
            tokio::time::sleep(Duration::from_millis(150)).await;
        } else {
            control.cancel();
        }
        release.send(())?;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), work)
                .await??
                .is_err()
        );
        let visible = database
            .state(None, 0, 0, |tx, budget| {
                tx.get(Ns::Metadata, b"late-commit", budget)
            })
            .await?;
        assert!(visible.is_none());
        runtime.shutdown(&request()?).await?;
    }
    Ok(())
}
