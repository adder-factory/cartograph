//! Joined, bounded source hashing. Every scan still discovers and reads actual bytes.

use super::{ProjectError, oversized_source_digest};
use cartograph_domain::{ContentDigest, NormalizedPath, SourceLanguage};
use cartograph_extract::{
    DiscoveredSource, SourceLimits, SourceReadError, SourceReadOptions, SourceRoot,
};
use std::{
    collections::BTreeSet,
    sync::{Mutex, mpsc},
};

pub(super) struct HashScan<'a> {
    pub(super) root: &'a SourceRoot,
    pub(super) files: &'a [DiscoveredSource],
    pub(super) limits: SourceLimits,
    pub(super) maximum_bytes: usize,
    pub(super) retain_native: bool,
}

pub(super) struct Observation {
    pub(super) path: NormalizedPath,
    pub(super) hash: ContentDigest,
    pub(super) bytes: u64,
    pub(super) admitted: bool,
    pub(super) v1: bool,
    pub(super) native: Option<cartograph_indexer::NativeObservedFile>,
}

/// Discard incomplete observations and rediscover after a concurrent edit.
/// The caller retains the original deadline/cancellation across all attempts.
pub(super) fn retry_complete_scan<T>(
    mut scan: impl FnMut() -> Result<T, ProjectError>,
) -> Result<T, ProjectError> {
    for attempt in 0..3 {
        match scan() {
            Err(ProjectError::SourceScanFailed) if attempt < 2 => {}
            result => return result,
        }
    }
    Err(ProjectError::SourceScanFailed)
}

pub(super) fn read_sources<Cancel>(
    mut scan: HashScan<'_>,
    cancelled: &mut Cancel,
    capture_paths: &BTreeSet<NormalizedPath>,
    captures: &mut super::SourceCaptures,
    mut observe: impl FnMut(Observation) -> Result<(), ProjectError>,
) -> Result<(), ProjectError>
where
    Cancel: FnMut() -> bool + Send,
{
    let parallel = capture_paths.is_empty()
        && scan.files.len() >= 32
        && scan
            .files
            .iter()
            .all(|file| file.byte_size() <= 8 * 1_048_576);
    if parallel {
        scan.limits = super::source_limits_with_max(scan.maximum_bytes.min(8 * 1_048_576))?;
        return read_hashes(&scan, cancelled, observe);
    }
    for file in scan.files {
        if let Some(observation) = read_one(&scan, file, &mut *cancelled, |snapshot| {
            if capture_paths.contains(file.path()) {
                captures.insert(file.path(), snapshot)?;
            }
            Ok(())
        })? {
            observe(observation)?;
        }
    }
    Ok(())
}

fn read_hashes<Cancel>(
    scan: &HashScan<'_>,
    cancelled: &mut Cancel,
    observe: impl FnMut(Observation) -> Result<(), ProjectError>,
) -> Result<(), ProjectError>
where
    Cancel: FnMut() -> bool + Send,
{
    // Four source buffers retain at most 32 MiB. Each producer can retain only
    // eight compact observations; strided reduction preserves manifest order.
    let cancellation = Mutex::new(cancelled);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut receivers = Vec::new();
        for worker in 0..4 {
            let (sender, receiver) = mpsc::sync_channel(8);
            receivers.push(receiver);
            let cancellation = &cancellation;
            if let Ok(handle) = std::thread::Builder::new()
                .name("cartograph-source-hash".into())
                .spawn_scoped(scope, move || {
                    hash_files(scan, worker, cancellation, &sender);
                })
            {
                handles.push(handle);
            } else {
                drop(receivers);
                join_hash_workers(handles)?;
                return Err(ProjectError::SourceScanFailed);
            }
        }
        let result = observe_hashes(&receivers, scan.files.len(), observe);
        drop(receivers);
        join_hash_workers(handles)?;
        result
    })
}

type HashResult = Result<Option<Observation>, ProjectError>;

fn hash_files<Cancel>(
    scan: &HashScan<'_>,
    worker: usize,
    cancellation: &Mutex<&mut Cancel>,
    sender: &mpsc::SyncSender<HashResult>,
) where
    Cancel: FnMut() -> bool + Send,
{
    for file in scan.files.iter().skip(worker).step_by(4) {
        let stopped = || cancellation.lock().map_or(true, |mut check| check());
        let value = read_one(scan, file, stopped, |_| Ok(()));
        let failed = value.is_err();
        if sender.send(value).is_err() || failed {
            break;
        }
    }
}

