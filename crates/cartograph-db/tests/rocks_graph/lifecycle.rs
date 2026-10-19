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
fn reference_sort_workspace_is_bounded_by_one_file_instead_of_the_complete_table() -> TestResult {
    let mut raw = GenerationFacts::default();
    for file_number in 1..=4_u8 {
        let file_id = FileId::from_uuid_v8([file_number; 16]);
        raw.files.push(FileInput {
            file_id: file_id.clone(),
            normalized_path: format!("src/file_{file_number}.rs"),
            language: "rust".into(),
            content_hash: digest(3),
            byte_size: 7_000,
            parse_status: FileParseStatus::Parsed,
        });
        for start_byte in 0..7_000 {
            raw.references.push(ReferenceInput {
                file_id: file_id.clone(),
                owner_symbol_id: None,
                target_symbol_id: None,
                reference_name: "x".into(),
                reference_kind: "references".into(),
                start_byte,
                end_byte: start_byte + 1,
                confidence: 0.0,
                resolution_provenance: "unresolved".into(),
                site_count: 1,
                span_precision: ReferenceSpanPrecision::Exact,
            });
        }
    }
    let facts = canonical(raw)?;
    let expected_digest = facts.digest().clone();
    let input = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(1),
            source_revision: digest(9),
            base: GraphBase::Empty,
        },
        facts,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let published = store.publish_graph(input, &request()?)?;
    assert_eq!(published.revision.content_digest, Some(expected_digest));
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    assert_eq!(graph.info().references, 28_000);
    for file_number in 1..=4_u8 {
        let file_id = FileId::from_uuid_v8([file_number; 16]);
        let (rows, _) = graph.references(&file_id, None, 10, &mut budget()?)?;
        assert_eq!(rows.len(), 10);
        assert!(rows.iter().all(|row| row.file_id == file_id));
    }
    Ok(())
}

#[test]
fn adjacency_sort_uses_bounded_indices_for_a_large_hub() -> TestResult {
    let edges = (2..=20_000)
        .map(|target| edge(1, target, "native"))
        .collect();
    let facts = facts(20_000, edges)?;
    let expected_digest = facts.digest().clone();
    let input = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(1),
            source_revision: digest(9),
            base: GraphBase::Empty,
        },
        facts,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let published = store.publish_graph(input, &request()?)?;
    assert_eq!(published.revision.content_digest, Some(expected_digest));
    assert_eq!(published.report.maximum_degree, 19_999);
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    assert_eq!(graph.info().edges, 19_999);
    for target in [2, 10_000, 20_000] {
        let incoming = graph.neighbors(
            &symbol(target),
            GraphDirection::Incoming,
            None,
            10,
            &mut budget()?,
        )?;
        assert_eq!(incoming.links.len(), 1);
        assert_eq!(incoming.links[0].source_symbol_id(), &symbol(1));
    }
    Ok(())
}

#[test]
fn compact_publication_preserves_all_native_records_and_rows_larger_than_a_block() -> TestResult {
    let mut raw = auxiliary_facts();
    raw.files[0].byte_size = 250_000;
    raw.documents[0].code = "fn repeated() { call(); }\n".repeat(6_000);
    raw.symbols[0].qualified_name = "módulo::λ_function".into();
    raw.edges.push(edge(1, 2, "quoted\"\nλ"));
    raw.edges.push(EdgeInput {
        kind: EdgeKind::References,
        ..edge(1, 2, "native")
    });
    let reference = raw.references[0].clone();
    raw.references = (0..2_000)
        .map(|start_byte| ReferenceInput {
            start_byte,
            end_byte: start_byte + 1,
            ..reference.clone()
        })
        .collect();
    let expected = canonical(raw.clone())?;
    let expected_digest = expected.digest().clone();
    let identity = || GraphRevision {
        generation_id: generation(1),
        source_revision: digest(9),
        base: GraphBase::Empty,
    };
    let compact_policy = GraphPolicy {
        maximum_input_bytes: 128 * 1_024,
        maximum_working_bytes: 1_048_576,
        ..GraphPolicy::default()
    };
    assert!(matches!(
        GraphPublicationInput::new(
            identity(),
            canonical(raw.clone())?,
            compact_policy,
            &request()?
        ),
        Err(RocksError::Limit {
            resource: "graph_input_bytes"
        })
    ));
    let packed = GraphPublicationInput::prepare(
        identity(),
        canonical(raw)?,
        compact_policy,
        256 * 1_048_576,
        &request()?,
    )?;
    assert!(packed.shared_facts().is_none());
    let resident =
        GraphPublicationInput::new(identity(), expected, GraphPolicy::default(), &request()?)?;
    let resident_root = tempfile::tempdir()?;
    let packed_root = tempfile::tempdir()?;
    let resident_store =
        ProjectStore::open(resident_root.path(), &digest(1), StorePolicy::default())?;
    let packed_store = ProjectStore::open(packed_root.path(), &digest(1), StorePolicy::default())?;
    resident_store.publish_graph(resident, &request()?)?;
    let published = packed_store.publish_graph(packed, &request()?)?;
    assert_eq!(published.revision.content_digest, Some(expected_digest));
    let resident_snapshot = resident_store.current_snapshot(None)?;
    let packed_snapshot = packed_store.current_snapshot(None)?;
    for collection in [
        cartograph_db::rocks::Collection::Graph,
        cartograph_db::rocks::Collection::Search,
    ] {
        let scan = |snapshot: &cartograph_db::rocks::RevisionSnapshot<'_>| -> TestResult<_> {
            let mut after = None;
            let mut rows = Vec::new();
            loop {
                let page =
                    snapshot.scan_prefix(collection, b"", after.as_ref(), 256, &mut budget()?)?;
                rows.extend(page.rows);
                after = page.next_after;
                if after.is_none() {
                    return Ok(rows);
                }
            }
        };
        assert_eq!(scan(&packed_snapshot)?, scan(&resident_snapshot)?);
    }
    Ok(())
}

