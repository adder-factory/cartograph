//! Failure cases found during independent review of the native owner.

use std::{collections::BTreeMap, path::Path, sync::mpsc};

use super::{
    Collection, Duration, GenerationDigestVersion, ProjectStore, ReadBudget, RecordChange,
    RocksError, StorePolicy, TestResult, budget, generation, identity, key, publish, store,
};

fn scan_history(native: &ProjectStore) -> TestResult {
    for sequence in 1..=16 {
        let mut changes = vec![
            RecordChange::put(
                Collection::Graph,
                key(b"scan/a")?,
                vec![u8::try_from(sequence)?; 8192],
            ),
            RecordChange::put(Collection::Graph, key(b"scan/b")?, vec![42; 8192]),
        ];
        if sequence == 1 {
            changes.extend([
                RecordChange::put(Collection::Graph, key(b"scan/a\0child")?, b"child".to_vec()),
                RecordChange::put(Collection::Graph, key(b"scan/z")?, b"tail".to_vec()),
            ]);
        }
        publish(native, sequence, &changes)?;
    }
    publish(
        native,
        17,
        &[
            RecordChange::put(Collection::Graph, key(b"scan/a")?, b"current".to_vec()),
            RecordChange::delete(Collection::Graph, key(b"scan/b")?),
        ],
    )?;
    // Failed versions below the next current sequence must still be ignored,
    // including one that would resurrect a deleted record.
    let failed = native.begin_revision(generation(18), identity(1), Duration::from_secs(30))?;
    native.stage(
        &failed,
        &[
            RecordChange::put(Collection::Graph, key(b"scan/a")?, b"failed".to_vec()),
            RecordChange::put(Collection::Graph, key(b"scan/b")?, b"resurrected".to_vec()),
        ],
    )?;
    native.fail(&failed)?;
    publish(
        native,
        19,
        &[RecordChange::put(
            Collection::Graph,
            key(b"scan/z")?,
            b"new tail".to_vec(),
        )],
    )?;
    Ok(())
}

#[test]
fn current_scan_seeks_past_shadowed_versions_without_resurrecting_tombstones() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    scan_history(&native)?;
    let snapshot = native.current_snapshot(None)?;
    let mut complete_allowance = ReadBudget::new(16, 1024, Duration::from_secs(10))?;
    let complete = snapshot.scan_prefix(
        Collection::Graph,
        b"scan/",
        None,
        10,
        &mut complete_allowance,
    )?;
    assert_eq!(
        complete.rows,
        [
            (key(b"scan/a")?, b"current".to_vec()),
            (key(b"scan/a\0child")?, b"child".to_vec()),
            (key(b"scan/z")?, b"new tail".to_vec()),
        ]
    );
    assert!(complete.next_after.is_none());
    let mut allowance = ReadBudget::new(16, 1024, Duration::from_secs(10))?;
    let first = snapshot.scan_prefix(Collection::Graph, b"scan/", None, 1, &mut allowance)?;
    assert_eq!(first.rows, [(key(b"scan/a")?, b"current".to_vec())]);
    let rest = snapshot.scan_prefix(
        Collection::Graph,
        b"scan/",
        first.next_after.as_ref(),
        10,
        &mut allowance,
    )?;
    assert_eq!(
        rest.rows,
        [
            (key(b"scan/a\0child")?, b"child".to_vec()),
            (key(b"scan/z")?, b"new tail".to_vec()),
        ]
    );
    assert!(rest.next_after.is_none());
    Ok(())
}

#[test]
fn retained_scan_seeks_past_future_versions_and_keeps_the_exact_pinned_value() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    scan_history(&native)?;
    let snapshot = native.retained_snapshot(&generation(8))?;
    let mut allowance = ReadBudget::new(16, 24 * 1024, Duration::from_secs(10))?;
    let page = snapshot.scan_prefix(Collection::Graph, b"scan/", None, 10, &mut allowance)?;
    assert_eq!(
        page.rows,
        [
            (key(b"scan/a")?, vec![8; 8192]),
            (key(b"scan/a\0child")?, b"child".to_vec()),
            (key(b"scan/b")?, vec![42; 8192]),
            (key(b"scan/z")?, b"tail".to_vec()),
        ]
    );
    assert!(page.next_after.is_none());
    Ok(())
}

