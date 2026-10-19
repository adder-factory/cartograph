//! Native `RocksDB` project isolation, publication, durability, and retention contracts.

#[path = "rocks_project/concurrency.rs"]
mod concurrency;
mod dependency_ownership;
#[path = "rocks_project/regressions.rs"]
mod regressions;

use std::{error::Error, path::Path, time::Duration};

use cartograph_db::rocks::{
    Collection, ProjectStore, ReadBudget, RecordChange, RecordKey, RocksError, StorePolicy,
};
use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn identity(byte: u8) -> ContentDigest {
    ContentDigest::from_bytes([byte; 32])
}

fn generation(sequence: u64) -> GenerationId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&sequence.to_be_bytes());
    GenerationId::from_uuid_v8(bytes)
}

fn key(value: &[u8]) -> TestResult<RecordKey> {
    Ok(RecordKey::new(value.to_vec())?)
}

fn budget() -> TestResult<ReadBudget> {
    Ok(ReadBudget::new(
        100_000,
        64 * 1_048_576,
        Duration::from_secs(10),
    )?)
}

fn store(root: &Path) -> TestResult<ProjectStore> {
    Ok(ProjectStore::open(
        root,
        &identity(7),
        StorePolicy::default(),
    )?)
}

fn publish(store: &ProjectStore, sequence: u64, changes: &[RecordChange]) -> TestResult<u64> {
    let writer =
        store.begin_revision(generation(sequence), identity(1), Duration::from_secs(30))?;
    let staged = store.stage(&writer, changes)?;
    store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
    store.publish(&writer)?;
    Ok(staged.changed_rows)
}

#[test]
fn projects_have_independent_primary_handles_and_data() -> TestResult {
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    let first_store = store(first.path())?;
    let second_store = store(second.path())?;
    publish(
        &first_store,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"symbol/a")?,
            b"first".to_vec(),
        )],
    )?;
    publish(
        &second_store,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"symbol/a")?,
            b"second".to_vec(),
        )],
    )?;
    assert_eq!(
        first_store.current_snapshot(None)?.get(
            Collection::Graph,
            &key(b"symbol/a")?,
            &mut budget()?
        )?,
        Some(b"first".to_vec())
    );
    assert_eq!(
        second_store.current_snapshot(None)?.get(
            Collection::Graph,
            &key(b"symbol/a")?,
            &mut budget()?
        )?,
        Some(b"second".to_vec())
    );
    assert!(matches!(
        ProjectStore::open(first.path(), &identity(7), StorePolicy::default()),
        Err(RocksError::WriterBusy)
    ));
    drop(first_store);
    assert!(matches!(
        ProjectStore::open(first.path(), &identity(8), StorePolicy::default()),
        Err(RocksError::ProjectMismatch)
    ));
    Ok(())
}

