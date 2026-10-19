//! Canonical native graph persistence, compact adjacency, and incremental publication.

#[path = "rocks_graph/concurrency.rs"]
mod concurrency;
#[path = "rocks_graph/corruption.rs"]
mod corruption;
mod dependency_ownership;
#[path = "rocks_graph/features.rs"]
mod features;
#[path = "rocks_graph/findings.rs"]
mod findings;
#[path = "rocks_graph/layout.rs"]
mod layout;
#[path = "rocks_graph/lexical.rs"]
mod lexical;
#[path = "rocks_graph/lifecycle.rs"]
mod lifecycle;
#[path = "rocks_graph/local_edits.rs"]
mod local_edits;
#[path = "rocks_graph/names.rs"]
mod names;
#[path = "rocks_graph/soak.rs"]
mod soak;
#[path = "rocks_graph/state_concurrency.rs"]
mod state_concurrency;
#[path = "rocks_graph/summaries.rs"]
mod summaries;

use std::{error::Error, time::Duration};

use cartograph_db::{
    CanonicalGenerationFacts, EdgeInput, FileInput, GenerationFacts, GenerationValidationLimits,
    GraphDirection, SymbolInput,
    rocks::{
        GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision, ProjectStore, ReadBudget,
        RocksError, StorageRequest, StorePolicy,
    },
    validate_generation_facts,
};
use cartograph_domain::{
    ContentDigest, EdgeKind, FileId, FileParseStatus, GenerationId, SymbolExecutionFlags,
    SymbolExportFlags, SymbolId,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn digest(value: u8) -> ContentDigest {
    ContentDigest::from_bytes([value; 32])
}

fn symbol(value: u64) -> SymbolId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&value.to_be_bytes());
    SymbolId::from_uuid_v8(bytes)
}

fn generation(value: u64) -> GenerationId {
    let mut bytes = [1; 16];
    bytes[8..].copy_from_slice(&value.to_be_bytes());
    GenerationId::from_uuid_v8(bytes)
}

fn request() -> TestResult<StorageRequest> {
    Ok(StorageRequest::new(Duration::from_secs(60))?)
}

fn budget() -> TestResult<ReadBudget> {
    Ok(ReadBudget::new(
        100_000,
        32 * 1_048_576,
        Duration::from_secs(30),
    )?)
}

fn facts(nodes: u64, edges: Vec<EdgeInput>) -> TestResult<CanonicalGenerationFacts> {
    canonical(raw_facts(nodes, edges))
}

fn raw_facts(nodes: u64, edges: Vec<EdgeInput>) -> GenerationFacts {
    let file_id = FileId::from_uuid_v8([1; 16]);
    let symbols = (1..=nodes)
        .map(|index| SymbolInput {
            symbol_id: symbol(index),
            file_id: file_id.clone(),
            symbol_kind: "function".into(),
            qualified_name: format!("function_{index}"),
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
    GenerationFacts {
        files: vec![FileInput {
            file_id,
            normalized_path: "src/lib.rs".into(),
            language: "rust".into(),
            content_hash: digest(3),
            byte_size: 1,
            parse_status: FileParseStatus::Parsed,
        }],
        symbols,
        edges,
        ..GenerationFacts::default()
    }
}

fn canonical(facts: GenerationFacts) -> TestResult<CanonicalGenerationFacts> {
    Ok(validate_generation_facts(
        facts,
        GenerationValidationLimits::new(64 * 1_048_576, 256 * 1_048_576)?,
        || false,
    )?
    .0)
}

fn edge(source: u64, target: u64, provenance: &str) -> EdgeInput {
    EdgeInput {
        source_symbol_id: symbol(source),
        target_symbol_id: symbol(target),
        kind: EdgeKind::Calls,
        confidence: 0.875,
        provenance: provenance.into(),
        site_count: 3,
    }
}

fn input(sequence: u64, facts: CanonicalGenerationFacts) -> TestResult<GraphPublicationInput> {
    Ok(GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(sequence),
            source_revision: digest(9),
            base: if sequence == 1 {
                GraphBase::Empty
            } else {
                GraphBase::Current(generation(sequence - 1))
            },
        },
        facts,
        GraphPolicy::default(),
        &request()?,
    )?)
}

#[test]
fn canonical_edges_preserve_both_directions_and_distinct_provenance() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let expected = facts(
        3,
        vec![
            edge(1, 2, "native"),
            edge(1, 2, "scip"),
            edge(3, 2, "native"),
        ],
    )?;
    let logical_digest = expected.digest().clone();
    let published = store.publish_graph(input(1, expected)?, &request()?)?;
    assert_eq!(published.revision.content_digest, Some(logical_digest));
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    let incoming = graph.neighbors(
        &symbol(2),
        GraphDirection::Incoming,
        None,
        20,
        &mut budget()?,
    )?;
    assert_eq!(incoming.links.len(), 3);
    for link in &incoming.links {
        assert_eq!(link.target_symbol_id(), &symbol(2));
        let property = graph.edge(link, &mut budget()?)?;
        assert_eq!(property.site_count, 3);
        assert_eq!(property.confidence, 0.875);
    }
    let outgoing = graph.neighbors(
        &symbol(1),
        GraphDirection::Outgoing,
        None,
        20,
        &mut budget()?,
    )?;
    assert_eq!(outgoing.links.len(), 2);
    assert!(
        graph
            .neighbors(
                &symbol(2),
                GraphDirection::Outgoing,
                None,
                20,
                &mut budget()?
            )?
            .links
            .is_empty()
    );
    Ok(())
}

#[test]
fn replacement_removes_reverse_edges_and_old_snapshots_remain_complete() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    store.publish_graph(
        input(1, facts(3, vec![edge(1, 2, "native")])?)?,
        &request()?,
    )?;
    let old = store.current_snapshot(None)?;
    let publication = store.publish_graph(
        input(2, facts(3, vec![edge(1, 3, "native")])?)?,
        &request()?,
    )?;
    assert!(publication.report.deleted_records > 0);
    assert!(publication.report.reused_records >= 4);
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    assert!(
        graph
            .neighbors(
                &symbol(2),
                GraphDirection::Incoming,
                None,
                20,
                &mut budget()?
            )?
            .links
            .is_empty()
    );
    assert_eq!(
        graph
            .neighbors(
                &symbol(3),
                GraphDirection::Incoming,
                None,
                20,
                &mut budget()?
            )?
            .links
            .len(),
        1
    );
    let old_graph = old.graph(&mut budget()?)?;
    assert_eq!(
        old_graph
            .neighbors(
                &symbol(2),
                GraphDirection::Incoming,
                None,
                20,
                &mut budget()?
            )?
            .links
            .len(),
        1
    );
    Ok(())
}

#[test]
fn stale_base_cannot_publish_even_with_a_new_generation_identity() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    store.publish_graph(
        input(1, facts(2, vec![edge(1, 2, "native")])?)?,
        &request()?,
    )?;
    let stale = input(1, facts(2, Vec::new())?)?;
    assert!(matches!(
        store
            .publish_graph(stale, &request()?)
            .map_err(|error| error.primary),
        Err(RocksError::GenerationChanged)
    ));
    assert_eq!(
        store.current_snapshot(None)?.revision().generation_id,
        generation(1)
    );
    Ok(())
}
