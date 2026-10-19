use std::{error::Error, time::Duration};

use cartograph_domain::{
    ContentDigest, DocumentId, DocumentKind, FileId, FileParseStatus, GenerationId,
};

use super::super::GraphPayload;
use super::*;
use crate::{
    GenerationFacts, GenerationValidationLimits, SearchDocumentInput,
    rocks::{GraphBase, GraphPublicationInput, GraphRevision, ProjectStore, StorePolicy},
    validate_generation_facts,
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

fn request() -> TestResult<StorageRequest> {
    Ok(StorageRequest::new(Duration::from_secs(30))?)
}

fn reader(blocks: &[Block]) -> Reader<'_> {
    Reader {
        blocks,
        next_block: 0,
        decoded: Vec::new(),
        offset: 0,
    }
}

fn table(rows: &[Vec<u8>]) -> TestResult<PackedTable> {
    let mut budget = Budget {
        bytes: 0,
        maximum: 16 * 1_048_576,
    };
    let mut builder = Builder::new(Table::Files, &mut budget)?;
    for row in rows {
        builder.row(row, &request()?)?;
    }
    Ok(builder.finish(&request()?)?)
}

#[test]
fn rows_and_length_headers_cross_blocks_without_changing_bytes() -> TestResult {
    let rows = vec![
        vec![b'a'; BLOCK_BYTES - 6],
        "éλ🦀".repeat(40_000).into_bytes(),
        (0..200_000_u32).flat_map(u32::to_le_bytes).collect(),
    ];
    let packed = table(&rows)?;
    assert!(packed.blocks.len() > 10);
    assert!(packed.blocks.iter().all(|block| {
        usize::try_from(block.decoded_length).is_ok_and(|len| len <= BLOCK_BYTES)
            && block.bytes.capacity() <= 2 * BLOCK_BYTES
    }));
    let mut stream = reader(&packed.blocks);
    for expected in rows {
        assert_eq!(stream.row(&request()?)?, Some(expected));
    }
    assert!(stream.row(&request()?)?.is_none());
    assert!(stream.decoded.capacity() <= BLOCK_BYTES);
    Ok(())
}

#[test]
fn incompressible_blocks_use_bounded_raw_storage_and_validate_their_length() -> TestResult {
    let raw: Vec<u8> = (0..2_048_u64)
        .flat_map(|index| *blake3::hash(&index.to_be_bytes()).as_bytes())
        .collect();
    assert_eq!(raw.len(), BLOCK_BYTES);
    let mut block = encode_block(&raw)?;
    assert!(!block.compressed);
    let mut output = vec![0; BLOCK_BYTES];
    reader(std::slice::from_ref(&block)).read(&mut output, &request()?)?;
    assert_eq!(output, raw);
    block.bytes.pop();
    assert!(matches!(
        reader(&[block]).fill(&request()?),
        Err(RocksError::Corrupt { .. })
    ));
    Ok(())
}

#[test]
fn damaged_blocks_and_invalid_row_lengths_fail_before_replay() -> TestResult {
    let mut checksum = table(&[vec![b'x'; 100_000]])?;
    checksum.blocks[0].checksum[0] ^= 1;
    assert!(matches!(
        reader(&checksum.blocks).row(&request()?),
        Err(RocksError::Corrupt { .. })
    ));
    let mut truncated = table(&[vec![b'x'; 100_000]])?;
    truncated.blocks.pop();
    assert!(matches!(
        reader(&truncated.blocks).row(&request()?),
        Err(RocksError::Corrupt { .. })
    ));
    let mut excessive = table(&[b"row".to_vec()])?;
    excessive.blocks[0].decoded_length = u32::MAX;
    assert!(matches!(
        reader(&excessive.blocks).row(&request()?),
        Err(RocksError::Corrupt { .. })
    ));
    for length in [0_u32, u32::MAX] {
        let blocks = [encode_block(&length.to_be_bytes())?];
        let mut stream = reader(&blocks);
        assert!(matches!(
            stream.row(&request()?),
            Err(RocksError::Corrupt { .. })
        ));
        assert!(stream.decoded.capacity() <= BLOCK_BYTES);
    }
    let blocks = [Block {
        bytes: vec![255],
        decoded_length: 128,
        checksum: [0; 32],
        compressed: true,
    }];
    assert!(matches!(
        reader(&blocks).row(&request()?),
        Err(RocksError::Corrupt { .. })
    ));
    Ok(())
}