#[cfg(unix)]
#[test]
fn existing_permissive_native_directories_are_private_before_any_payload_is_written() -> TestResult
{
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    for mode in [0o755, 0o777] {
        let root = tempfile::tempdir()?;
        let state = root.path().join(".cartograph");
        let data = state.join("data");
        std::fs::create_dir_all(&data)?;
        for path in [&state, &data] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        let native = store(root.path())?;
        for path in [&state, &data] {
            assert_eq!(std::fs::metadata(path)?.mode() & 0o7777, 0o700);
        }
        publish(&native, 1, &[put(b"private graph payload")?])?;
        drop(native);
        // Existing native files must receive the same protected admission on
        // reopen, rather than relying only on permissions during first use.
        for path in [&state, &data] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        let reopened = store(root.path())?;
        for path in [&state, &data] {
            assert_eq!(std::fs::metadata(path)?.mode() & 0o7777, 0o700);
        }
        assert_eq!(
            reopened.current_snapshot(None)?.get(
                Collection::Graph,
                &key(b"moving")?,
                &mut budget()?
            )?,
            Some(b"private graph payload".to_vec())
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlinked_primary_lock_is_refused_without_touching_its_target() -> TestResult {
    let root = tempfile::tempdir()?;
    let foreign = tempfile::NamedTempFile::new()?;
    std::fs::write(foreign.path(), b"preserve foreign lock target")?;
    let state = root.path().join(".cartograph");
    std::fs::create_dir(&state)?;
    std::os::unix::fs::symlink(foreign.path(), state.join("native-primary.lock"))?;
    assert!(
        matches!(store(root.path()), Err(error) if error.downcast_ref::<RocksError>() == Some(&RocksError::Directory))
    );
    assert_eq!(
        std::fs::read(foreign.path())?,
        b"preserve foreign lock target"
    );
    assert!(!state.join("data").exists());
    Ok(())
}

#[test]
fn an_empty_published_base_ignores_failed_payloads_and_keeps_noop_replay_claims() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    let abandoned = native.begin_revision(generation(1), identity(1), Duration::from_secs(30))?;
    native.stage(&abandoned, &[put(b"never published")?])?;
    native.fail(&abandoned)?;
    let retry = native.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    assert_eq!(
        native
            .stage(
                &retry,
                &[RecordChange::delete(Collection::Graph, key(b"moving")?)]
            )?
            .changed_rows,
        0,
    );
    assert!(matches!(
        native.stage(&retry, &[put(b"conflicting replay")?]),
        Err(RocksError::ReplayConflict)
    ));
    native.fail(&retry)?;
    assert_eq!(publish(&native, 3, &[put(b"first complete value")?])?, 1);
    assert_eq!(
        native
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"moving")?, &mut budget()?)?,
        Some(b"first complete value".to_vec())
    );
    drop(native);
    assert_eq!(
        store(root.path())?.current_snapshot(None)?.get(
            Collection::Graph,
            &key(b"moving")?,
            &mut budget()?
        )?,
        Some(b"first complete value".to_vec())
    );
    Ok(())
}

#[test]
fn bounded_retirement_never_exposes_an_incomplete_historical_graph() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(&store, 1, &[put(b"one")?])?;
    let pinned = store.current_snapshot(None)?;
    publish(&store, 2, &[put(b"two")?])?;
    publish(&store, 3, &[put(b"three")?])?;
    let report = store.prune(0, 1, 100, &mut budget()?)?;
    assert_eq!(report.generations_removed, 1);
    assert!(!report.complete);
    // Generation one still has metadata because retirement is bounded. Its
    // logical readability is revoked in the same batch that removes versions.
    for keep in [2, 0, 3, 0] {
        for (sequence, value) in [(1, b"one".as_slice()), (2, b"two"), (3, b"three")] {
            match store.retained_snapshot(&generation(sequence)) {
                Ok(snapshot) => assert_eq!(
                    snapshot.get(Collection::Graph, &key(b"moving")?, &mut budget()?)?,
                    Some(value.to_vec())
                ),
                Err(RocksError::GenerationUnavailable) if sequence < 3 => {}
                Err(error) => return Err(error.into()),
            }
        }
        assert_eq!(
            pinned.get(Collection::Graph, &key(b"moving")?, &mut budget()?)?,
            Some(b"one".to_vec())
        );
        store.prune(keep, 1, 1, &mut budget()?)?;
    }
    assert!(matches!(
        store.retained_snapshot(&generation(1)),
        Err(RocksError::GenerationUnavailable)
    ));
    Ok(())
}

#[test]
fn rejected_identity_and_format_do_not_change_native_database_files() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    publish(&native, 1, &[put(b"one")?])?;
    drop(native);
    let directory = root.path().join(".cartograph/data");
    let before = files(&directory)?;
    assert!(matches!(
        ProjectStore::open(root.path(), &identity(8), StorePolicy::default()),
        Err(RocksError::ProjectMismatch)
    ));
    assert_eq!(files(&directory)?, before);
    std::fs::write(
        directory.join("CARTOGRAPH_OWNER"),
        b"unsupported owner format",
    )?;
    let before = files(&directory)?;
    assert!(matches!(
        ProjectStore::open(root.path(), &identity(7), StorePolicy::default()),
        Err(RocksError::Format)
    ));
    assert_eq!(files(&directory)?, before);
    Ok(())
}

