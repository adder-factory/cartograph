use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};

use super::*;
use crate::rocks::StorePolicy;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn failed_cleanup_preserves_cancellation_and_cannot_terminalize_a_newer_writer() -> TestResult {
    let root = tempfile::tempdir()?;
    let digest = ContentDigest::from_bytes([1; 32]);
    let store = ProjectStore::open(root.path(), &digest, StorePolicy::default())?;
    let old = store.begin_revision(
        GenerationId::from_uuid_v8([1; 16]),
        digest.clone(),
        super::super::Duration::from_secs(30),
    )?;
    store.fail(&old)?;
    let replacement = store.begin_revision(
        GenerationId::from_uuid_v8([2; 16]),
        digest.clone(),
        super::super::Duration::from_secs(30),
    )?;
    // Deterministic interleaving: another owner acquired before old cleanup.
    let request = StorageRequest::new(super::super::Duration::from_secs(30))?;
    request.cancel();
    let primary = request.check().err().ok_or("missing cancellation")?;
    let failure = failed_publication(&store, &old, primary);
    assert_eq!(failure.primary, RocksError::Cancelled);
    assert_eq!(failure.cleanup_failure, Some(RocksError::FenceLost));
    assert_eq!(failure.to_string(), RocksError::Cancelled.to_string());
    store.seal(&replacement, digest, GenerationDigestVersion::V23)?;
    store.publish(&replacement)?;
    assert_eq!(store.current_snapshot(None)?.revision().sequence, 2);
    Ok(())
}