#[test]
fn atomic_publication_spans_graph_and_search_and_reuses_unchanged_rows() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(
        &store,
        1,
        &[
            RecordChange::put(Collection::Graph, key(b"symbol/a")?, b"version1".to_vec()),
            RecordChange::put(
                Collection::Search,
                key(b"posting/ready/a")?,
                b"version1".to_vec(),
            ),
            RecordChange::put(
                Collection::Graph,
                key(b"symbol/unchanged")?,
                b"same".to_vec(),
            ),
        ],
    )?;
    let before = store.current_snapshot(None)?;
    let writer = store.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    let changes = [
        RecordChange::put(Collection::Graph, key(b"symbol/a")?, b"version2".to_vec()),
        RecordChange::put(
            Collection::Search,
            key(b"posting/ready/a")?,
            b"version2".to_vec(),
        ),
        RecordChange::put(
            Collection::Graph,
            key(b"symbol/unchanged")?,
            b"same".to_vec(),
        ),
    ];
    assert_eq!(store.stage(&writer, &changes)?.changed_rows, 2);
    assert_eq!(store.stage(&writer, &changes)?.changed_rows, 2);
    assert_eq!(
        store.current_snapshot(None)?.revision().generation_id,
        generation(1)
    );
    store.seal(&writer, identity(2), GenerationDigestVersion::V23)?;
    assert_eq!(
        store.current_snapshot(None)?.revision().generation_id,
        generation(1)
    );
    store.publish(&writer)?;
    let after = store.current_snapshot(Some(&generation(2)))?;
    for (collection, name) in [
        (Collection::Graph, b"symbol/a".as_slice()),
        (Collection::Search, b"posting/ready/a".as_slice()),
    ] {
        assert_eq!(
            before.get(collection, &key(name)?, &mut budget()?)?,
            Some(b"version1".to_vec())
        );
        assert_eq!(
            after.get(collection, &key(name)?, &mut budget()?)?,
            Some(b"version2".to_vec())
        );
    }
    assert_eq!(
        after.get(
            Collection::Graph,
            &key(b"symbol/unchanged")?,
            &mut budget()?
        )?,
        Some(b"same".to_vec())
    );
    assert!(matches!(
        store.current_snapshot(Some(&generation(1))),
        Err(RocksError::GenerationChanged)
    ));
    assert_eq!(
        store.retained_snapshot(&generation(1))?.get(
            Collection::Graph,
            &key(b"symbol/a")?,
            &mut budget()?
        )?,
        Some(b"version1".to_vec())
    );
    Ok(())
}

#[test]
fn failed_versions_and_deletions_do_not_leak_into_later_revisions() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(
        &store,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"a")?,
            b"original".to_vec(),
        )],
    )?;
    let failed = store.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    store.stage(
        &failed,
        &[
            RecordChange::put(Collection::Graph, key(b"a")?, b"unpublished".to_vec()),
            RecordChange::put(Collection::Graph, key(b"b")?, b"unpublished".to_vec()),
        ],
    )?;
    store.fail(&failed)?;
    publish(
        &store,
        3,
        &[RecordChange::put(
            Collection::Graph,
            key(b"c")?,
            b"visible".to_vec(),
        )],
    )?;
    let current = store.current_snapshot(None)?;
    assert_eq!(
        current.get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        Some(b"original".to_vec())
    );
    assert_eq!(
        current.get(Collection::Graph, &key(b"b")?, &mut budget()?)?,
        None
    );
    assert_eq!(
        current
            .scan_prefix(Collection::Graph, b"", None, 10, &mut budget()?)?
            .rows,
        vec![
            (key(b"a")?, b"original".to_vec()),
            (key(b"c")?, b"visible".to_vec())
        ]
    );
    publish(
        &store,
        4,
        &[RecordChange::delete(Collection::Graph, key(b"a")?)],
    )?;
    assert_eq!(
        store
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        None
    );
    assert_eq!(
        current.get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        Some(b"original".to_vec())
    );
    Ok(())
}

#[test]
fn writer_fences_and_batch_limits_fail_before_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = StorePolicy {
        maximum_batch_rows: 1,
        ..StorePolicy::default()
    };
    let store = ProjectStore::open(root.path(), &identity(7), policy)?;
    let writer = store.begin_revision(generation(1), identity(1), Duration::from_secs(30))?;
    assert!(matches!(
        store.begin_revision(generation(2), identity(1), Duration::from_secs(30)),
        Err(RocksError::WriterBusy)
    ));
    assert!(matches!(
        store.stage(
            &writer,
            &[
                RecordChange::put(Collection::Graph, key(b"a")?, b"one".to_vec()),
                RecordChange::put(Collection::Graph, key(b"b")?, b"two".to_vec()),
            ]
        ),
        Err(RocksError::Limit {
            resource: "batch_rows"
        })
    ));
    assert!(matches!(
        store.publish(&writer),
        Err(RocksError::Transition)
    ));
    store.stage(
        &writer,
        &[RecordChange::put(
            Collection::Graph,
            key(b"a")?,
            b"one".to_vec(),
        )],
    )?;
    assert!(matches!(
        store.stage(
            &writer,
            &[RecordChange::put(
                Collection::Graph,
                key(b"a")?,
                b"different".to_vec()
            )]
        ),
        Err(RocksError::ReplayConflict)
    ));
    store.fail(&writer)?;
    let replacement = store.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    assert_eq!(
        store.heartbeat(&writer, Duration::from_secs(30)),
        Err(RocksError::FenceLost)
    );
    assert!(matches!(
        store.stage(&writer, &[]),
        Err(RocksError::FenceLost)
    ));
    store.fail(&replacement)?;
    Ok(())
}

