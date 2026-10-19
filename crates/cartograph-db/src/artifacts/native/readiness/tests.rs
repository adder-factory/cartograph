use super::*;
use crate::{
    FileInput, GenerationFacts, GenerationValidationLimits, SearchDocumentInput, SymbolInput,
    SymbolRoleSaveInput, SymbolSummarySaveInput,
    rocks::{
        AsyncProjectStore, GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision,
        StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy,
    },
    validate_generation_facts,
};
use cartograph_domain::{
    DocumentId, DocumentKind, FileId, FileParseStatus, SymbolExecutionFlags, SymbolExportFlags,
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

fn facts() -> GenerationFacts {
    let file = FileInput {
        file_id: FileId::from_uuid_v8([1; 16]),
        normalized_path: "src/lib.rs".into(),
        language: "rust".into(),
        content_hash: digest(2),
        byte_size: 1,
        parse_status: FileParseStatus::Parsed,
    };
    let symbols: Vec<_> = (1..=257)
        .map(|id| SymbolInput {
            symbol_id: symbol(id),
            file_id: file.file_id.clone(),
            symbol_kind: "function".into(),
            qualified_name: format!("function_{id}"),
            signature: "fn function()".into(),
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            end_line: 5,
            structural_digest: digest(2),
            visibility: None,
            export: SymbolExportFlags::default(),
            execution: SymbolExecutionFlags::default(),
            declaration_only: false,
            betweenness_ppb: None,
            pagerank_ppb: None,
        })
        .collect();
    let documents = symbols
        .iter()
        .zip(1_u64..)
        .map(|(symbol, index)| SearchDocumentInput {
            document_id: {
                let mut bytes = [0; 16];
                bytes[8..].copy_from_slice(&index.to_be_bytes());
                DocumentId::from_uuid_v8(bytes)
            },
            file_id: Some(file.file_id.clone()),
            symbol_id: Some(symbol.symbol_id.clone()),
            path: file.normalized_path.clone(),
            language: "rust".into(),
            kind: DocumentKind::Symbol,
            qualified_name: symbol.qualified_name.clone(),
            code: "fn function() {}".into(),
            natural_text: if index == 257 {
                "Documented behavior that requires no generated summary.".into()
            } else {
                String::new()
            },
            metadata: serde_json::json!({}),
        })
        .collect();
    GenerationFacts {
        files: vec![file],
        symbols,
        documents,
        ..GenerationFacts::default()
    }
}

async fn publish(store: &AsyncProjectStore, number: u8, mut facts: GenerationFacts) -> TestResult {
    if number == 2 {
        facts.symbols[0].structural_digest = digest(3);
    }
    let (facts, _) = validate_generation_facts(
        facts,
        GenerationValidationLimits::new(16 * 1_048_576, 64 * 1_048_576)?,
        || false,
    )?;
    store
        .publish_graph(
            GraphPublicationInput::new(
                GraphRevision {
                    generation_id: GenerationId::from_uuid_v8([number; 16]),
                    source_revision: digest(9),
                    base: if number == 1 {
                        GraphBase::Empty
                    } else {
                        GraphBase::Current(GenerationId::from_uuid_v8([1; 16]))
                    },
                },
                facts,
                GraphPolicy::default(),
                &request()?,
            )?,
            &request()?,
        )
        .await?;
    Ok(())
}

async fn populate(db: &NativeDatabase) -> TestResult {
    for id in 1..=257 {
        if id <= 200 {
            db.save_symbol_role(
                SymbolRoleSaveInput::new(
                    db.project_id(),
                    &symbol(id),
                    if id % 2 == 0 {
                        "test_helper"
                    } else {
                        "business_logic"
                    },
                )
                .with_metadata(serde_json::json!({"model":"fixture-judge"})),
            )
            .await?;
        }
        if id <= 129 {
            db.save_symbol_summary(
                SymbolSummarySaveInput::new(db.project_id(), &symbol(id), &digest(2))
                    .with_summary("Exact stored behavior.")
                    .with_model("fixture-judge"),
            )
            .await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn populated_status_readiness_is_exact_bounded_and_generation_fenced() -> TestResult {
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
    let db = NativeDatabase::new(store.clone(), &digest(1));
    publish(&store, 1, facts()).await?;
    populate(&db).await?;
    let policy = SummaryCandidatePolicy::default();
    let (coverage, roles) = db
        .state(None, 0, 0, move |tx, _| {
            // Shared physical accounting fits fewer than seven reads per symbol.
            let mut budget = ReadBudget::new(1800, 4 * 1_048_576, Duration::from_secs(5))?;
            let snapshot = tx.graph_snapshot()?;
            let graph = snapshot.graph(&mut budget)?;
            counts(tx, &graph, &policy, &mut budget)
        })
        .await?;
    assert_base_counts(&coverage, &roles);
    assert_eq!(
        coverage,
        db.current_summary_coverage(db.project_id()).await?
    );
    assert_eq!(roles, db.agent_role_distribution(db.project_id()).await?);
    let inventory = db
        .current_status_inventory(db.project_id(), &GenerationId::from_uuid_v8([1; 16]))
        .await?;
    assert_eq!(inventory.0.total_files(), 1);
    assert_eq!(inventory.0.total_symbols(), 257);
    assert_eq!(serde_json::to_value(&inventory.1)?[0]["symbols"], 257);
    publish(&store, 2, facts()).await?;
    assert!(matches!(
        db.current_agent_readiness(
            db.project_id(),
            &GenerationId::from_uuid_v8([1; 16]),
            &SummaryCandidatePolicy::default()
        )
        .await,
        Err(StorageError::CurrentGenerationChanged)
    ));
    let (coverage, roles) = db
        .current_agent_readiness(
            db.project_id(),
            &GenerationId::from_uuid_v8([2; 16]),
            &SummaryCandidatePolicy::default(),
        )
        .await?;
    assert_eq!(coverage.summarized_symbols, 128);
    assert_eq!(roles[0].role, "test_helper");
    assert_eq!(roles[0].symbols, 100);
    assert_eq!(roles[1].symbols, 99);
    assert!(matches!(
        db.current_status_inventory(db.project_id(), &GenerationId::from_uuid_v8([1; 16]))
            .await,
        Err(StorageError::CurrentGenerationChanged)
    ));
    reject_broken_scope(&db).await?;
    runtime.shutdown(&request()?).await?;
    Ok(())
}

fn assert_base_counts(coverage: &SummaryCoverageStats, roles: &[AgentRoleCount]) {
    assert_eq!(coverage.eligible_symbols, 256);
    assert_eq!(coverage.summarized_symbols, 129);
    assert_eq!(coverage.pending_symbols, 127);
    assert_eq!(
        coverage.by_model,
        BTreeMap::from([("fixture-judge".into(), 129)])
    );
    assert_eq!(
        roles,
        [
            AgentRoleCount {
                role: "business_logic".into(),
                symbols: 100
            },
            AgentRoleCount {
                role: "test_helper".into(),
                symbols: 100
            }
        ]
    );
}

async fn reject_broken_scope(db: &NativeDatabase) -> TestResult {
    db.state(None, 1024, 1, |tx, _| {
        tx.put(
            NAMESPACE,
            scope_key(Kind::Role, Scope::Symbol, symbol(1).as_str()).as_bytes(),
            u64::MAX.to_be_bytes().to_vec(),
        )
    })
    .await?;
    assert!(matches!(
        db.current_agent_readiness(
            db.project_id(),
            &GenerationId::from_uuid_v8([2; 16]),
            &SummaryCandidatePolicy::default()
        )
        .await,
        Err(StorageError::CorruptStoredValue { .. })
    ));
    Ok(())
}