type NativeRecord = (cartograph_db::rocks::RecordKey, Vec<u8>);
type NativeRows = Vec<(cartograph_db::rocks::Collection, Vec<NativeRecord>)>;

fn all_native_rows(
    snapshot: &cartograph_db::rocks::RevisionSnapshot<'_>,
) -> TestResult<NativeRows> {
    let mut result = Vec::new();
    for collection in [
        cartograph_db::rocks::Collection::Graph,
        cartograph_db::rocks::Collection::Search,
    ] {
        let mut after = None;
        let mut rows = Vec::new();
        loop {
            let page =
                snapshot.scan_prefix(collection, b"", after.as_ref(), 256, &mut budget()?)?;
            rows.extend(page.rows);
            after = page.next_after;
            if after.is_none() {
                break;
            }
        }
        result.push((collection, rows));
    }
    Ok(result)
}

#[test]
fn compact_replacements_preserve_pinned_readers_deletions_and_reopened_records() -> TestResult {
    let resident_root = tempfile::tempdir()?;
    let compact_root = tempfile::tempdir()?;
    let resident = ProjectStore::open(resident_root.path(), &digest(1), StorePolicy::default())?;
    let compact = ProjectStore::open(compact_root.path(), &digest(1), StorePolicy::default())?;
    let mut pinned = None;
    let mut pinned_rows = None;
    for sequence in 1..=3 {
        let mut raw = auxiliary_facts();
        raw.documents[0].code = format!("fn revision_{sequence}() {{ call(); }}\n").repeat(8_000);
        raw.files[0].byte_size = 300_000;
        if sequence == 2 {
            raw.edges.clear();
            raw.references.clear();
            raw.numerical_sites.clear();
            raw.symbols[0].qualified_name = "changed::λ".into();
        } else if sequence == 3 {
            raw.edges = vec![edge(2, 1, "replacement")];
            raw.references[0].target_symbol_id = Some(symbol(2));
        }
        let identity = || GraphRevision {
            generation_id: generation(sequence),
            source_revision: digest(9),
            base: if sequence == 1 {
                GraphBase::Empty
            } else {
                GraphBase::Current(generation(sequence - 1))
            },
        };
        let packed = GraphPublicationInput::prepare(
            identity(),
            canonical(raw.clone())?,
            GraphPolicy {
                maximum_input_bytes: 64 * 1_024,
                ..GraphPolicy::default()
            },
            256 * 1_048_576,
            &request()?,
        )?;
        assert!(packed.shared_facts().is_none());
        resident.publish_graph(
            GraphPublicationInput::new(
                identity(),
                canonical(raw)?,
                GraphPolicy::default(),
                &request()?,
            )?,
            &request()?,
        )?;
        compact.publish_graph(packed, &request()?)?;
        let current = compact.current_snapshot(None)?;
        assert_eq!(
            all_native_rows(&current)?,
            all_native_rows(&resident.current_snapshot(None)?)?
        );
        if sequence == 1 {
            pinned_rows = Some(all_native_rows(&current)?);
            pinned = Some(current);
        } else if let Some(old) = &pinned {
            assert_eq!(Some(all_native_rows(old)?), pinned_rows);
        }
    }
    let final_rows = all_native_rows(&compact.current_snapshot(None)?)?;
    drop(pinned);
    drop(compact);
    let reopened = ProjectStore::open(compact_root.path(), &digest(1), StorePolicy::default())?;
    assert_eq!(
        all_native_rows(&reopened.current_snapshot(None)?)?,
        final_rows
    );
    assert_eq!(
        reopened.current_snapshot(None)?.revision().generation_id,
        generation(3)
    );
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