#[test]
fn replay_rejects_duplicate_keys_and_wrong_declared_counts() -> TestResult {
    let row = br#"{"file_id":"01010101-0101-8101-8101-010101010101"}"#.to_vec();
    let duplicate = table(&[row.clone(), row.clone()])?;
    assert!(matches!(
        duplicate.visit(&request()?, |_, _| Ok(())),
        Err(RocksError::Corrupt { .. })
    ));
    let mut missing = table(&[row])?;
    missing.rows = 2;
    assert!(matches!(
        missing.visit(&request()?, |_, _| Ok(())),
        Err(RocksError::Corrupt { .. })
    ));
    Ok(())
}

#[test]
fn codec_budget_cancellation_and_deadline_are_enforced() -> TestResult {
    let mut budget = Budget {
        bytes: 0,
        maximum: 1,
    };
    let mut builder = Builder::new(Table::Files, &mut budget)?;
    assert!(matches!(
        builder.row(&vec![b'x'; BLOCK_BYTES], &request()?),
        Err(RocksError::Limit {
            resource: "graph_input_bytes"
        })
    ));
    let packed = table(&[vec![b'x'; 100_000]])?;
    let cancelled = request()?;
    cancelled.cancel();
    assert!(matches!(
        reader(&packed.blocks).row(&cancelled),
        Err(RocksError::Cancelled)
    ));
    let expired = StorageRequest::new(Duration::from_nanos(1))?;
    while expired.check().is_ok() {
        std::hint::spin_loop();
    }
    assert!(matches!(
        reader(&packed.blocks).row(&expired),
        Err(RocksError::Limit {
            resource: "operation_deadline"
        })
    ));
    Ok(())
}

fn canonical() -> TestResult<CanonicalGenerationFacts> {
    let file = FileInput {
        file_id: FileId::from_uuid_v8([1; 16]),
        normalized_path: "src/lib.rs".into(),
        language: "rust".into(),
        content_hash: ContentDigest::from_bytes([3; 32]),
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
        qualified_name: "source".into(),
        code: "fn repeat() {}\n".repeat(10_000),
        natural_text: String::new(),
        metadata: serde_json::json!({}),
    };
    Ok(validate_generation_facts(
        GenerationFacts {
            files: vec![file],
            documents: vec![document],
            ..GenerationFacts::default()
        },
        GenerationValidationLimits::new(8 * 1_048_576, 32 * 1_048_576)?,
        || false,
    )?
    .0)
}

fn identity(sequence: u8, base: GraphBase) -> GraphRevision {
    GraphRevision {
        generation_id: GenerationId::from_uuid_v8([sequence; 16]),
        source_revision: ContentDigest::from_bytes([9; 32]),
        base,
    }
}

