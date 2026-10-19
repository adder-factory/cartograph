use super::*;
use crate::rocks::{ProjectStore, RecordChange, RocksError, StorageRequest, StorePolicy};
use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};
use std::time::Duration;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn budget() -> TestResult<ReadBudget> {
    Ok(ReadBudget::new(1_000, 1_048_576, Duration::from_secs(10))?)
}

fn row(collection: Collection, key: &[u8], value: &[u8]) -> TestResult<RecordChange> {
    Ok(RecordChange::put(
        collection,
        RecordKey::new(key.to_vec())?,
        value.to_vec(),
    ))
}

fn publish(store: &ProjectStore, sequence: u8, changes: &[RecordChange]) -> TestResult {
    let writer = store.begin_revision(
        GenerationId::from_uuid_v8([sequence; 16]),
        ContentDigest::from_bytes([1; 32]),
        Duration::from_secs(30),
    )?;
    store.stage(&writer, changes)?;
    store.seal(
        &writer,
        ContentDigest::from_bytes([sequence; 32]),
        GenerationDigestVersion::V23,
    )?;
    store.publish(&writer)?;
    Ok(())
}

#[test]
fn point_readers_reseek_all_collections_without_crossing_their_snapshot() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(
        root.path(),
        &ContentDigest::from_bytes([7; 32]),
        StorePolicy::default(),
    )?;
    let mut rows = Collection::ALL
        .into_iter()
        .map(|collection| row(collection, b"a", collection.name().as_bytes()))
        .collect::<TestResult<Vec<_>>>()?;
    rows.push(row(Collection::Graph, b"a\0b", b"binary")?);
    rows.push(row(Collection::Graph, b"z", b"last")?);
    publish(&store, 1, &rows)?;
    let original = store.current_snapshot(None)?;
    let original_readers = original.point_readers();
    let failed = store.begin_revision(
        GenerationId::from_uuid_v8([2; 16]),
        ContentDigest::from_bytes([1; 32]),
        Duration::from_secs(30),
    )?;
    store.stage(&failed, &[row(Collection::Graph, b"missing", b"hidden")?])?;
    store.fail(&failed)?;
    publish(
        &store,
        3,
        &[
            row(Collection::Graph, b"a", b"new")?,
            RecordChange::delete(Collection::Graph, RecordKey::new(b"z".to_vec())?),
        ],
    )?;
    let current = store.current_snapshot(None)?;
    let readers = current.point_readers();
    for _ in 0..3 {
        for (name, expected) in [
            (b"z".as_slice(), None),
            (b"a\0b", Some(b"binary".as_slice())),
            (b"missing", None),
            (b"a\0", None),
            (b"a", Some(b"new".as_slice())),
        ] {
            assert_eq!(
                readers
                    .get(
                        Collection::Graph,
                        &RecordKey::new(name.to_vec())?,
                        &mut budget()?
                    )?
                    .as_deref(),
                expected,
            );
        }
        for collection in Collection::ALL.into_iter().rev() {
            let expected = if collection == Collection::Graph {
                b"new".as_slice()
            } else {
                collection.name().as_bytes()
            };
            assert_eq!(
                readers
                    .get(collection, &RecordKey::new(b"a".to_vec())?, &mut budget()?)?
                    .as_deref(),
                Some(expected)
            );
        }
    }
    publish(&store, 4, &[row(Collection::Graph, b"a", b"later")?])?;
    assert_eq!(
        readers
            .get(
                Collection::Graph,
                &RecordKey::new(b"a".to_vec())?,
                &mut budget()?
            )?
            .as_deref(),
        Some(b"new".as_slice())
    );
    assert_eq!(
        original_readers
            .get(
                Collection::Graph,
                &RecordKey::new(b"a".to_vec())?,
                &mut budget()?
            )?
            .as_deref(),
        Some(b"graph".as_slice())
    );
    assert_eq!(
        original_readers
            .get(
                Collection::Graph,
                &RecordKey::new(b"z".to_vec())?,
                &mut budget()?
            )?
            .as_deref(),
        Some(b"last".as_slice())
    );
    assert_eq!(readers.readers.lock().map_err(|_| "reader lock")?.len(), 4);
    Ok(())
}

#[test]
fn cached_point_readers_keep_concurrent_reads_cancellation_and_physical_bounds() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(
        root.path(),
        &ContentDigest::from_bytes([7; 32]),
        StorePolicy::default(),
    )?;
    publish(&store, 1, &[row(Collection::Graph, b"a", b"value")?])?;
    let snapshot = store.current_snapshot(None)?;
    let readers = snapshot.point_readers();
    let key = RecordKey::new(b"a".to_vec())?;
    std::thread::scope(|scope| -> TestResult {
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let readers = &readers;
            let key = &key;
            tasks.push(scope.spawn(move || -> Result<()> {
                let mut allowance = ReadBudget::new(200, 1_048_576, Duration::from_secs(10))?;
                for _ in 0..100 {
                    assert_eq!(
                        readers
                            .get(Collection::Graph, key, &mut allowance)?
                            .as_deref(),
                        Some(b"value".as_slice())
                    );
                }
                Ok(())
            }));
        }
        for task in tasks {
            task.join().map_err(|_| "point reader thread panicked")??;
        }
        Ok(())
    })?;
    let mut bytes = ReadBudget::new(10, 1, Duration::from_secs(10))?;
    assert_eq!(
        readers.get(Collection::Graph, &key, &mut bytes),
        Err(RocksError::Limit {
            resource: "read_bytes"
        })
    );
    // A missing key before 'a' still inspects and charges its physical neighbor.
    let missing = RecordKey::new(b"0".to_vec())?;
    assert_eq!(
        readers.get(Collection::Graph, &missing, &mut bytes),
        Err(RocksError::Limit {
            resource: "read_bytes"
        })
    );
    let mut rows = ReadBudget::new(1, 1_048_576, Duration::from_secs(10))?;
    assert!(readers.get(Collection::Graph, &key, &mut rows)?.is_some());
    assert_eq!(
        readers.get(Collection::Graph, &key, &mut rows),
        Err(RocksError::Limit {
            resource: "read_rows"
        })
    );
    let control = StorageRequest::new(Duration::from_secs(10))?;
    let mut cancelled = budget()?.with_control(control.clone());
    control.cancel();
    assert_eq!(
        readers.get(Collection::Graph, &key, &mut cancelled),
        Err(RocksError::Cancelled)
    );
    assert_eq!(readers.readers.lock().map_err(|_| "reader lock")?.len(), 1);
    Ok(())
}
