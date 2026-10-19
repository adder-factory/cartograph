use cartograph_db::{
    NumericalSiteInput, ReferenceInput, ReferenceSpanPrecision, SearchDocumentInput,
};
use cartograph_domain::{DocumentId, DocumentKind, NumericalSiteId};

use super::*;

fn auxiliary_facts() -> GenerationFacts {
    let mut facts = raw_facts(2, vec![edge(1, 2, "native")]);
    let file_id = facts.files[0].file_id.clone();
    facts.references.push(ReferenceInput {
        file_id: file_id.clone(),
        owner_symbol_id: Some(symbol(1)),
        target_symbol_id: None,
        reference_name: "x".into(),
        reference_kind: "calls".into(),
        start_byte: 0,
        end_byte: 1,
        confidence: 0.0,
        resolution_provenance: "unresolved".into(),
        site_count: 1,
        span_precision: ReferenceSpanPrecision::Exact,
    });
    facts.numerical_sites.push(NumericalSiteInput {
        site_id: NumericalSiteId::from_uuid_v8([3; 16]),
        file_id: file_id.clone(),
        owner_symbol_id: Some(symbol(1)),
        start_byte: 0,
        end_byte: 1,
        start_line: 1,
        end_line: 1,
        operation: "division".into(),
        hazard: "none_observed".into(),
        precision: "unknown".into(),
        expression_digest: digest(4),
        confidence_ppm: 500_000,
        provenance: "rust_ast_v1".into(),
        evidence_level: "heuristic".into(),
        unknowns: "input_range".into(),
    });
    facts.documents.push(SearchDocumentInput {
        document_id: DocumentId::from_uuid_v8([4; 16]),
        file_id: Some(file_id),
        symbol_id: Some(symbol(1)),
        path: "src/lib.rs".into(),
        language: "rust".into(),
        kind: DocumentKind::Symbol,
        qualified_name: "function_1".into(),
        code: "x".into(),
        natural_text: String::new(),
        metadata: serde_json::json!({"kind":"function"}),
    });
    facts
}

#[test]
fn canonical_auxiliary_records_roundtrip_and_are_removed_on_replacement() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let expected = auxiliary_facts();
    let file = expected.files[0].clone();
    let symbol_row = expected.symbols[0].clone();
    let numerical = expected.numerical_sites[0].clone();
    let reference = expected.references[0].clone();
    let document_id = expected.documents[0].document_id.clone();
    let canonical = canonical(expected)?;
    let expected_document = serde_json::to_value(&canonical.documents()[0])?;
    store.publish_graph(input(1, canonical)?, &request()?)?;
    let pinned = store.current_snapshot(None)?;
    let graph = pinned.graph(&mut budget()?)?;
    assert_eq!(graph.info().documents, 1);
    assert_eq!(
        graph.file(&file.file_id, &mut budget()?)?,
        Some(file.clone())
    );
    assert_eq!(
        graph.symbol(&symbol_row.symbol_id, &mut budget()?)?,
        Some(symbol_row)
    );
    assert_eq!(
        graph.numerical_site(&numerical.site_id, &mut budget()?)?,
        Some(numerical.clone())
    );
    assert_eq!(
        graph.references(&file.file_id, None, 10, &mut budget()?)?.0,
        vec![reference]
    );
    assert_eq!(
        serde_json::to_value(graph.document(&document_id, &mut budget()?)?)?,
        expected_document
    );
    store.publish_graph(input(2, facts(0, Vec::new())?)?, &request()?)?;
    let current = store.current_snapshot(None)?;
    let graph = current.graph(&mut budget()?)?;
    assert_eq!(graph.info().symbols, 0);
    assert!(graph.symbol(&symbol(1), &mut budget()?)?.is_none());
    assert!(
        graph
            .numerical_site(&numerical.site_id, &mut budget()?)?
            .is_none()
    );
    assert_eq!(
        graph.references(&file.file_id, None, 10, &mut budget()?)?.0,
        [] as [ReferenceInput; 0]
    );
    assert!(graph.document(&document_id, &mut budget()?)?.is_none());
    Ok(())
}

