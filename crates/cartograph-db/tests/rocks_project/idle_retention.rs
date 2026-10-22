//! Completed maintenance must stay cheap without skipping newly admitted work.

use super::{
    Collection, Duration, RecordChange, RocksError, TestResult, budget, generation, identity, key,
    publish, store,
};

#[test]
fn completed_retention_skips_unchanged_versions_across_reopen() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    publish(
        &native,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"stable")?,
            b"value".to_vec(),
        )],
    )?;
    let first = native.prune(2, 4, 100, &mut budget()?)?;
    assert!(first.complete);
    assert_eq!(first.versions_visited, 1);
    for owner in [&native, &native.clone()] {
        let repeated = owner.prune(2, 1, 1, &mut budget()?)?;
        assert!(repeated.complete);
        assert_eq!(repeated.versions_visited, 0);
        assert_eq!(repeated.versions_removed, 0);
        assert_eq!(repeated.generations_removed, 0);
        assert_eq!(repeated.oldest_retained_sequence, 1);
    }
    drop(native);
    let reopened = store(root.path())?;
    assert_eq!(
        reopened.prune(2, 4, 100, &mut budget()?)?.versions_visited,
        0
    );
    assert_eq!(
        reopened.current_snapshot(None)?.get(
            Collection::Graph,
            &key(b"stable")?,
            &mut budget()?
        )?,
        Some(b"value".to_vec())
    );
    Ok(())
}

#[test]
fn completed_retention_rejects_live_writers_and_drains_failed_and_published_changes() -> TestResult
{
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    publish(
        &native,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"stable")?,
            b"original".to_vec(),
        )],
    )?;
    let pinned = native.current_snapshot(None)?;
    assert!(native.prune(2, 4, 100, &mut budget()?)?.complete);
    let failed = native.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    native.stage(
        &failed,
        &[
            RecordChange::put(Collection::Graph, key(b"stable")?, b"failed".to_vec()),
            RecordChange::put(Collection::Graph, key(b"ghost")?, b"failed".to_vec()),
        ],
    )?;
    assert!(matches!(
        native.prune(2, 4, 100, &mut budget()?),
        Err(RocksError::WriterBusy)
    ));
    native.fail(&failed)?;
    let mut removed = 0;
    let mut complete = false;
    for _ in 0..10 {
        let progress = native.prune(2, 4, 1, &mut budget()?)?;
        removed += progress.versions_removed;
        if progress.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(removed, 2);
    assert_eq!(native.prune(2, 4, 100, &mut budget()?)?.versions_visited, 0);
    publish(
        &native,
        3,
        &[RecordChange::delete(Collection::Graph, key(b"stable")?)],
    )?;
    let changed = native.prune(2, 4, 100, &mut budget()?)?;
    assert!(changed.complete);
    assert!(changed.versions_visited > 0);
    let current = native.current_snapshot(None)?;
    for name in [b"stable".as_slice(), b"ghost"] {
        assert_eq!(
            current.get(Collection::Graph, &key(name)?, &mut budget()?)?,
            None
        );
    }
    assert_eq!(
        pinned.get(Collection::Graph, &key(b"stable")?, &mut budget()?)?,
        Some(b"original".to_vec())
    );
    Ok(())
}

#[test]
fn a_policy_change_invalidates_completion_even_when_cleanup_is_partial() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    for sequence in 1..=3 {
        publish(
            &native,
            sequence,
            &[RecordChange::put(
                Collection::Graph,
                key(b"moving")?,
                sequence.to_be_bytes().to_vec(),
            )],
        )?;
    }
    let pinned = native.retained_snapshot(&generation(1))?;
    assert!(native.prune(2, 4, 100, &mut budget()?)?.complete);
    let partial = native.prune(0, 1, 1, &mut budget()?)?;
    assert!(!partial.complete);
    let resumed = native.prune(2, 4, 100, &mut budget()?)?;
    assert!(resumed.complete);
    assert!(resumed.versions_visited > 0);
    assert_eq!(native.prune(2, 4, 100, &mut budget()?)?.versions_visited, 0);
    assert_eq!(
        pinned.get(Collection::Graph, &key(b"moving")?, &mut budget()?)?,
        Some(1_u64.to_be_bytes().to_vec())
    );
    assert_eq!(
        native
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"moving")?, &mut budget()?)?,
        Some(3_u64.to_be_bytes().to_vec())
    );
    Ok(())
}

#[test]
fn a_new_reservation_restarts_partial_cleanup_before_certifying_completion() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    publish(
        &native,
        1,
        &[
            RecordChange::put(Collection::Graph, key(b"a")?, b"original".to_vec()),
            RecordChange::put(Collection::Graph, key(b"z")?, b"stable".to_vec()),
        ],
    )?;
    let partial = native.prune(2, 4, 1, &mut budget()?)?;
    assert!(!partial.complete);
    assert_eq!(partial.versions_visited, 1);
    assert_eq!(partial.oldest_retained_sequence, 1);
    let failed = native.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    native.stage(
        &failed,
        &[RecordChange::put(
            Collection::Graph,
            key(b"a")?,
            b"failed".to_vec(),
        )],
    )?;
    native.fail(&failed)?;
    // The newer version of a sorts before the old cursor. Finish the old pass
    // without certifying completion, then wrap to collect that failed version.
    let resumed = native.prune(2, 4, 100, &mut budget()?)?;
    assert!(!resumed.complete);
    assert_eq!(resumed.versions_visited, 1);
    assert_eq!(resumed.versions_removed, 0);
    let resumed = native.prune(2, 4, 100, &mut budget()?)?;
    assert!(resumed.complete);
    assert_eq!(resumed.oldest_retained_sequence, 1);
    assert_eq!(resumed.versions_removed, 1);
    assert_eq!(resumed.versions_visited, 3);
    assert_eq!(native.prune(2, 4, 100, &mut budget()?)?.versions_visited, 0);
    assert_eq!(
        native
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        Some(b"original".to_vec())
    );
    Ok(())
}

#[test]
fn new_reservations_do_not_starve_cleanup_after_a_retained_prefix() -> TestResult {
    let root = tempfile::tempdir()?;
    let native = store(root.path())?;
    publish(
        &native,
        1,
        &[RecordChange::put(
            Collection::Graph,
            key(b"a")?,
            b"retained".to_vec(),
        )],
    )?;
    let failed = native.begin_revision(generation(2), identity(1), Duration::from_secs(30))?;
    native.stage(
        &failed,
        &[RecordChange::put(
            Collection::Graph,
            key(b"z")?,
            b"failed".to_vec(),
        )],
    )?;
    native.fail(&failed)?;
    let partial = native.prune(2, 4, 1, &mut budget()?)?;
    assert!(!partial.complete);
    assert_eq!(partial.versions_visited, 1);
    let mut removed = 0;
    for sequence in 3..=10 {
        let empty =
            native.begin_revision(generation(sequence), identity(1), Duration::from_secs(30))?;
        native.fail(&empty)?;
        let progress = native.prune(2, 4, 1, &mut budget()?)?;
        assert!(!progress.complete);
        removed += progress.versions_removed;
    }
    assert_eq!(removed, 1);
    assert_eq!(
        native
            .current_snapshot(None)?
            .get(Collection::Graph, &key(b"a")?, &mut budget()?)?,
        Some(b"retained".to_vec())
    );
    assert!(native.prune(2, 4, 100, &mut budget()?)?.complete);
    assert_eq!(native.prune(2, 4, 100, &mut budget()?)?.versions_visited, 0);
    Ok(())
}