#[test]
fn unowned_database_with_nondefault_rows_is_refused_before_writable_open() -> TestResult {
    let root = tempfile::tempdir()?;
    let directory = root.path().join(".cartograph/data");
    std::fs::create_dir_all(&directory)?;
    let mut options = rocksdb::Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    let native = rocksdb::DB::open_cf(&options, &directory, ["graph"])?;
    let graph = native.cf_handle("graph").ok_or("missing graph family")?;
    native.put_cf(&graph, b"foreign", b"preserve")?;
    drop(graph);
    drop(native);
    let before = files(&directory)?;
    assert!(matches!(
        ProjectStore::open(root.path(), &identity(7), StorePolicy::default()),
        Err(RocksError::Format)
    ));
    assert_eq!(files(&directory)?, before);
    assert_eq!(
        rocksdb::DB::list_cf(&options, &directory)?,
        vec!["default", "graph"]
    );
    Ok(())
}

#[test]
fn incompatible_inner_format_is_rejected_without_native_writes() -> TestResult {
    let root = tempfile::tempdir()?;
    drop(store(root.path())?);
    let directory = root.path().join(".cartograph/data");
    let options = rocksdb::Options::default();
    let names = rocksdb::DB::list_cf(&options, &directory)?;
    let native = rocksdb::DB::open_cf(&options, &directory, names)?;
    native.put(b"format", b"incompatible-native-format")?;
    drop(native);
    let before = files(&directory)?;
    assert!(matches!(
        ProjectStore::open(root.path(), &identity(7), StorePolicy::default()),
        Err(RocksError::Format)
    ));
    assert_eq!(files(&directory)?, before);
    Ok(())
}

#[test]
fn interrupted_marker_preparation_does_not_block_an_identical_retry() -> TestResult {
    use std::io::Write;
    let root = tempfile::tempdir()?;
    let state = root.path().join(".cartograph");
    let directory = state.join("data");
    std::fs::create_dir_all(&directory)?;
    let mut pending = tempfile::Builder::new()
        .prefix(".cartograph-rocks-owner-")
        .tempfile_in(&state)?;
    pending.write_all(b"Cartograph Rocks")?;
    pending.as_file().sync_all()?;
    // Leave the same partial sibling an interrupted process could leave. The
    // authoritative marker and native database have not been created yet.
    let (file, pending_path) = pending.keep()?;
    drop(file);
    assert!(!directory.join("CARTOGRAPH_OWNER").exists());
    let native = store(root.path())?;
    publish(&native, 1, &[put(b"complete")?])?;
    drop(native);
    let reopened = store(root.path())?;
    assert_eq!(reopened.current_snapshot(None)?.revision().sequence, 1);
    assert_eq!(std::fs::read(pending_path)?, b"Cartograph Rocks");
    Ok(())
}

#[test]
fn interrupted_empty_owned_column_family_catalog_is_completed_on_retry() -> TestResult {
    let root = tempfile::tempdir()?;
    let directory = partial_catalog(root.path(), "graph", false)?;
    let native = store(root.path())?;
    publish(&native, 1, &[put(b"complete")?])?;
    drop(native);
    let names = rocksdb::DB::list_cf(&rocksdb::Options::default(), &directory)?;
    assert_eq!(names.len(), 8);
    let reopened = store(root.path())?;
    assert_eq!(
        reopened.current_snapshot(None)?.get(
            Collection::Graph,
            &key(b"moving")?,
            &mut budget()?
        )?,
        Some(b"complete".to_vec())
    );
    Ok(())
}

#[test]
fn partial_owned_catalog_never_adopts_populated_or_unknown_families() -> TestResult {
    for (family, populated) in [("graph", true), ("foreign", false)] {
        let root = tempfile::tempdir()?;
        let directory = partial_catalog(root.path(), family, populated)?;
        let before = files(&directory)?;
        assert!(matches!(
            ProjectStore::open(root.path(), &identity(7), StorePolicy::default()),
            Err(RocksError::Format)
        ));
        assert_eq!(files(&directory)?, before);
    }
    Ok(())
}

fn partial_catalog(root: &Path, family: &str, populated: bool) -> TestResult<std::path::PathBuf> {
    let directory = root.join(".cartograph/data");
    std::fs::create_dir_all(&directory)?;
    let mut marker = b"Cartograph RocksDB owner\ncartograph-rocks-v1\n".to_vec();
    marker.extend_from_slice(identity(7).as_str().as_bytes());
    marker.push(b'\n');
    std::fs::write(directory.join("CARTOGRAPH_OWNER"), marker)?;
    let mut options = rocksdb::Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    let native = rocksdb::DB::open_cf(&options, &directory, [family])?;
    if populated {
        let family = native.cf_handle(family).ok_or("missing native family")?;
        native.put_cf(&family, b"foreign", b"preserve")?;
    }
    drop(native);
    Ok(directory)
}

