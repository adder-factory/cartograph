use super::*;
use crate::{IndexOptions, ProjectError, RetrievalRequest};
use cartograph_search::{RerankState, SearchMode};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn fixture(endpoint: &str) -> TestResult<(tempfile::TempDir, ProjectRuntime, ProjectId)> {
    let root = tempfile::tempdir()?;
    std::fs::write(
        root.path().join("lib.rs"),
        "pub fn parse_ledger_entry() {}\npub fn render_ledger_entry() {}\npub fn ledger_entry_total() -> u32 { 0 }\n",
    )?;
    std::fs::create_dir(root.path().join(".cartograph"))?;
    std::fs::write(
        root.path().join(".cartograph/config.json"),
        serde_json::to_vec(&json!({
            "version": 2, "llm": {"enabled": true, "embeddingLlm": null,
                "rerankerLlm": {"provider": "openai-compat", "endpoint": endpoint,
                    "model": "fixture-reranker", "timeoutMs": 2000}}
        }))?,
    )?;
    let runtime = ProjectRuntime::open(root.path()).await?;
    runtime
        .index(IndexOptions::default().with_history_refresh(false))
        .await?;
    let project = runtime
        .status()
        .await?
        .snapshot
        .ok_or("missing snapshot")?
        .project_id;
    Ok((root, runtime, project))
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> TestResult<Value> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut chunk))
            .await??;
        if read == 0 || bytes.len() + read > 128 * 1024 {
            return Err("invalid HTTP request bound".into());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = std::str::from_utf8(&bytes[..header_end])?;
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .ok_or("missing content length")?;
            if bytes.len() >= header_end + 4 + length {
                return Ok(serde_json::from_slice(
                    &bytes[header_end + 4..header_end + 4 + length],
                )?);
            }
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, body: &Value) -> TestResult {
    let body = serde_json::to_string(body)?;
    socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reranks_lexical_candidates_without_embeddings_and_keeps_original_scores() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let (root, runtime, project) = fixture(&format!("http://{}", listener.local_addr()?)).await?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let request = read_request(&mut socket).await?;
        let documents = request["documents"].as_array().ok_or("documents absent")?;
        assert!(documents.len() > 1);
        assert_eq!(
            request["query"],
            "How are the ledger entry totals calculated?"
        );
        let results = documents.iter().enumerate().rev().map(|(index, document)| {
            let text = document.as_str().unwrap_or_default();
            assert!(text.len() <= 1536);
            json!({"index": index, "relevance_score": if text.starts_with("name: ledger_entry_total\n") {0.9} else {0.1}})
        }).collect::<Vec<_>>();
        respond(&mut socket, 200, &json!({"results": results})).await
    });
    let options = RetrievalOptions::new(SearchMode::Auto, 5)?;
    let prepared = runtime
        .prepare_retrieval(RetrievalRequest::new(
            project,
            "How are the ledger entry totals calculated?",
            options,
        ))
        .await?;
    let channels = prepared.channels.clone();
    let packet = prepared.into_packet(options)?;
    server.await??;
    assert_eq!(
        packet.semantic_readiness(),
        SemanticReadiness::NotConfigured
    );
    assert_eq!(packet.rerank_report().state(), RerankState::Applied);
    assert_eq!(
        packet.items()[0].document().qualified_name(),
        "ledger_entry_total"
    );
    assert!(packet.items()[0].rerank_score().is_some());
    for item in packet.items() {
        let original = channels
            .lexical()
            .ok_or("missing lexical channel")?
            .candidates()
            .iter()
            .find(|candidate| candidate.document().document_id() == item.document().document_id())
            .ok_or("missing original candidate")?;
        assert_eq!(item.contributions()[0].rank(), original.rank());
        assert_eq!(item.contributions()[0].raw_score(), original.raw_score());
    }
    drop(runtime);
    drop(root);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outage_and_malformed_responses_retain_fusion_order() -> TestResult {
    for (status, body) in [(500, json!({})), (200, json!({"results": []}))] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let (_root, runtime, project) =
            fixture(&format!("http://{}", listener.local_addr()?)).await?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let _request = read_request(&mut socket).await?;
            respond(&mut socket, status, &body).await
        });
        let options = RetrievalOptions::new(SearchMode::Auto, 5)?;
        let prepared = runtime
            .prepare_retrieval(RetrievalRequest::new(project, "ledger entry", options))
            .await?;
        let original = fuse_search(
            HybridSearchInput::new(SearchMode::Auto, prepared.semantic_readiness, 5)?
                .with_preference(prepared.preference)
                .with_channels(prepared.channels.clone()),
        )?;
        let packet = prepared.into_packet(options)?;
        assert_eq!(packet.items(), original.items());
        assert_eq!(packet.rerank_report().state(), RerankState::Unavailable);
        server.await??;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_cancellation_stops_an_active_rerank_request() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let (_root, runtime, project) = fixture(&format!("http://{}", listener.local_addr()?)).await?;
    let (started, received) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let _request = read_request(&mut socket).await?;
        started.send(()).map_err(|()| "start receiver dropped")?;
        let _finished = finished.await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let cancellation = ProjectCancellation::new();
    let pending = runtime.prepare_retrieval_with_cancellation(
        RetrievalRequest::new(
            project,
            "ledger entry",
            RetrievalOptions::new(SearchMode::Auto, 5)?,
        ),
        cancellation.clone(),
    );
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => panic!("rerank finished before cancellation: {}", result.is_ok()),
        ready = received => ready?,
    }
    cancellation.cancel();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), pending).await?,
        Err(ProjectError::RequestCancelled)
    ));
    let _finished = finish.send(());
    server.await??;
    Ok(())
}