#[test]
fn corrupted_publication_keeps_current_releases_writer_and_allows_reopen_retry() -> TestResult {
    let root = tempfile::tempdir()?;
    let project = ContentDigest::from_bytes([1; 32]);
    let store = ProjectStore::open(root.path(), &project, StorePolicy::default())?;
    let first = identity(1, GraphBase::Empty);
    let generation = first.generation_id.clone();
    store.publish_graph(
        GraphPublicationInput::new(first, canonical()?, GraphPolicy::default(), &request()?)?,
        &request()?,
    )?;
    let mut packed = PackedGraph::build(canonical()?, GraphPolicy::default(), &request()?)?;
    let documents = packed
        .tables
        .iter_mut()
        .find(|table| table.kind == Table::Documents)
        .ok_or("missing documents")?;
    documents.blocks[0].checksum[0] ^= 1;
    let damaged = GraphPublicationInput {
        identity: identity(2, GraphBase::Current(generation.clone())),
        retained_bytes: packed.retained_bytes()?,
        payload: GraphPayload::Packed(packed),
        changes: None,
        policy: GraphPolicy::default(),
        run_excludes: Vec::new(),
        maximum_file_bytes: None,
    };
    let mut credited = 0_usize;
    let error = store
        .publish_graph_reclaiming(damaged, &request()?, |bytes| {
            credited = credited
                .checked_add(bytes)
                .ok_or_else(super::super::limit)?;
            Ok(())
        })
        .err()
        .ok_or("damaged publication succeeded")?;
    assert!(matches!(error.primary, RocksError::Corrupt { .. }));
    assert!(credited > 0);
    assert!(error.cleanup_failure.is_none());
    assert_eq!(
        store.current_snapshot(None)?.revision().generation_id,
        generation
    );
    drop(store);
    let store = ProjectStore::open(root.path(), &project, StorePolicy::default())?;
    let replacement = GraphPublicationInput::prepare(
        identity(3, GraphBase::Current(generation)),
        canonical()?,
        GraphPolicy {
            maximum_input_bytes: 64 * 1_024,
            ..GraphPolicy::default()
        },
        256 * 1_048_576,
        &request()?,
    )?;
    assert!(replacement.shared_facts().is_none());
    let next = replacement.identity.generation_id.clone();
    store.publish_graph(replacement, &request()?)?;
    assert_eq!(store.current_snapshot(None)?.revision().generation_id, next);
    Ok(())
}

#[test]
fn consuming_replay_returns_destroyed_allocations_including_spare_descriptors() -> TestResult {
    let mut graph = PackedGraph::build(canonical()?, GraphPolicy::default(), &request()?)?;
    graph.tables.try_reserve_exact(3)?;
    let descriptors = graph.tables.capacity() * size_of::<PackedTable>();
    assert!(graph.tables.capacity() > graph.tables.len());
    assert!(
        graph
            .tables
            .iter()
            .any(|table| table.blocks.capacity() > table.blocks.len())
    );
    let before = graph.retained_bytes()?;
    let mut credited = Vec::new();
    let mut records = 0;
    let control = request()?;
    graph.replay(
        |_, _, table| {
            table.visit(&control, |_, _| {
                records += 1;
                Ok(())
            })
        },
        &control,
        |bytes| {
            credited.push(bytes);
            Ok(())
        },
    )?;
    assert_eq!(records, 2);
    assert!(graph.tables.is_empty());
    assert_eq!(graph.tables.capacity(), 0);
    assert_eq!(credited.last(), Some(&descriptors));
    assert_eq!(credited.len(), Table::ALL.len() + 1);
    assert_eq!(
        u64::try_from(credited.iter().sum::<usize>())?,
        before - graph.retained_bytes()?
    );
    assert_eq!(graph.files.len(), 1);
    assert!(graph.retained_bytes()? > 0);
    Ok(())
}

#[test]
fn preparation_peak_and_final_source_admission_stay_bounded() -> TestResult {
    let policy = GraphPolicy {
        maximum_input_bytes: 64 * 1_024,
        ..GraphPolicy::default()
    };
    assert!(matches!(
        GraphPublicationInput::prepare(
            identity(1, GraphBase::Empty),
            canonical()?,
            policy,
            1_048_576,
            &request()?
        ),
        Err(RocksError::Limit {
            resource: "graph_preparation_bytes"
        })
    ));
    let input = GraphPublicationInput::prepare(
        identity(1, GraphBase::Empty),
        canonical()?,
        policy,
        256 * 1_048_576,
        &request()?,
    )?;
    assert!(input.retained_input_bytes() <= policy.maximum_input_bytes);
    let mut pattern = String::with_capacity(64 * 1_024);
    pattern.push_str("evidence/**");
    assert!(matches!(
        input.with_source_admission(vec![pattern], None),
        Err(RocksError::Limit { .. })
    ));
    Ok(())
}
