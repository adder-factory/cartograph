use cartograph_domain::{ContentDigest, GenerationDigestVersion, GenerationId};

use super::*;
use crate::rocks::StorePolicy;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn grouped_discovery_polls_cancellation_before_a_large_group_finishes() -> TestResult {
    use cartograph_domain::{FileId, FileParseStatus};
    let file_id = FileId::from_uuid_v8([1; 16]);
    let facts = crate::GenerationFacts {
        files: vec![crate::FileInput {
            file_id: file_id.clone(),
            normalized_path: "src/lib.rs".into(),
            language: "rust".into(),
            content_hash: ContentDigest::from_bytes([3; 32]),
            byte_size: 10_000,
            parse_status: FileParseStatus::Parsed,
        }],
        references: (0..1_000)
            .map(|start_byte| crate::ReferenceInput {
                file_id: file_id.clone(),
                owner_symbol_id: None,
                target_symbol_id: None,
                reference_name: "target".into(),
                reference_kind: "calls".into(),
                start_byte,
                end_byte: start_byte + 1,
                confidence: 0.0,
                resolution_provenance: "unresolved".into(),
                site_count: 1,
                span_precision: crate::ReferenceSpanPrecision::Exact,
            })
            .collect(),
        ..crate::GenerationFacts::default()
    };
    let (facts, _) = crate::validate_generation_facts(
        facts,
        crate::GenerationValidationLimits::new(8 * 1_048_576, 32 * 1_048_576)?,
        || false,
    )?;
    let mut polls = 0;
    let mut groups = table_groups(&facts, REFERENCE, facts.references().len(), || {
        polls += 1;
        if polls == 17 {
            Err(RocksError::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(groups.next(), Some(Err(RocksError::Cancelled))));
    assert!(groups.next().is_none());
    drop(groups);
    assert_eq!(polls, 17);
    Ok(())
}

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
