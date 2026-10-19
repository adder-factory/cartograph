use std::{error::Error, fs, time::Duration};

use cartograph_db::{
    GraphDirection,
    rocks::{
        AsyncProjectStore, GraphBase, ReadBudget, RevisionSnapshot, StorageRuntime, StorePolicy,
        WorkerPolicy,
    },
};
use cartograph_domain::{ContentDigest, EdgeKind, GenerationId};
use cartograph_extract::{DiscoveryLimits, SourceLimits, SourceRoot};
use tokio::time::Instant;

use super::*;
use crate::{
    NativePipelineConfig, NativePipelineDeadlines, NativePipelineLimits, NativePipelineParallelism,
    NativeRetainedLimits, StageCapacity, stage::test_stage_runner,
};

type TestResult = Result<(), Box<dyn Error>>;

fn config() -> Result<NativePipelineConfig, Box<dyn Error>> {
    Ok(NativePipelineConfig::new(
        NativePipelineLimits::new(
            DiscoveryLimits::new(32, 1_048_576)?,
            SourceLimits::new(1_048_576)?,
            NativeRetainedLimits::new(4 * 1_048_576, 32 * 1_048_576)?,
        ),
        NativePipelineParallelism::new(StageCapacity::new(4, 4), StageCapacity::new(4, 4))?,
        NativePipelineDeadlines::new(
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(2),
        )?,
    ))
}

fn request() -> Result<StorageRequest, RocksError> {
    StorageRequest::new(Duration::from_secs(60))
}

fn budget() -> Result<ReadBudget, RocksError> {
    ReadBudget::new(20_000, 8 * 1_048_576, Duration::from_secs(30))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_cross_file_edit_replaces_both_directions_and_keeps_unresolved_references()
-> TestResult {
    let source = tempfile::tempdir()?;
    fs::create_dir(source.path().join("src"))?;
    fs::write(
        source.path().join("Cargo.toml"),
        "[package]\nname = \"graph_probe\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        source.path().join("src/lib.rs"),
        "mod provider; pub fn caller() { provider::target(); }\n",
    )?;
    fs::write(
        source.path().join("src/provider.rs"),
        "pub fn target() {}\n",
    )?;
    let storage_root = tempfile::tempdir()?;
    let runtime = StorageRuntime::new(WorkerPolicy::default())?;
    let store = AsyncProjectStore::open(
        &runtime,
        storage_root.path().to_path_buf(),
        ContentDigest::from_bytes([1; 32]),
        StorePolicy::default(),
        &request()?,
    )
    .await?;
    let mut previous = None;
    for sequence in 1..=3_u8 {
        if sequence == 2 {
            fs::write(
                source.path().join("src/provider.rs"),
                "pub fn replacement() {}\n",
            )?;
        }
        if sequence == 3 {
            fs::write(
                source.path().join("src/lib.rs"),
                "mod provider; pub fn caller() { provider::replacement(); }\n",
            )?;
        }
        let identity = GraphRevision {
            generation_id: GenerationId::from_uuid_v8([sequence; 16]),
            source_revision: ContentDigest::from_bytes([sequence; 32]),
            base: previous
                .clone()
                .map_or(GraphBase::Empty, GraphBase::Current),
        };
        let (runner, tasks, cancellation) = test_stage_runner(16, 256 * 1_048_576).await;
        let (input, _) = build_native_rocks_graph(
            &runner,
            NativeGenerationBuild::new(SourceRoot::open(source.path())?, config()?),
            identity,
            GraphPolicy::default(),
            &request()?,
        )
        .await?;
        drop(cancellation);
        let reaped = tasks
            .close_abort_and_reap_retained(Instant::now() + Duration::from_secs(10))
            .await;
        assert!(reaped.all_joined && !reaped.worker_failed && !reaped.unobserved_results);
        let published = store.publish_graph(input, &request()?).await?;
        previous = Some(published.revision.generation_id.clone());
        store
            .read(
                previous.clone(),
                budget()?,
                &request()?,
                move |snapshot, budget| verify_native_graph(snapshot, budget, sequence),
            )
            .await?;
    }
    runtime.shutdown(&request()?).await?;
    Ok(())
}

fn verify_native_graph(
    snapshot: &RevisionSnapshot<'_>,
    budget: &mut ReadBudget,
    sequence: u8,
) -> Result<(), RocksError> {
    let graph = snapshot.graph(budget)?;
    // Identify actual native declarations through their canonical symbol keys.
    let page = snapshot.scan_prefix(
        cartograph_db::rocks::Collection::Graph,
        b"cg1/s/",
        None,
        100,
        budget,
    )?;
    let symbols: Vec<cartograph_db::SymbolInput> = page
        .rows
        .into_iter()
        .map(|(_, bytes)| {
            serde_json::from_slice(&bytes).map_err(|_| RocksError::Corrupt {
                field: "probe_symbol",
            })
        })
        .collect::<Result<_, _>>()?;
    let caller = symbols
        .iter()
        .find(|symbol| symbol.qualified_name.ends_with("caller"))
        .ok_or(RocksError::Corrupt {
            field: "probe_caller",
        })?;
    let calls = graph
        .neighbors(
            &caller.symbol_id,
            GraphDirection::Outgoing,
            None,
            100,
            budget,
        )?
        .links
        .into_iter()
        .filter(|edge| edge.kind() == EdgeKind::Calls)
        .collect::<Vec<_>>();
    if sequence == 2 {
        assert!(calls.is_empty());
        assert!(
            !symbols
                .iter()
                .any(|symbol| symbol.qualified_name.ends_with("target"))
        );
        let references = graph.references(&caller.file_id, None, 100, budget)?.0;
        assert!(
            references
                .iter()
                .any(|reference| reference.reference_name.contains("target")
                    && reference.target_symbol_id.is_none())
        );
    } else {
        assert_eq!(calls.len(), 1);
        let target = calls[0].target_symbol_id();
        let reverse = graph.neighbors(target, GraphDirection::Incoming, None, 100, budget)?;
        assert!(
            reverse
                .links
                .iter()
                .any(|edge| edge.kind() == EdgeKind::Calls
                    && edge.source_symbol_id() == &caller.symbol_id)
        );
    }
    Ok(())
}
