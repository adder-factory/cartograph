use std::sync::mpsc;

use super::*;
use crate::rocks::StorageRequest;

mod batching;
mod cache;
mod idle_retention;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn identity(byte: u8) -> ContentDigest {
    ContentDigest::from_bytes([byte; 32])
}

#[test]
fn cancellation_while_waiting_for_publication_lock_is_checked_before_durable_commit() -> TestResult
{
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let writer = store.begin_revision(
        GenerationId::from_uuid_v8([1; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
    let request = StorageRequest::new(Duration::from_secs(10))?;
    let controlled = store.with_control(request.clone());
    let writer_copy = writer.clone();
    let result = std::thread::scope(|scope| -> TestResult<Result<StoredRevision>> {
        let held = store.lock_mutation()?;
        let (entered, started) = mpsc::sync_channel(1);
        let task = scope.spawn(move || {
            entered.send(()).map_err(|_| operation("test_gate"))?;
            controlled.publish(&writer_copy)
        });
        started.recv_timeout(Duration::from_secs(2))?;
        request.cancel();
        drop(held);
        Ok(task.join().map_err(|_| "publication worker panicked")?)
    })?;
    assert_eq!(result, Err(RocksError::Cancelled));
    assert!(matches!(
        store.current_snapshot(None),
        Err(RocksError::GenerationUnavailable)
    ));
    assert_eq!(store.publish(&writer)?.sequence, 1);
    Ok(())
}

#[test]
fn checked_merge_counter_fails_closed_on_corrupt_bytes_and_overflow() {
    // Exercise the installed native callback through RocksDB, including merge
    // evaluation during a read rather than duplicating addition in a mock.
    let result = counter_failure_cases();
    assert!(result.is_ok(), "native counter corruption contract failed");
}

fn counter_failure_cases() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let meta = store.meta()?;
    let mut batch = WriteBatch::default();
    batch.put_cf(&meta, b"counter/overflow", u64::MAX.to_be_bytes());
    batch.merge_cf(&meta, b"counter/overflow", 1_u64.to_be_bytes());
    store.write(&batch)?;
    assert!(store.get_meta(b"counter/overflow").is_err());
    let mut batch = WriteBatch::default();
    batch.put_cf(&meta, b"counter/corrupt", b"invalid");
    batch.merge_cf(&meta, b"counter/corrupt", 1_u64.to_be_bytes());
    store.write(&batch)?;
    assert!(store.get_meta(b"counter/corrupt").is_err());
    Ok(())
}

#[test]
fn heartbeat_renews_from_lock_acquisition_instead_of_its_queue_arrival() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let writer = store.begin_revision(
        GenerationId::from_uuid_v8([1; 16]),
        identity(1),
        Duration::from_secs(5),
    )?;
    let renewed = Duration::from_millis(30);
    std::thread::scope(|scope| -> TestResult {
        let held = store.lock_writer()?;
        let (entered, started) = mpsc::sync_channel(1);
        let store = &store;
        let writer = &writer;
        let task = scope.spawn(move || {
            entered.send(()).map_err(|_| operation("test_gate"))?;
            store.heartbeat(writer, renewed)
        });
        started.recv_timeout(Duration::from_secs(2))?;
        std::thread::sleep(Duration::from_millis(80));
        let released_at = Instant::now();
        drop(held);
        task.join().map_err(|_| "heartbeat worker panicked")??;
        let active = store.lock_writer()?;
        let expiry = active.as_ref().ok_or("missing writer")?.expires_at;
        assert!(expiry >= released_at + renewed);
        Ok(())
    })?;
    Ok(())
}

#[test]
fn source_admission_stays_durable_without_leaking_private_patterns() -> TestResult {
    const PRIVATE: &str = "private-customer-sentinel/**";
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let writer = store.begin_revision(
        GenerationId::from_uuid_v8([1; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    store.seal_with_admission(
        &writer,
        identity(2),
        GenerationDigestVersion::V23,
        vec![PRIVATE.to_owned()],
        Some(1024),
    )?;
    let revision = store.publish(&writer)?;
    assert!(!format!("{revision:?}").contains(PRIVATE));
    assert!(!serde_json::to_string(&revision)?.contains(PRIVATE));
    drop(store);
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let current = store.current_snapshot(None)?;
    assert_eq!(current.revision().run_excludes, [PRIVATE]);
    assert_eq!(current.revision().maximum_file_bytes, Some(1024));
    Ok(())
}

#[test]
fn reused_lookup_keeps_exact_keys_and_versions_across_reverse_and_missing_seeks() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let first = store.begin_revision(
        GenerationId::from_uuid_v8([1; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    let row = |name: &[u8], value: &[u8]| -> TestResult<RecordChange> {
        Ok(RecordChange::put(
            Collection::Graph,
            RecordKey::new(name.to_vec())?,
            value.to_vec(),
        ))
    };
    store.stage(
        &first,
        &[
            row(b"a", b"old")?,
            row(b"a\0b", b"binary")?,
            row(b"z", b"last")?,
        ],
    )?;
    store.seal(&first, identity(2), GenerationDigestVersion::V23)?;
    store.publish(&first)?;
    let failed = store.begin_revision(
        GenerationId::from_uuid_v8([2; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    store.stage(
        &failed,
        &[row(b"a", b"hidden")?, row(b"missing", b"hidden")?],
    )?;
    store.fail(&failed)?;
    let current = store.begin_revision(
        GenerationId::from_uuid_v8([3; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    store.stage(
        &current,
        &[
            row(b"a", b"new")?,
            RecordChange::delete(Collection::Graph, RecordKey::new(b"z".to_vec())?),
        ],
    )?;
    store.seal(&current, identity(3), GenerationDigestVersion::V23)?;
    store.publish(&current)?;
    let snapshot = store.inner.database.snapshot();
    let family = store.family(Collection::Graph)?;
    let meta = store.meta()?;
    let mut iterator = snapshot.raw_iterator_cf(&family);
    let mut budget = ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?;
    for (sequence, key, expected) in [
        (3, b"z".as_slice(), None),
        (3, b"a", Some(b"new".as_slice())),
        (3, b"a\0b", Some(b"binary".as_slice())),
        (3, b"missing", None),
        (3, b"a\0", None),
        (1, b"z", Some(b"last".as_slice())),
        (1, b"a", Some(b"old".as_slice())),
        (0, b"a", None),
    ] {
        assert_eq!(
            read_record_from(
                &snapshot,
                &mut iterator,
                &meta,
                sequence,
                &RecordKey::new(key.to_vec())?,
                &mut budget
            )?
            .as_deref(),
            expected,
        );
    }
    Ok(())
}
