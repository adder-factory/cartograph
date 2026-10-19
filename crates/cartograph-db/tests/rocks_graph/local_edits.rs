use super::*;
use cartograph_db::{ReferenceInput, ReferenceSpanPrecision};

fn sparse(
    facts: CanonicalGenerationFacts,
    replacements: GenerationFacts,
) -> TestResult<GraphPublicationInput> {
    Ok(GraphPublicationInput::replace_files(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(8),
            base: GraphBase::Current(generation(1)),
        },
        facts,
        replacements,
        GraphPolicy::default(),
        &request()?,
    )?)
}

fn reference() -> ReferenceInput {
    ReferenceInput {
        file_id: FileId::from_uuid_v8([1; 16]),
        owner_symbol_id: Some(symbol(1)),
        target_symbol_id: Some(symbol(2)),
        reference_name: "function_2".into(),
        reference_kind: "calls".into(),
        start_byte: 0,
        end_byte: 1,
        confidence: 1.0,
        resolution_provenance: "native".into(),
        site_count: 1,
        span_precision: ReferenceSpanPrecision::Exact,
    }
}

#[test]
fn sparse_publication_matches_cold_digest_and_keeps_pinned_topology_and_rows() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut old = raw_facts(3, vec![edge(1, 2, "native")]);
    old.references.push(reference());
    store.publish_graph(input(1, canonical(old.clone())?)?, &request()?)?;
    let pinned = store.current_snapshot(None)?;
    let mut replacement = old.clone();
    replacement.edges.clear();
    replacement.files[0].content_hash = digest(4);
    replacement.symbols[0].structural_digest = digest(5);
    replacement.references.clear();
    let mut cold = replacement.clone();
    cold.edges = old.edges.clone();
    let expected = canonical(cold)?.digest().clone();
    let report = store.publish_graph(sparse(canonical(old)?, replacement)?, &request()?)?;
    assert_eq!(report.revision.content_digest, Some(expected));
    assert_eq!(report.report.adjacency_blocks, 0);
    assert_eq!(report.report.written_records, 3); // file, symbol, manifest
    assert_eq!(report.report.deleted_records, 1);
    let current = store.current_snapshot(None)?;
    let current = current.graph(&mut budget()?)?;
    let old = pinned.graph(&mut budget()?)?;
    assert_eq!(
        old.symbol(&symbol(1), &mut budget()?)?
            .ok_or("old symbol missing")?
            .structural_digest,
        digest(2)
    );
    assert_eq!(
        current
            .symbol(&symbol(1), &mut budget()?)?
            .ok_or("new symbol missing")?
            .structural_digest,
        digest(5)
    );
    assert_eq!(old.reference_records(None, 10, &mut budget()?)?.0.len(), 1);
    assert_eq!(current.reference_records(None, 10, &mut budget()?)?.0, []);
    for graph in [&old, &current] {
        assert_eq!(
            graph
                .neighbors(
                    &symbol(1),
                    GraphDirection::Outgoing,
                    None,
                    10,
                    &mut budget()?
                )?
                .links
                .len(),
            1
        );
        assert_eq!(
            graph
                .neighbors(
                    &symbol(2),
                    GraphDirection::Incoming,
                    None,
                    10,
                    &mut budget()?
                )?
                .links
                .len(),
            1
        );
    }
    Ok(())
}

#[test]
fn sparse_publication_rejects_a_canonical_base_with_the_wrong_digest() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let original = store.publish_graph(
        input(1, facts(3, vec![edge(1, 2, "native")])?)?,
        &request()?,
    )?;
    let wrong_base = facts(3, Vec::new())?;
    let error = store
        .publish_graph(sparse(wrong_base, raw_facts(3, Vec::new()))?, &request()?)
        .err()
        .ok_or("wrong base was accepted")?;
    assert_eq!(error.primary, RocksError::GenerationChanged);
    assert_eq!(store.current_snapshot(None)?.revision(), &original.revision);
    Ok(())
}

#[test]
fn sparse_replacements_reject_identity_and_relation_changes_before_publication() -> TestResult {
    for case in 0..8 {
        let mut replacement = raw_facts(3, Vec::new());
        match case {
            0 => replacement.files[0].normalized_path = "other.rs".into(),
            1 => replacement.files[0].language = "python".into(),
            2 => {
                replacement.symbols.pop();
            }
            3 => replacement.symbols[0].end_byte = 2,
            4 => {
                let mut row = reference();
                row.target_symbol_id = Some(symbol(99));
                replacement.references.push(row);
            }
            5 => {
                let mut row = reference();
                row.end_byte = 2;
                replacement.references.push(row);
            }
            6 => replacement.symbols[0].qualified_name = "renamed".into(),
            _ => replacement.edges.push(edge(1, 2, "native")),
        }
        assert!(
            sparse(facts(3, Vec::new())?, replacement).is_err(),
            "invalid case {case} accepted"
        );
    }
    Ok(())
}

#[test]
fn sparse_replacements_observe_cancellation_and_input_bounds() -> TestResult {
    let request = request()?;
    request.cancel();
    let identity = GraphRevision {
        generation_id: generation(2),
        source_revision: digest(8),
        base: GraphBase::Current(generation(1)),
    };
    let cancelled = GraphPublicationInput::replace_files(
        identity.clone(),
        facts(3, Vec::new())?,
        raw_facts(3, Vec::new()),
        GraphPolicy::default(),
        &request,
    );
    assert!(matches!(cancelled, Err(RocksError::Cancelled)));
    let constrained = GraphPublicationInput::replace_files(
        identity,
        facts(3, Vec::new())?,
        raw_facts(3, Vec::new()),
        GraphPolicy {
            maximum_input_bytes: 32,
            ..GraphPolicy::default()
        },
        &super::request()?,
    );
    assert!(matches!(constrained, Err(RocksError::Limit { .. })));
    Ok(())
}

#[test]
fn replacement_size_is_remeasured_after_the_old_immutable_proof_is_invalidated() -> TestResult {
    let mut replacement = raw_facts(3, Vec::new());
    replacement.symbols[0].signature = "x".repeat(50_000);
    let input = GraphPublicationInput::replace_files(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(8),
            base: GraphBase::Current(generation(1)),
        },
        facts(3, Vec::new())?,
        replacement,
        GraphPolicy {
            maximum_input_bytes: 32 * 1024,
            ..GraphPolicy::default()
        },
        &request()?,
    );
    assert!(matches!(input, Err(RocksError::Limit { .. })));
    Ok(())
}

#[test]
fn sparse_old_and_new_encoded_rows_share_one_workspace_allowance() -> TestResult {
    let mut old = raw_facts(10, Vec::new());
    for row in &mut old.symbols {
        row.signature = "x".repeat(60_000);
    }
    let mut replacement = old.clone();
    replacement.files[0].content_hash = digest(9);
    let result = GraphPublicationInput::replace_files(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(8),
            base: GraphBase::Current(generation(1)),
        },
        canonical(old)?,
        replacement,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    );
    assert!(matches!(result, Err(RocksError::Limit { .. })));
    Ok(())
}
