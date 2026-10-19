use super::*;
use crate::{
    FileInput, GenerationFacts, GenerationValidationLimits, SearchDocumentInput,
    rocks::{GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision},
    validate_generation_facts,
};
use cartograph_domain::{DocumentId, DocumentKind, FileId, FileParseStatus};

fn input(number: u8, base: GraphBase) -> Result<GraphPublicationInput> {
    let file = FileInput {
        file_id: FileId::from_uuid_v8([1; 16]),
        normalized_path: "src/lib.rs".into(),
        language: "rust".into(),
        content_hash: identity(number),
        byte_size: 200_000,
        parse_status: FileParseStatus::Parsed,
    };
    let document = SearchDocumentInput {
        document_id: DocumentId::from_uuid_v8([2; 16]),
        file_id: Some(file.file_id.clone()),
        symbol_id: None,
        path: file.normalized_path.clone(),
        language: file.language.clone(),
        kind: DocumentKind::File,
        qualified_name: "fixture".into(),
        code: format!("fn fixture_{number}() {{}}\n").repeat(10_000),
        natural_text: String::new(),
        metadata: serde_json::json!({}),
    };
    let (facts, _) = validate_generation_facts(
        GenerationFacts {
            files: vec![file],
            documents: vec![document],
            ..GenerationFacts::default()
        },
        GenerationValidationLimits::new(8 * 1_048_576, 32 * 1_048_576)
            .map_err(|_| super::super::super::operation("fixture-validation"))?,
        || false,
    )
    .map_err(|_| super::super::super::operation("fixture-validation"))?;
    GraphPublicationInput::prepare(
        GraphRevision {
            generation_id: generation(number),
            source_revision: identity(number),
            base,
        },
        facts,
        GraphPolicy {
            maximum_input_bytes: 64 * 1_024,
            ..GraphPolicy::default()
        },
        256 * 1_048_576,
        &request()?,
    )
}

fn read_budget() -> Result<ReadBudget> {
    ReadBudget::new(128, 1_048_576, Duration::from_secs(5))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destroyed_packed_tables_admit_old_graph_reads_before_publication_finishes() -> TestResult {
    for cancelled in [false, true] {
        let root = tempfile::tempdir()?;
        let next = input(2, GraphBase::Current(generation(1)))?;
        assert!(next.shared_facts().is_none());
        let reservation = next.reservation_bytes()?;
        // The reader is short exactly one byte until a real allocation dies.
        let runtime = StorageRuntime::new(WorkerPolicy {
            pending_bytes: reservation + read_budget()?.reservation_bytes()? - 1,
            ..WorkerPolicy::default()
        })?;
        let store = open(&runtime, root.path()).await?;
        store
            .publish_graph(input(1, GraphBase::Empty)?, &request()?)
            .await?;
        let (entered, before_credit) = tokio::sync::oneshot::channel();
        let (reclaim, reclaim_now) = mpsc::channel();
        let (reclaimed, after_credit) = tokio::sync::oneshot::channel();
        let (finish, finish_now) = mpsc::channel();
        *store
            .graph_reclaim_gate
            .lock()
            .map_err(|_| "fixture lock")? = Some(GraphReclaimTestGate {
            entered,
            reclaim: reclaim_now,
            reclaimed,
            finish: finish_now,
        });
        let control = request()?;
        let writer_control = control.clone();
        let writer_store = store.clone();
        let mut writer =
            tokio::spawn(async move { writer_store.publish_graph(next, &writer_control).await });
        tokio::time::timeout(Duration::from_secs(2), before_credit).await??;
        let reader_control = request()?;
        let mut reader = Box::pin(store.read(
            Some(generation(1)),
            read_budget()?,
            &reader_control,
            |snapshot, budget| {
                let graph = snapshot.graph(budget)?;
                let (documents, cursor) = graph.documents(None, 8, budget)?;
                assert!(cursor.is_none());
                assert_eq!(documents.len(), 1);
                assert!(documents[0].code().starts_with("fn fixture_1()"));
                Ok(graph.info().content_digest.clone())
            },
        ));
        assert!(futures_util::poll!(reader.as_mut()).is_pending());
        reclaim.send(())?;
        tokio::time::timeout(Duration::from_secs(2), after_credit).await??;
        let old_digest = tokio::time::timeout(Duration::from_secs(2), reader).await??;
        assert!(futures_util::poll!(&mut writer).is_pending());
        if cancelled {
            control.cancel();
        }
        finish.send(())?;
        let published = writer.await?;
        if cancelled {
            let error = published.err().ok_or("cancelled publication succeeded")?;
            assert_eq!(error.primary, RocksError::Cancelled);
            assert!(error.cleanup_failure.is_none());
        } else {
            assert_eq!(published?.revision.generation_id, generation(2));
        }
        let expected = if cancelled {
            generation(1)
        } else {
            generation(2)
        };
        store
            .read(
                Some(expected),
                read_budget()?,
                &request()?,
                move |snapshot, budget| {
                    let graph = snapshot.graph(budget)?;
                    assert_eq!(graph.info().files, 1);
                    assert_eq!(graph.info().documents, 1);
                    let (documents, _) = graph.documents(None, 8, budget)?;
                    assert!(documents[0].code().starts_with(if cancelled {
                        "fn fixture_1()"
                    } else {
                        "fn fixture_2()"
                    }));
                    assert_eq!(graph.info().content_digest == old_digest, cancelled);
                    Ok(())
                },
            )
            .await?;
        drop(store);
        runtime.shutdown(&request()?).await?;
    }
    Ok(())
}
