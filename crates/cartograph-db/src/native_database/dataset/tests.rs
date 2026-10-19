//! Real native transactions prove that staged pages never become partial visible heads.

use super::*;
use crate::rocks::{AsyncProjectStore, StorageRequest, StorageRuntime, StorePolicy, WorkerPolicy};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_native_state_deadline_survives_thirty_seconds_of_work() -> TestResult {
    let fixture = Fixture::new().await?;
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let database = fixture.database.clone();
    let work = tokio::spawn(async move {
        database
            .state_with_timeout(
                None,
                1024,
                1,
                Duration::from_secs(60),
                move |transaction, budget| {
                    transaction.put(StateNamespace::Metadata, b"long-request", vec![42])?;
                    let _entered = entered.send(());
                    released
                        .recv_timeout(Duration::from_secs(40))
                        .map_err(|_| crate::rocks::RocksError::Operation {
                            operation: "fixture-gate",
                        })?;
                    budget.checkpoint()
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), started).await??;
    // This crosses the former independent thirty-second read-budget cap.
    tokio::time::sleep(Duration::from_secs(31)).await;
    let returned_early = work.is_finished();
    release.send(())?;
    let result = tokio::time::timeout(Duration::from_secs(5), work).await??;
    assert!(
        !returned_early,
        "native request abandoned its admitted work"
    );
    result?;
    assert_eq!(
        fixture
            .database
            .state(None, 0, 0, |transaction, budget| {
                transaction.get(StateNamespace::Metadata, b"long-request", budget)
            })
            .await?,
        Some(vec![42])
    );
    fixture
        .runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dataset_preparation_deadline_drains_the_native_head_job_and_preserves_commit_outcome()
-> TestResult {
    for after_commit in [false, true] {
        let fixture = Fixture::new().await?;
        fixture.write(vec!["old-complete".into()]).await?;
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        *fixture
            .database
            .store
            .state_gate
            .lock()
            .map_err(|_| "gate poisoned")? = Some(crate::rocks::StateTestGate {
            write_rows: 3,
            after_commit,
            entered,
            release: released,
        });
        let database = fixture.database.clone();
        let work = tokio::spawn(async move {
            NativeDatabase::with_preparation_deadline(
                Duration::from_millis(500),
                database.prepare_dataset(
                    StateNamespace::Derived,
                    "atomic".to_owned(),
                    None,
                    serde_json::Value::Null,
                    vec!["new-complete".to_owned()].into_iter(),
                    CommitGuard::default(),
                ),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), started).await??;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let returned_early = work.is_finished();
        release.send(())?;
        let result = tokio::time::timeout(Duration::from_secs(3), work).await??;
        assert!(
            !returned_early,
            "preparation dropped an admitted native head job"
        );
        assert_eq!(
            result.is_ok(),
            after_commit,
            "actual commit outcome was lost: {result:?}"
        );
        assert_eq!(
            fixture.values().await?,
            [if after_commit {
                "new-complete"
            } else {
                "old-complete"
            }]
        );
        fixture
            .runtime
            .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
            .await?;
    }
    Ok(())
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: StorageRuntime,
    database: NativeDatabase,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_similarity_rebuild_retains_its_actual_native_read_until_release() -> TestResult {
    let fixture = Fixture::new().await?;
    fixture.write(vec!["old-complete".into()]).await?;
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *fixture
        .database
        .store
        .state_gate
        .lock()
        .map_err(|_| "gate poisoned")? = Some(crate::rocks::StateTestGate {
        write_rows: 0,
        after_commit: false,
        entered,
        release: released,
    });
    let request = StorageRequest::new(Duration::from_secs(30))?;
    let control = request.clone();
    let database = fixture.database.clone();
    let policy = crate::SimilarityMaterializationPolicy::new(10, 0.0, Duration::from_secs(30))?;
    let work = tokio::spawn(async move {
        NativeDatabase::with_request(
            control,
            database.rebuild_current_similarity_edges(database.project_id(), policy),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), started).await??;
    request.cancel();
    tokio::time::sleep(Duration::from_millis(25)).await;
    let returned_early = work.is_finished();
    release.send(())?;
    assert!(
        tokio::time::timeout(Duration::from_secs(3), work)
            .await??
            .is_err()
    );
    assert!(
        !returned_early,
        "similarity rebuild dropped its admitted native reader"
    );
    assert_eq!(fixture.values().await?, ["old-complete"]);
    fixture
        .runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}
impl Fixture {
    async fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let runtime = StorageRuntime::new(WorkerPolicy::default())?;
        let identity = ContentDigest::from_bytes([1; 32]);
        let request = StorageRequest::new(Duration::from_secs(30))?;
        let store = AsyncProjectStore::open(
            &runtime,
            root.path().to_owned(),
            identity.clone(),
            StorePolicy::default(),
            &request,
        )
        .await?;
        Ok(Self {
            _root: root,
            runtime,
            database: NativeDatabase::new(store, &identity),
        })
    }
    async fn write(&self, values: Vec<String>) -> TestResult {
        self.database
            .replace_dataset(
                StateNamespace::Derived,
                "atomic",
                None,
                &(),
                values.into_iter(),
            )
            .await?;
        Ok(())
    }
    async fn values(&self) -> TestResult<Vec<String>> {
        Ok(self
            .database
            .state(None, 0, 0, |transaction, budget| {
                let manifest = manifest(transaction, StateNamespace::Derived, "atomic", budget)?
                    .ok_or(corrupt())?;
                let mut output = Vec::new();
                visit(
                    transaction,
                    StateNamespace::Derived,
                    "atomic",
                    &manifest,
                    budget,
                    |value: String, _| {
                        output.push(value);
                        Ok(())
                    },
                )?;
                Ok(output)
            })
            .await?)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_later_part_keeps_the_old_complete_head_and_collects_only_obsolete_pages()
-> TestResult {
    let fixture = Fixture::new().await?;
    fixture.write(vec!["old-complete".into()]).await?;
    let mut rows = vec!["staged-new".to_owned(); 256];
    rows.push("x".repeat(5 * 1_048_576));
    assert!(fixture.write(rows).await.is_err());
    assert_eq!(fixture.values().await?, ["old-complete"]);
    age_preparations(&fixture.database).await?;
    let pruned = fixture.database.prune_auxiliary_snapshots().await?;
    assert_eq!(pruned.snapshots_removed, 1);
    assert_eq!(pruned.pages_removed, 1);
    assert_eq!(fixture.values().await?, ["old-complete"]);
    assert_eq!(
        fixture
            .database
            .prune_auxiliary_snapshots()
            .await?
            .snapshots_removed,
        0
    );
    fixture
        .runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}

async fn age_preparations(database: &NativeDatabase) -> TestResult {
    database
        .state(None, 1_048_576, 10, |transaction, budget| {
            let mut updates = Vec::new();
            super::super::visit_state::<Prepared>(
                transaction,
                StateNamespace::Derived,
                b"dataset/s/",
                budget,
                |key, mut prepared, _| {
                    prepared.manifest.created = "2000-01-01T00:00:00Z".into();
                    updates.push((key, encode(&prepared)?));
                    Ok(())
                },
            )?;
            for (key, bytes) in updates {
                transaction.put(StateNamespace::Derived, &key, bytes)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_auxiliary_inputs_reject_a_precomputed_dataset_without_replacing_the_head()
-> TestResult {
    let fixture = Fixture::new().await?;
    fixture.write(vec!["old-complete".into()]).await?;
    let guard = fixture
        .database
        .state(None, 0, 0, |transaction, budget| {
            CommitGuard::capture(transaction, &[b"coverage_epoch"], budget)
        })
        .await?;
    fixture
        .database
        .state(None, 4096, 1, |transaction, budget| {
            crate::artifacts::native::bump_epoch(transaction, budget, b"coverage_epoch")
        })
        .await?;
    assert_eq!(
        fixture
            .database
            .replace_dataset_guarded(
                StateNamespace::Derived,
                "atomic",
                None,
                &(),
                vec!["stale-computation".to_owned()].into_iter(),
                guard
            )
            .await,
        Err(StorageError::CurrentGenerationChanged)
    );
    assert_eq!(fixture.values().await?, ["old-complete"]);
    let guard = fixture
        .database
        .state(None, 0, 0, |transaction, budget| {
            CommitGuard::capture(transaction, &[b"coverage_epoch"], budget)
        })
        .await?;
    fixture
        .database
        .replace_dataset_guarded(
            StateNamespace::Derived,
            "atomic",
            None,
            &(),
            vec!["new-complete".to_owned()].into_iter(),
            guard,
        )
        .await?;
    assert_eq!(fixture.values().await?, ["new-complete"]);
    fixture
        .runtime
        .shutdown(&StorageRequest::new(Duration::from_secs(30))?)
        .await?;
    Ok(())
}
