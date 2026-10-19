use cartograph_domain::{FileId, FileParseStatus};
use rocksdb::WriteBatchIteratorCf;

use super::*;
use crate::{
    FileInput, GenerationFacts, GenerationValidationLimits,
    rocks::{GraphBase, GraphPolicy, GraphPublicationInput, GraphRevision},
    validate_generation_facts,
};

#[derive(Default)]
struct FactPuts {
    rows: usize,
}

impl WriteBatchIteratorCf for FactPuts {
    fn put_cf(&mut self, cf_id: u32, _key: &[u8], _value: &[u8]) {
        // The default family contains metadata. This fixture only writes graph
        // and search facts to the other families, never vectors or derived data.
        if cf_id != 0 {
            self.rows += 1;
        }
    }

    fn delete_cf(&mut self, _cf_id: u32, _key: &[u8]) {}

    fn merge_cf(&mut self, _cf_id: u32, _key: &[u8], _value: &[u8]) {}
}

fn files(count: u32, path_padding: usize) -> Vec<FileInput> {
    (0..count)
        .map(|index| {
            let mut bytes = [0; 16];
            bytes[..4].copy_from_slice(&index.to_be_bytes());
            FileInput {
                file_id: FileId::from_uuid_v8(bytes),
                normalized_path: format!("src/{}/{index}.rs", "a".repeat(path_padding)),
                language: "rust".into(),
                content_hash: identity(3),
                byte_size: 100,
                parse_status: FileParseStatus::Parsed,
            }
        })
        .collect()
}

fn native_batches(policy: StorePolicy, rows: &[FileInput]) -> TestResult<Vec<usize>> {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), policy)?;
    let (facts, _) = validate_generation_facts(
        GenerationFacts {
            files: rows.to_vec(),
            ..GenerationFacts::default()
        },
        GenerationValidationLimits::new(8 * 1_048_576, 32 * 1_048_576)?,
        || false,
    )?;
    let digest = facts.digest().clone();
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let input = GraphPublicationInput::new(
        GraphRevision {
            generation_id: GenerationId::from_uuid_v8([1; 16]),
            source_revision: identity(1),
            base: GraphBase::Empty,
        },
        facts,
        GraphPolicy::default(),
        &request,
    )?;
    let start = store.inner.database.latest_sequence_number();
    let publication = store.publish_graph(input, &request)?;
    assert_eq!(publication.revision.content_digest.as_ref(), Some(&digest));
    let mut batches = Vec::new();
    let mut updates = store.inner.database.get_updates_since(start)?;
    for update in updates.by_ref() {
        let (_, batch) = update?;
        assert!(batch.size_in_bytes() <= policy.maximum_batch_bytes);
        let mut puts = FactPuts::default();
        batch.iterate_cf(&mut puts);
        assert!(puts.rows <= policy.maximum_batch_rows);
        if puts.rows != 0 {
            batches.push(puts.rows);
        }
    }
    updates.status()?;
    drop(updates);
    drop(store);

    let reopened = ProjectStore::open(root.path(), &identity(7), policy)?;
    let snapshot = reopened.current_snapshot(None)?;
    let mut budget = ReadBudget::new(10_000, 32 * 1_048_576, Duration::from_secs(30))?;
    let graph = snapshot.graph(&mut budget)?;
    assert_eq!(snapshot.revision().content_digest.as_ref(), Some(&digest));
    for expected in rows {
        assert_eq!(
            graph.file(&expected.file_id, &mut budget)?.as_ref(),
            Some(expected)
        );
    }
    Ok(batches)
}

#[test]
fn graph_publication_fills_existing_native_batch_allowance() -> TestResult {
    let batches = native_batches(StorePolicy::default(), &files(2_048, 8))?;
    assert!(
        batches.iter().any(|&rows| rows > 256),
        "native durable batches stayed artificially small"
    );
    Ok(())
}

#[test]
fn graph_publication_honors_tighter_native_row_and_byte_limits() -> TestResult {
    let rows = native_batches(
        StorePolicy {
            maximum_batch_rows: 17,
            ..StorePolicy::default()
        },
        &files(513, 8),
    )?;
    assert!(rows.len() > 20);
    let bytes = native_batches(
        StorePolicy {
            maximum_batch_bytes: 1_048_576,
            ..StorePolicy::default()
        },
        &files(4_096, 200),
    )?;
    assert!(bytes.len() > 1);
    Ok(())
}
