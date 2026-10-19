use super::*;
use crate::{
    AgentArtifactQuery, GenerationFacts, GenerationValidationLimits, SummarySaveInput,
    SymbolSummarySaveInput,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};
use cartograph_domain::{
    FileId, FileParseStatus, NormalizedPath, SymbolExecutionFlags, SymbolExportFlags, SymbolId,
};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
fn digest(value: u8) -> ContentDigest {
    ContentDigest::from_bytes([value; 32])
}
fn symbol(value: u64) -> SymbolId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&value.to_be_bytes());
    SymbolId::from_uuid_v8(bytes)
}
fn request() -> TestResult<StorageRequest> {
    Ok(StorageRequest::new(Duration::from_secs(30))?)
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: StorageRuntime,
    database: NativeDatabase,
}
impl Fixture {
    async fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let store = AsyncProjectStore::open(
            &runtime,
            root.path().to_owned(),
            digest(1),
            StorePolicy::default(),
            &request()?,
        )
        .await?;
        let files = (1_u8..=5)
            .map(|id| FileInput {
                file_id: FileId::from_uuid_v8([id; 16]),
                normalized_path: format!("src/file_{id}.rs"),
                language: "rust".into(),
                content_hash: digest(2),
                byte_size: 1,
                parse_status: FileParseStatus::Parsed,
            })
            .collect::<Vec<_>>();
        let symbols = (1..=2004_u64)
            .map(|id| SymbolInput {
                symbol_id: symbol(id),
                file_id: files[if id <= 4 {
                    usize::try_from(id - 1).unwrap_or(0)
                } else {
                    4
                }]
                .file_id
                .clone(),
                symbol_kind: "function".into(),
                qualified_name: format!("function_{id}"),
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
            })
            .collect();
        let (facts, _) = validate_generation_facts(
            GenerationFacts {
                files,
                symbols,
                ..GenerationFacts::default()
            },
            GenerationValidationLimits::new(64 * 1_048_576, 128 * 1_048_576)?,
            || false,
        )?;
        store
            .publish_graph(
                GraphPublicationInput::new(
                    GraphRevision {
                        generation_id: GenerationId::from_uuid_v8([1; 16]),
                        source_revision: digest(9),
                        base: GraphBase::Empty,
                    },
                    facts,
                    GraphPolicy::default(),
                    &request()?,
                )?,
                &request()?,
            )
            .await?;
        let database = NativeDatabase::new(store, &digest(1));
        for id in 1..=4 {
            database
                .save_symbol_summary(
                    SymbolSummarySaveInput::new(database.project_id(), &symbol(id), &digest(2))
                        .with_summary("Verified symbol behavior.")
                        .with_model("fixture-judge"),
                )
                .await?;
        }
        Ok(Self {
            _root: root,
            runtime,
            database,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_batch_scans_unrelated_symbols_once_and_preserves_order_and_quality() -> TestResult
{
    let fixture = Fixture::new().await?;
    let db = &fixture.database;
    let anchor = digest(7);
    let pending = db
        .pending_file_summaries(
            PendingSummaryRollupQuery::new(db.project_id(), "structural:v2", &anchor).page(10, 2),
        )
        .await?;
    assert_eq!(pending.len(), 4);
    let paths = pending
        .iter()
        .map(|item| NormalizedPath::parse(item.path()))
        .collect::<Result<Vec<_>, _>>()?;
    let digests = pending
        .iter()
        .map(|item| ContentDigest::parse(item.content_hash()))
        .collect::<Result<Vec<_>, _>>()?;
    db.save_file_summary(
        db.project_id(),
        FileSummarySaveRequest::new(
            &paths[0],
            SummarySaveInput::new(&digests[0], &anchor, "Higher quality.")
                .with_model("judge")
                .with_item_limit(2),
        )?,
    )
    .await?;
    let inputs = [3, 0, 2, 1].map(|index| {
        (
            paths[index].as_str(),
            SummarySaveFields::new(
                SummarySaveInput::new(&digests[index], &anchor, "Structural paragraph.")
                    .with_model("structural:v2")
                    .with_item_limit(2),
                50,
            ),
        )
    });
    let mut owned = owned_saves(
        inputs
            .into_iter()
            .map(|(scope, fields)| Ok((scope, fields?)))
            .collect::<Result<Vec<_>, StorageError>>()?,
    )?;
    for item in &mut owned {
        item.mode = "structural_rule";
    }
    let singles = owned.clone();
    let repeated = db
        .store
        .state(
            None,
            ReadBudget::new(6_000, 16 * 1_048_576, Duration::from_secs(30))?,
            WRITE_BYTES,
            4 * WRITE_ROWS,
            &request()?,
            move |transaction, budget| {
                for item in singles {
                    save_batch(transaction, budget, vec![item], false)?;
                }
                Ok(())
            },
        )
        .await;
    assert!(matches!(
        repeated,
        Err(RocksError::Limit {
            resource: "read_rows"
        })
    ));
    // One streaming graph scan fits; repeating it for each scope exceeds these bounds.
    let records = db
        .store
        .state(
            None,
            ReadBudget::new(6_000, 16 * 1_048_576, Duration::from_secs(30))?,
            WRITE_BYTES,
            4 * WRITE_ROWS,
            &request()?,
            move |transaction, budget| save_batch(transaction, budget, owned, false),
        )
        .await?;
    let scopes = records
        .iter()
        .map(|record| record.as_ref().map(Record::scope_key))
        .collect::<Vec<_>>();
    assert_eq!(
        scopes,
        [
            Some("src/file_4.rs"),
            None,
            Some("src/file_3.rs"),
            Some("src/file_2.rs")
        ]
    );
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_member_rejects_entire_batch_and_batch_admission_is_bounded() -> TestResult {
    let fixture = Fixture::new().await?;
    let db = &fixture.database;
    let anchor = digest(7);
    let pending = db
        .pending_file_summaries(
            PendingSummaryRollupQuery::new(db.project_id(), "judge", &anchor).page(10, 2),
        )
        .await?;
    let paths = pending
        .iter()
        .map(|item| NormalizedPath::parse(item.path()))
        .collect::<Result<Vec<_>, _>>()?;
    let digests = pending
        .iter()
        .map(|item| ContentDigest::parse(item.content_hash()))
        .collect::<Result<Vec<_>, _>>()?;
    let make = |index: usize| {
        FileSummarySaveRequest::new(
            &paths[index],
            SummarySaveInput::new(&digests[index], &anchor, "Must not publish partially.")
                .with_model("judge")
                .with_item_limit(2),
        )
    };
    db.save_symbol_summary(
        SymbolSummarySaveInput::new(db.project_id(), &symbol(4), &digest(2))
            .with_summary("Changed evidence.")
            .with_model("judge"),
    )
    .await?;
    let requests = (0..4).map(make).collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        db.save_file_summaries(db.project_id(), &requests).await,
        Err(StorageError::CurrentGenerationChanged)
    );
    assert_eq!(
        db.list_agent_artifacts(
            db.project_id(),
            AgentArtifactQuery::new(10)?
                .with_kind(Kind::Summary)
                .with_scope(Scope::File)
        )
        .await?,
        [] as [crate::AgentArtifactRecord; 0]
    );
    for requests in [
        vec![],
        vec![make(0)?, make(0)?],
        (0..17).map(|_| make(0)).collect::<Result<Vec<_>, _>>()?,
    ] {
        assert_eq!(
            db.save_file_summaries(db.project_id(), &requests).await,
            Err(StorageError::InvalidInput {
                field: "rollup_batch"
            })
        );
    }
    let admission_paths = (0..17)
        .map(|index| NormalizedPath::parse(&format!("bounds/file_{index}.rs")))
        .collect::<Result<Vec<_>, _>>()?;
    let body = "x".repeat(65_536);
    for (count, summary) in [(17, "Small paragraph."), (16, body.as_str())] {
        let requests = admission_paths[..count]
            .iter()
            .map(|path| {
                FileSummarySaveRequest::new(
                    path,
                    SummarySaveInput::new(&digests[0], &anchor, summary)
                        .with_model("judge")
                        .with_item_limit(2),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            db.save_file_summaries(db.project_id(), &requests).await,
            Err(StorageError::InvalidInput {
                field: "rollup_batch"
            })
        );
    }
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_trace_waits_for_a_real_mutation_transaction() -> TestResult {
    use crate::{
        McpToolCallData, McpToolCallInput, McpToolCallWrite, NewMcpSession, rocks::StateNamespace,
    };
    let fixture = Fixture::new().await?;
    let db = &fixture.database;
    let session = db
        .create_mcp_session(db.project_id(), &NewMcpSession::automatic())
        .await?;
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let store = db.store.clone();
    let writing = tokio::spawn(async move {
        store
            .state(
                None,
                ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?,
                4096,
                1,
                &StorageRequest::new(Duration::from_secs(10))?,
                move |transaction, _| {
                    transaction.put(StateNamespace::Metadata, b"writer-test", vec![1])?;
                    entered.send(()).map_err(|()| RocksError::Cancelled)?;
                    wait.recv_timeout(Duration::from_secs(5))
                        .map_err(|_| RocksError::Cancelled)?;
                    Ok(())
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), started).await??;
    let database = db.clone();
    let session_id = session.session_id().to_owned();
    let trace = tokio::spawn(async move {
        let input = McpToolCallInput::new(McpToolCallData {
            tool_name: "cartograph_find",
            arguments: serde_json::json!({}),
            result_summary: "success",
            success: true,
            duration_ms: 0,
        })?;
        database
            .record_mcp_tool_call(McpToolCallWrite {
                project_id: database.project_id(),
                session_id: &session_id,
                input: &input,
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    let blocked = !trace.is_finished();
    release.send(())?;
    tokio::time::timeout(Duration::from_secs(3), writing).await???;
    tokio::time::timeout(Duration::from_secs(3), trace).await???;
    assert!(blocked, "trace mutation bypassed the held transaction");
    fixture.runtime.shutdown(&request()?).await?;
    Ok(())
}