#[test]
fn retention_commits_partial_progress_when_long_keys_fill_its_batch() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(
        root.path(),
        &identity(7),
        StorePolicy {
            maximum_batch_bytes: 1_048_576,
            ..StorePolicy::default()
        },
    )?;
    for sequence in 1..=3_u64 {
        let writer =
            store.begin_revision(generation(sequence), identity(1), Duration::from_secs(30))?;
        for chunk in 0..12_u64 {
            let mut changes = Vec::new();
            for row in 0..25_u64 {
                let mut bytes = vec![b'x'; 4_096];
                bytes[..8].copy_from_slice(&(chunk * 25 + row).to_be_bytes());
                changes.push(RecordChange::put(
                    Collection::Graph,
                    key(&bytes)?,
                    sequence.to_be_bytes().to_vec(),
                ));
            }
            store.stage(&writer, &changes)?;
        }
        store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
        store.publish(&writer)?;
    }
    let mut removed = 0;
    let mut completed = false;
    for _ in 0..20 {
        let report = store.prune(0, 1, 10_000, &mut budget()?)?;
        removed += report.versions_removed;
        if report.complete {
            completed = true;
            break;
        }
    }
    assert!(completed);
    assert_eq!(removed, 600);
    let snapshot = store.current_snapshot(None)?;
    let mut count = 0;
    let mut after = None;
    let mut read_budget = budget()?;
    loop {
        let page =
            snapshot.scan_prefix(Collection::Graph, b"", after.as_ref(), 50, &mut read_budget)?;
        assert!(
            page.rows
                .iter()
                .all(|(_, value)| value == &3_u64.to_be_bytes())
        );
        count += page.rows.len();
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(count, 300);
    Ok(())
}

#[test]
fn unchanged_and_changed_staging_values_conflict_in_both_orders() -> TestResult {
    for (first, second) in [
        (b"base".as_slice(), b"changed".as_slice()),
        (b"changed", b"base"),
    ] {
        let root = tempfile::tempdir()?;
        let store = store(root.path())?;
        publish(&store, 1, &[put(b"base")?])?;
        let writer = store.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
        store.stage(&writer, &[put(first)?])?;
        assert_eq!(
            store.stage(&writer, &[put(second)?]),
            Err(RocksError::ReplayConflict)
        );
        store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
        store.publish(&writer)?;
        assert_eq!(
            store.current_snapshot(None)?.get(
                Collection::Graph,
                &key(b"moving")?,
                &mut budget()?
            )?,
            Some(first.to_vec())
        );
    }
    Ok(())
}

#[test]
fn unchanged_overlapping_staging_and_noop_deletions_claim_their_logical_keys() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(&store, 1, &[put(b"base")?])?;
    let writer = store.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    let (start, started) = mpsc::sync_channel(1);
    let unchanged = put(b"base")?;
    let changed = put(b"changed")?;
    let outcomes = std::thread::scope(|scope| {
        let store = &store;
        let writer = &writer;
        let first = scope.spawn(move || {
            let _sent = start.send(());
            store.stage(writer, &[unchanged])
        });
        let second = scope.spawn(move || {
            started
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| RocksError::Cancelled)?;
            store.stage(writer, &[changed])
        });
        (first.join(), second.join())
    });
    let first = outcomes.0.map_err(|_| "staging worker panicked")?;
    let second = outcomes.1.map_err(|_| "staging worker panicked")?;
    assert!(matches!(
        (first, second),
        (Ok(_), Err(RocksError::ReplayConflict)) | (Err(RocksError::ReplayConflict), Ok(_))
    ));
    assert_eq!(
        store
            .stage(
                &writer,
                &[RecordChange::delete(Collection::Graph, key(b"absent")?)]
            )?
            .changed_rows,
        store
            .stage(
                &writer,
                &[RecordChange::delete(Collection::Graph, key(b"absent")?)]
            )?
            .changed_rows
    );
    assert_eq!(
        store.stage(
            &writer,
            &[RecordChange::put(
                Collection::Graph,
                key(b"absent")?,
                b"late".to_vec()
            )]
        ),
        Err(RocksError::ReplayConflict)
    );
    store.fail(&writer)?;
    Ok(())
}

fn put(value: &[u8]) -> TestResult<RecordChange> {
    Ok(RecordChange::put(
        Collection::Graph,
        key(b"moving")?,
        value.to_vec(),
    ))
}

fn files(directory: &Path) -> TestResult<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        files.insert(
            entry.file_name().to_string_lossy().into_owned(),
            std::fs::read(entry.path())?,
        );
    }
    Ok(files)
}