#[test]
fn reopen_terminalizes_interrupted_work_and_retains_acknowledged_graph() -> TestResult {
    let root = tempfile::tempdir()?;
    let writer;
    {
        let store = store(root.path())?;
        publish(
            &store,
            1,
            &[RecordChange::put(
                Collection::Graph,
                key(b"a")?,
                b"durable".to_vec(),
            )],
        )?;
        writer = store.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
        store.stage(
            &writer,
            &[RecordChange::put(
                Collection::Graph,
                key(b"a")?,
                b"incomplete".to_vec(),
            )],
        )?;
    }
    let store = store(root.path())?;
    assert_eq!(
        store
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        Some(b"durable".to_vec())
    );
    assert_eq!(
        store.heartbeat(&writer, Duration::from_secs(30)),
        Err(RocksError::FenceLost)
    );
    publish(&store, 3, &[])?;
    assert_eq!(
        store
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        Some(b"durable".to_vec())
    );
    Ok(())
}

#[test]
fn snapshot_pagination_is_binary_safe_and_ignores_other_prefixes() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(
        &store,
        1,
        &[
            RecordChange::put(Collection::Graph, key(b"edge\0a\0b")?, b"first".to_vec()),
            RecordChange::put(Collection::Graph, key(b"edge\0a\0c")?, b"second".to_vec()),
            RecordChange::put(Collection::Graph, key(b"edge\0b\0c")?, b"outside".to_vec()),
        ],
    )?;
    let snapshot = store.current_snapshot(None)?;
    let first = snapshot.scan_prefix(Collection::Graph, b"edge\0a\0", None, 1, &mut budget()?)?;
    assert_eq!(first.rows, vec![(key(b"edge\0a\0b")?, b"first".to_vec())]);
    let second = snapshot.scan_prefix(
        Collection::Graph,
        b"edge\0a\0",
        first.next_after.as_ref(),
        1,
        &mut budget()?,
    )?;
    assert_eq!(second.rows, vec![(key(b"edge\0a\0c")?, b"second".to_vec())]);
    assert_eq!(
        snapshot
            .scan_prefix(
                Collection::Graph,
                b"edge\0a\0",
                second.next_after.as_ref(),
                1,
                &mut budget()?
            )?
            .rows,
        Vec::new()
    );
    assert!(matches!(
        snapshot.get(
            Collection::Graph,
            &key(b"edge\0a\0b")?,
            &mut ReadBudget::new(1, 1, Duration::from_secs(1))?
        ),
        Err(RocksError::Limit {
            resource: "read_bytes"
        })
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn project_storage_refuses_symlinked_data_directory() -> TestResult {
    let root = tempfile::tempdir()?;
    let foreign = tempfile::tempdir()?;
    std::fs::create_dir(root.path().join(".cartograph"))?;
    std::os::unix::fs::symlink(foreign.path(), root.path().join(".cartograph/data"))?;
    assert!(matches!(
        ProjectStore::open(root.path(), &identity(7), StorePolicy::default()),
        Err(RocksError::Directory)
    ));
    assert_eq!(std::fs::read_dir(foreign.path())?.count(), 0);
    Ok(())
}

#[test]
fn retention_resumes_without_resurrection_or_losing_unchanged_anchors() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut store = store(root.path())?;
    publish(
        &store,
        1,
        &[
            RecordChange::put(Collection::Graph, key(b"fixed")?, b"unchanged".to_vec()),
            RecordChange::put(
                Collection::Graph,
                key(b"gone")?,
                b"must_not_resurrect".to_vec(),
            ),
            RecordChange::put(Collection::Graph, key(b"moving")?, b"1".to_vec()),
        ],
    )?;
    let pinned = store.current_snapshot(None)?;
    publish(
        &store,
        2,
        &[
            RecordChange::delete(Collection::Graph, key(b"gone")?),
            RecordChange::put(Collection::Graph, key(b"moving")?, b"2".to_vec()),
        ],
    )?;
    for sequence in 3..=20 {
        publish(
            &store,
            sequence,
            &[RecordChange::put(
                Collection::Graph,
                key(b"moving")?,
                sequence.to_string().into_bytes(),
            )],
        )?;
    }
    let mut removed = 0;
    let mut complete = false;
    for _ in 0..200 {
        let report = store.prune(2, 2, 1, &mut budget()?)?;
        assert!(report.versions_visited <= 1);
        removed += report.generations_removed;
        assert_eq!(
            store
                .current_snapshot(None)?
                .get(Collection::Graph, &key(b"gone")?, &mut budget()?)?,
            None
        );
        assert_eq!(
            store.current_snapshot(None)?.get(
                Collection::Graph,
                &key(b"fixed")?,
                &mut budget()?
            )?,
            Some(b"unchanged".to_vec())
        );
        assert_eq!(
            pinned.get(Collection::Graph, &key(b"gone")?, &mut budget()?)?,
            Some(b"must_not_resurrect".to_vec())
        );
        if report.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(removed, 17);
    assert!(matches!(
        store.retained_snapshot(&generation(1)),
        Err(RocksError::GenerationUnavailable)
    ));
    assert_retained_anchor_values(&store)?;
    drop(pinned);
    drop(store);
    store = self::store(root.path())?;
    publish(
        &store,
        21,
        &[RecordChange::put(
            Collection::Graph,
            key(b"moving")?,
            b"21".to_vec(),
        )],
    )?;
    let interrupted = store.prune(2, 100, 1, &mut budget()?)?;
    assert!(!interrupted.complete);
    drop(store);
    store = self::store(root.path())?;
    for _ in 0..100 {
        if store.prune(2, 100, 2, &mut budget()?)?.complete {
            complete = true;
            break;
        }
        complete = false;
    }
    assert!(complete);
    drop(store);
    // Independent native inspection proves old versions were removed rather
    // than merely omitted from the public read path.
    assert_eq!(native_graph_version_count(root.path())?, 4);
    Ok(())
}

fn assert_retained_anchor_values(store: &ProjectStore) -> TestResult {
    for sequence in 18..=20 {
        let retained = store.retained_snapshot(&generation(sequence))?;
        assert_eq!(
            retained.get(Collection::Graph, &key(b"moving")?, &mut budget()?)?,
            Some(sequence.to_string().into_bytes())
        );
        assert_eq!(
            retained.get(Collection::Graph, &key(b"fixed")?, &mut budget()?)?,
            Some(b"unchanged".to_vec())
        );
    }
    Ok(())
}

fn native_graph_version_count(root: &Path) -> TestResult<usize> {
    let directory = root.join(".cartograph/data");
    let options = rocksdb::Options::default();
    let names = rocksdb::DB::list_cf(&options, &directory)?;
    let native = rocksdb::DB::open_cf_for_read_only(&options, &directory, names, false)?;
    let family = native.cf_handle("graph").ok_or("missing graph family")?;
    let physical = native
        .iterator_cf(&family, rocksdb::IteratorMode::Start)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(physical.len())
}

#[test]
fn checkpoint_reopens_as_a_complete_independent_project_copy() -> TestResult {
    let root = tempfile::tempdir()?;
    let copy = tempfile::tempdir()?;
    let store = store(root.path())?;
    publish(
        &store,
        1,
        &[
            RecordChange::put(Collection::Graph, key(b"symbol/a")?, b"before".to_vec()),
            RecordChange::put(Collection::Search, key(b"posting/a")?, b"before".to_vec()),
        ],
    )?;
    std::fs::create_dir(copy.path().join(".cartograph"))?;
    store.checkpoint(&copy.path().join(".cartograph/data"))?;
    publish(
        &store,
        2,
        &[RecordChange::put(
            Collection::Graph,
            key(b"symbol/a")?,
            b"after".to_vec(),
        )],
    )?;
    let copied = self::store(copy.path())?;
    assert_eq!(
        copied.current_snapshot(None)?.revision().generation_id,
        generation(1)
    );
    assert_eq!(
        copied.current_snapshot(None)?.get(
            Collection::Graph,
            &key(b"symbol/a")?,
            &mut budget()?
        )?,
        Some(b"before".to_vec())
    );
    assert_eq!(
        store
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"symbol/a")?, &mut budget()?)?,
        Some(b"after".to_vec())
    );
    Ok(())
}