#[test]
fn partial_staging_failure_is_invisible_releases_writer_and_survives_reopen() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    store.publish_graph(
        input(1, facts(2, vec![edge(1, 2, "native")])?)?,
        &request()?,
    )?;
    let mut oversized = auxiliary_facts();
    oversized.symbols = raw_facts(600, Vec::new()).symbols;
    oversized.documents[0].code = "x".repeat(1_048_576);
    let failing = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(9),
            base: GraphBase::Current(generation(1)),
        },
        canonical(oversized)?,
        GraphPolicy {
            maximum_batch_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    assert!(matches!(
        store
            .publish_graph(failing, &request()?)
            .map_err(|error| error.primary),
        Err(RocksError::Limit {
            resource: "graph_batch_bytes"
        })
    ));
    assert_eq!(
        store.current_snapshot(None)?.revision().generation_id,
        generation(1)
    );
    drop(store);
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let retry = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(3),
            source_revision: digest(9),
            base: GraphBase::Current(generation(1)),
        },
        facts(2, Vec::new())?,
        GraphPolicy::default(),
        &request()?,
    )?;
    store.publish_graph(retry, &request()?)?;
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    assert_eq!(graph.info().symbols, 2);
    assert!(
        graph
            .neighbors(
                &symbol(2),
                GraphDirection::Incoming,
                None,
                10,
                &mut budget()?
            )?
            .links
            .is_empty()
    );
    assert!(graph.symbol(&symbol(600), &mut budget()?)?.is_none());
    Ok(())
}

#[test]
fn automatic_retention_preserves_pinned_readers_without_resurrecting_deleted_edges() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    store.publish_graph(
        input(1, facts(2, vec![edge(1, 2, "native")])?)?,
        &request()?,
    )?;
    let old = store.current_snapshot(None)?;
    for sequence in 2..=12 {
        store.publish_graph(input(sequence, facts(2, Vec::new())?)?, &request()?)?;
    }
    assert!(matches!(
        store.retained_snapshot(&generation(1)),
        Err(RocksError::GenerationUnavailable)
    ));
    assert_eq!(
        old.graph(&mut budget()?)?
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
    let current = store.current_snapshot(None)?;
    assert!(
        current
            .graph(&mut budget()?)?
            .neighbors(
                &symbol(1),
                GraphDirection::Outgoing,
                None,
                10,
                &mut budget()?
            )?
            .links
            .is_empty()
    );
    Ok(())
}

#[test]
fn cancelled_inputs_and_resource_bounds_leave_no_writer_or_partial_revision() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let cancelled = request()?;
    cancelled.cancel();
    let identity = || GraphRevision {
        generation_id: generation(1),
        source_revision: digest(9),
        base: GraphBase::Empty,
    };
    assert!(matches!(
        GraphPublicationInput::new(
            identity(),
            facts(2, Vec::new())?,
            GraphPolicy::default(),
            &cancelled
        ),
        Err(RocksError::Cancelled)
    ));
    assert!(
        GraphPublicationInput::new(
            identity(),
            facts(2, Vec::new())?,
            GraphPolicy {
                maximum_input_bytes: 1,
                ..GraphPolicy::default()
            },
            &request()?
        )
        .is_err()
    );
    assert!(
        GraphPublicationInput::new(
            identity(),
            facts(2, Vec::new())?,
            GraphPolicy {
                maximum_working_bytes: 0,
                ..GraphPolicy::default()
            },
            &request()?
        )
        .is_err()
    );
    assert!(matches!(
        store
            .publish_graph(input(1, facts(2, Vec::new())?)?, &cancelled)
            .map_err(|error| error.primary),
        Err(RocksError::Cancelled)
    ));
    store.publish_graph(input(1, facts(2, Vec::new())?)?, &request()?)?;
    Ok(())
}

#[test]
fn source_admission_charges_retained_capacity_and_replacement_only_once() -> TestResult {
    let build = || input(1, facts(1, Vec::new())?);
    let mut pattern = String::with_capacity(5 * 1_048_576);
    pattern.push_str("tmp/**");
    assert!(build()?.with_source_admission(vec![pattern], None).is_err());
    let mut patterns = Vec::with_capacity(200_000);
    patterns.push("tmp/**".to_owned());
    assert!(build()?.with_source_admission(patterns, None).is_err());
    let admitted = build()?.with_source_admission(vec!["tmp/**".to_owned()], None)?;
    assert!(
        admitted
            .with_source_admission(vec!["src/private/**".to_owned()], Some(1024))
            .is_ok()
    );
    Ok(())
}