fn observe_hashes(
    receivers: &[mpsc::Receiver<HashResult>],
    files: usize,
    mut observe: impl FnMut(Observation) -> Result<(), ProjectError>,
) -> Result<(), ProjectError> {
    for index in 0..files {
        if let Some(observation) = receivers[index % 4]
            .recv()
            .map_err(|_| ProjectError::SourceScanFailed)??
        {
            observe(observation)?;
        }
    }
    Ok(())
}

fn join_hash_workers(
    handles: Vec<std::thread::ScopedJoinHandle<'_, ()>>,
) -> Result<(), ProjectError> {
    let mut joined = true;
    for handle in handles {
        joined &= handle.join().is_ok();
    }
    if joined {
        Ok(())
    } else {
        Err(ProjectError::SourceScanFailed)
    }
}

fn read_one(
    scan: &HashScan<'_>,
    file: &DiscoveredSource,
    mut cancelled: impl FnMut() -> bool,
    mut capture: impl FnMut(&cartograph_extract::SourceSnapshot) -> Result<(), ProjectError>,
) -> Result<Option<Observation>, ProjectError> {
    if cancelled() {
        return Err(ProjectError::SourceScanFailed);
    }
    if file.byte_size() > scan.maximum_bytes as u64 {
        return Ok(Some(Observation {
            path: file.path().clone(),
            hash: oversized_source_digest(file),
            bytes: 0,
            admitted: false,
            v1: SourceLanguage::is_v1_candidate_path(file.path().as_str()),
            native: None,
        }));
    }
    let snapshot = match scan.root.read_with_cancellation(
        file.path(),
        SourceReadOptions::new(scan.limits, &mut cancelled),
    ) {
        Ok(snapshot) => snapshot,
        Err(SourceReadError::UnsupportedLanguage) => return Ok(None),
        Err(_) => return Err(ProjectError::SourceScanFailed),
    };
    capture(&snapshot)?;
    Ok(Some(Observation {
        path: file.path().clone(),
        hash: snapshot.content_hash().clone(),
        bytes: snapshot.byte_size(),
        admitted: true,
        v1: SourceLanguage::for_v1_normalized_path_with_source(
            file.path().as_str(),
            snapshot.source(),
        )
        .is_some(),
        native: scan
            .retain_native
            .then(|| cartograph_indexer::NativeObservedFile::from_snapshot(&snapshot)),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[tokio::test]
    async fn captured_source_queue_retains_the_inherited_native_deadline()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("a.rs"), "pub fn value() {}\n")?;
        let policy = project_source_policy(root.path())?;
        let permits = Arc::new(Semaphore::new(1));
        let held = permits.clone().acquire_owned().await?;
        let counters = Arc::new(SourceScanCounters::default());
        let request = cartograph_db::rocks::StorageRequest::new(Duration::from_millis(25))?;
        let scan = CartographDatabase::with_request(
            request,
            scan_source_path(SourceScanRequest {
                root: root.path().to_owned(),
                permits,
                counters: counters.clone(),
                capture_paths: BTreeSet::from([NormalizedPath::parse("a.rs")?]),
                retain_scip_overlay: false,
                max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
                discovery_policy: policy.discovery,
                index_policy: policy.index,
                cancellation: ProjectCancellation::new(),
            }),
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), scan).await?,
            Err(ProjectError::RequestCancelled)
        ));
        assert_eq!(counters.observations.load(Ordering::Relaxed), 0);
        drop(held);
        Ok(())
    }

    #[test]
    fn complete_scan_restarts_after_a_discovered_file_disappears()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.path().join(name), "pub fn value() {}\n")?;
        }
        let source_root = SourceRoot::open(root.path())?;
        let mut attempts = 0;
        let paths = super::retry_complete_scan(|| {
            attempts += 1;
            let files = source_root
                .discover(discovery_limits()?)
                .map_err(|_| ProjectError::SourceScanFailed)?;
            if attempts == 1 {
                std::fs::remove_file(root.path().join("c.rs"))
                    .map_err(|_| ProjectError::SourceScanFailed)?;
            }
            let mut paths = Vec::new();
            super::read_sources(
                super::HashScan {
                    root: &source_root,
                    files: &files,
                    limits: source_limits_with_max(DEFAULT_MAX_SOURCE_BYTES)?,
                    maximum_bytes: DEFAULT_MAX_SOURCE_BYTES,
                    retain_native: false,
                },
                &mut || false,
                &BTreeSet::new(),
                &mut SourceCaptures::default(),
                |observation| {
                    paths.push(observation.path);
                    Ok(())
                },
            )?;
            Ok(paths)
        })?;
        assert_eq!(attempts, 2);
        assert_eq!(
            paths.iter().map(NormalizedPath::as_str).collect::<Vec<_>>(),
            ["a.rs", "b.rs"]
        );
        attempts = 0;
        assert!(matches!(
            super::retry_complete_scan::<()>(|| {
                attempts += 1;
                Err(ProjectError::SourceScanFailed)
            }),
            Err(ProjectError::SourceScanFailed)
        ));
        assert_eq!(attempts, 3);
        attempts = 0;
        assert!(matches!(
            super::retry_complete_scan::<()>(|| {
                attempts += 1;
                Err(ProjectError::RequestCancelled)
            }),
            Err(ProjectError::RequestCancelled)
        ));
        assert_eq!(attempts, 1);
        Ok(())
    }

    #[test]
    fn retried_capture_discards_bytes_from_the_incomplete_observation()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let captured_path = NormalizedPath::parse("a.rs")?;
        std::fs::write(root.path().join("a.rs"), "pub fn old_value() {}\n")?;
        std::fs::write(root.path().join("b.rs"), "pub fn removed() {}\n")?;
        let source_root = SourceRoot::open(root.path())?;
        let mut attempts = 0;
        let captures = super::retry_complete_scan(|| {
            attempts += 1;
            let files = source_root
                .discover(discovery_limits()?)
                .map_err(|_| ProjectError::SourceScanFailed)?;
            let mut captures = SourceCaptures::default();
            super::read_sources(
                super::HashScan {
                    root: &source_root,
                    files: &files,
                    limits: source_limits_with_max(DEFAULT_MAX_SOURCE_BYTES)?,
                    maximum_bytes: DEFAULT_MAX_SOURCE_BYTES,
                    retain_native: false,
                },
                &mut || false,
                &BTreeSet::from([captured_path.clone()]),
                &mut captures,
                |observation| {
                    if attempts == 1 && observation.path == captured_path {
                        std::fs::write(root.path().join("a.rs"), "pub fn new_value() {}\n")
                            .map_err(|_| ProjectError::SourceScanFailed)?;
                        std::fs::remove_file(root.path().join("b.rs"))
                            .map_err(|_| ProjectError::SourceScanFailed)?;
                    }
                    Ok(())
                },
            )?;
            Ok(captures)
        })?;
        assert_eq!(attempts, 2);
        assert_eq!(captures.entries.len(), 1);
        assert_eq!(
            captures.entries[&captured_path].source.as_ref(),
            "pub fn new_value() {}\n"
        );
        Ok(())
    }

    #[test]
    fn parallel_source_hashing_matches_serial_bytes_and_detects_cancellation()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        for index in 0..64 {
            std::fs::write(
                root.path().join(format!("file_{index:03}.rs")),
                format!("pub fn value_{index}() -> u32 {{ {index} }}\n"),
            )?;
        }
        let empty = BTreeSet::new();
        let unmatched = BTreeSet::from([NormalizedPath::parse("not-discovered.rs")?]);
        let scan = |captures: &BTreeSet<NormalizedPath>| -> Result<SourceRevision, ProjectError> {
            let policy = project_source_policy(root.path())?;
            source_revision_with_options(
                SourceRevisionRequest {
                    root: root.path(),
                    capture_paths: captures,
                    retain_scip_overlay: false,
                    max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
                    discovery_policy: policy.discovery,
                    index_policy: policy.index,
                },
                || false,
            )
        };
        let parallel = scan(&empty)?;
        let serial = scan(&unmatched)?;
        assert_eq!(parallel.digest, serial.digest);
        assert_eq!(parallel.admitted_manifest, serial.admitted_manifest);
        assert_eq!(parallel.v1_source_manifest, serial.v1_source_manifest);
        assert_eq!(parallel.source_bytes, serial.source_bytes);
        let policy = project_source_policy(root.path())?;
        let mut checkpoints = 0_u64;
        let cancelled = source_revision_with_options(
            SourceRevisionRequest {
                root: root.path(),
                capture_paths: &empty,
                retain_scip_overlay: false,
                max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
                discovery_policy: policy.discovery,
                index_policy: policy.index,
            },
            || {
                checkpoints += 1;
                checkpoints > 80
            },
        );
        assert!(cancelled.is_err());
        Ok(())
    }
}
