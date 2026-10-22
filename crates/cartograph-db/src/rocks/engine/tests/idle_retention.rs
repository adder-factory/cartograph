use super::*;

fn budget() -> TestResult<ReadBudget> {
    Ok(ReadBudget::new(1_000, 1_048_576, Duration::from_secs(10))?)
}

fn published(store: &ProjectStore, sequence: u8) -> TestResult {
    let writer = store.begin_revision(
        GenerationId::from_uuid_v8([sequence; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    store.stage(
        &writer,
        &[RecordChange::put(
            Collection::Graph,
            RecordKey::new(b"value".to_vec())?,
            vec![sequence],
        )],
    )?;
    store.seal(&writer, identity(2), GenerationDigestVersion::CURRENT)?;
    store.publish(&writer)?;
    Ok(())
}

#[test]
fn unchanged_retention_does_not_write_the_wal_even_after_mutable_state_changes() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    published(&store, 1)?;
    assert!(store.prune(2, 4, 100, &mut budget()?)?.complete);
    store.state(None, 4_096, 1, &mut budget()?, |transaction, _| {
        transaction.put(
            state::StateNamespace::Sessions,
            b"session",
            b"active".to_vec(),
        )
    })?;
    let before = store.inner.database.latest_sequence_number();
    let report = store.prune(2, 4, 100, &mut budget()?)?;
    assert!(report.complete);
    assert_eq!(report.versions_visited, 0);
    assert_eq!(store.inner.database.latest_sequence_number(), before);
    Ok(())
}

#[test]
fn completed_retention_still_checks_cancellation_after_waiting_for_its_lock() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    published(&store, 1)?;
    store.prune(2, 4, 100, &mut budget()?)?;
    let before = store.inner.database.latest_sequence_number();
    let request = StorageRequest::new(Duration::from_secs(10))?;
    let controlled = store.with_control(request.clone());
    let result = std::thread::scope(|scope| -> TestResult<Result<PruneReport>> {
        let held = store.lock_mutation()?;
        let (entered, started) = mpsc::sync_channel(1);
        let task = scope.spawn(move || {
            entered.send(()).map_err(|_| operation("test_gate"))?;
            let mut allowance = ReadBudget::new(100, 1_048_576, Duration::from_secs(10))?;
            controlled.prune(2, 4, 100, &mut allowance)
        });
        started.recv_timeout(Duration::from_secs(2))?;
        request.cancel();
        drop(held);
        Ok(task.join().map_err(|_| "retention worker panicked")?)
    })?;
    assert_eq!(result, Err(RocksError::Cancelled));
    assert_eq!(store.inner.database.latest_sequence_number(), before);
    let mut cancelled_budget = budget()?.with_control(request);
    assert_eq!(
        store.prune(2, 4, 100, &mut cancelled_budget),
        Err(RocksError::Cancelled)
    );
    Ok(())
}

#[test]
fn completed_retention_terminalizes_an_expired_writer_and_collects_its_rows() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    published(&store, 1)?;
    store.prune(2, 4, 100, &mut budget()?)?;
    let abandoned = store.begin_revision(
        GenerationId::from_uuid_v8([2; 16]),
        identity(1),
        Duration::from_secs(30),
    )?;
    store.stage(
        &abandoned,
        &[RecordChange::put(
            Collection::Graph,
            RecordKey::new(b"failed".to_vec())?,
            vec![2],
        )],
    )?;
    store
        .lock_writer()?
        .as_mut()
        .ok_or("writer missing")?
        .expires_at = Instant::now();
    let report = store.prune(2, 4, 100, &mut budget()?)?;
    assert!(report.complete);
    assert_eq!(report.versions_removed, 1);
    assert!(store.lock_writer()?.is_none());
    assert!(store.stage(&abandoned, &[]).is_err());
    assert_eq!(store.current_snapshot(None)?.revision().sequence, 1);
    assert_eq!(store.prune(2, 4, 100, &mut budget()?)?.versions_visited, 0);
    Ok(())
}

#[test]
fn older_executable_retention_progress_invalidates_a_saved_completion_marker() -> TestResult {
    const COMPLETED: &[u8] = b"retention/completed-v1";
    for changed_floor in [false, true] {
        let root = tempfile::tempdir()?;
        let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
        published(&store, 1)?;
        published(&store, 2)?;
        store.prune(1, 4, 100, &mut budget()?)?;
        let saved = store.get_meta(COMPLETED)?.ok_or("completion missing")?;
        let meta = store.meta()?;
        let mut batch = WriteBatch::default();
        batch.delete_cf(&meta, COMPLETED);
        store.write(&batch)?;
        // An older executable changes the floor or leaves a cursor without
        // updating the completion marker introduced by this executable.
        store.prune(u8::from(!changed_floor), 4, 1, &mut budget()?)?;
        let mut batch = WriteBatch::default();
        batch.put_cf(&meta, COMPLETED, saved);
        store.write(&batch)?;
        let report = store.prune(1, 4, 100, &mut budget()?)?;
        assert!(report.complete);
        assert!(report.versions_visited > 0);
        assert!(store.get_meta(b"retention/cursor")?.is_none());
        assert_eq!(store.prune(1, 4, 100, &mut budget()?)?.versions_visited, 0);
    }
    Ok(())
}

#[test]
fn malformed_completion_marker_fails_closed() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    let mut batch = WriteBatch::default();
    batch.put_cf(&store.meta()?, b"retention/completed-v1", b"{}");
    store.write(&batch)?;
    assert_eq!(
        store.prune(2, 4, 100, &mut budget()?),
        Err(RocksError::Corrupt {
            field: "retention_completed"
        })
    );
    Ok(())
}

#[test]
fn legacy_cursor_without_a_reservation_watermark_wraps_before_completion() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    published(&store, 1)?;
    assert!(!store.prune(2, 4, 1, &mut budget()?)?.complete);
    let bytes = store
        .get_meta(b"retention/cursor")?
        .ok_or("cursor missing")?;
    let mut legacy: serde_json::Value = serde_json::from_slice(&bytes)?;
    legacy
        .as_object_mut()
        .ok_or("cursor must be an object")?
        .remove("reservation_sequence");
    let mut batch = WriteBatch::default();
    batch.put_cf(
        &store.meta()?,
        b"retention/cursor",
        serde_json::to_vec(&legacy)?,
    );
    store.write(&batch)?;
    let report = store.prune(2, 4, 100, &mut budget()?)?;
    assert!(!report.complete);
    assert_eq!(report.versions_visited, 0);
    let report = store.prune(2, 4, 100, &mut budget()?)?;
    assert!(report.complete);
    assert_eq!(report.versions_visited, 1);
    assert_eq!(store.prune(2, 4, 100, &mut budget()?)?.versions_visited, 0);
    Ok(())
}

#[test]
fn cursor_from_a_future_reservation_fails_closed() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &identity(7), StorePolicy::default())?;
    published(&store, 1)?;
    assert!(!store.prune(2, 4, 1, &mut budget()?)?.complete);
    let bytes = store
        .get_meta(b"retention/cursor")?
        .ok_or("cursor missing")?;
    let mut cursor: serde_json::Value = serde_json::from_slice(&bytes)?;
    cursor["reservation_sequence"] = serde_json::json!(2);
    let mut batch = WriteBatch::default();
    batch.put_cf(
        &store.meta()?,
        b"retention/cursor",
        serde_json::to_vec(&cursor)?,
    );
    store.write(&batch)?;
    assert_eq!(
        store.prune(2, 4, 100, &mut budget()?),
        Err(RocksError::Corrupt {
            field: "retention_cursor"
        })
    );
    Ok(())
}