#[test]
fn acknowledged_revision_survives_owned_process_kill() -> TestResult {
    use std::{
        process::{Command, Stdio},
        time::Instant,
    };
    let root = tempfile::tempdir()?;
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", "rocks_process_crash_worker", "--nocapture"])
        .env("CARTOGRAPH_ROCKS_CRASH_TEST_ROOT", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let ack = root.path().join("ack");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ack.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("crash worker exited before acknowledgement: {status}").into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let acknowledged = ack.exists();
    child.kill()?;
    child.wait()?;
    assert!(
        acknowledged,
        "owned crash worker did not acknowledge publication"
    );
    let store = store(root.path())?;
    let snapshot = store.current_snapshot(Some(&generation(2)))?;
    for (collection, name) in [
        (Collection::Graph, b"symbol/a".as_slice()),
        (Collection::Search, b"posting/a".as_slice()),
    ] {
        assert_eq!(
            snapshot.get(collection, &key(name)?, &mut budget()?)?,
            Some(b"acknowledged".to_vec())
        );
    }
    assert_eq!(
        snapshot.get(Collection::Graph, &key(b"staging")?, &mut budget()?)?,
        None
    );
    drop(snapshot);
    publish(&store, 4, &[])?;
    assert_eq!(
        store
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"staging")?, &mut budget()?)?,
        None
    );
    Ok(())
}

#[test]
fn rocks_process_crash_worker() -> TestResult {
    let Some(root) = std::env::var_os("CARTOGRAPH_ROCKS_CRASH_TEST_ROOT") else {
        return Ok(());
    };
    let root = Path::new(&root);
    let store = store(root)?;
    publish(
        &store,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"symbol/a")?,
            b"before".to_vec(),
        )],
    )?;
    publish(
        &store,
        2,
        &[
            RecordChange::put(
                Collection::Graph,
                key(b"symbol/a")?,
                b"acknowledged".to_vec(),
            ),
            RecordChange::put(
                Collection::Search,
                key(b"posting/a")?,
                b"acknowledged".to_vec(),
            ),
        ],
    )?;
    let writer = store.begin_revision(generation(3), identity(1), Duration::from_secs(60))?;
    store.stage(
        &writer,
        &[RecordChange::put(
            Collection::Graph,
            key(b"staging")?,
            b"incomplete".to_vec(),
        )],
    )?;
    std::fs::write(root.join("ack"), b"published")?;
    // The parent kills this owned process after observing the acknowledgement.
    std::thread::sleep(Duration::from_secs(30));
    Err("crash worker was not killed by its parent".into())
}
