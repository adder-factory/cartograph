use super::*;

#[test]
fn large_reference_table_publishes_and_deletes_with_one_mib_sort_workspace() -> TestResult {
    use cartograph_db::{ReferenceInput, ReferenceSpanPrecision};
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let make_facts = |stride: usize| -> TestResult<CanonicalGenerationFacts> {
        let mut raw = raw_facts(1, Vec::new());
        raw.files[0].byte_size = 20_000;
        raw.references = (0_u64..12_000)
            .step_by(stride)
            .map(|offset| ReferenceInput {
                file_id: raw.files[0].file_id.clone(),
                owner_symbol_id: Some(symbol(1)),
                target_symbol_id: Some(symbol(1)),
                reference_name: "function_1".to_owned(),
                reference_kind: "identifier".to_owned(),
                start_byte: offset,
                end_byte: offset + 1,
                confidence: 1.0,
                resolution_provenance: "native".to_owned(),
                site_count: 1,
                span_precision: ReferenceSpanPrecision::Exact,
            })
            .collect();
        canonical(raw)
    };
    for sequence in 1_u64..=2 {
        let native = GraphPublicationInput::new(
            GraphRevision {
                generation_id: generation(sequence),
                source_revision: digest(9),
                base: if sequence == 1 {
                    GraphBase::Empty
                } else {
                    GraphBase::Current(generation(1))
                },
            },
            make_facts(usize::try_from(sequence)?)?,
            GraphPolicy {
                maximum_working_bytes: 1_048_576,
                ..GraphPolicy::default()
            },
            &request()?,
        )?;
        let published = store.publish_graph(native, &request()?)?;
        if sequence == 2 {
            assert_eq!(published.report.deleted_records, 6_000);
        }
        let snapshot = store.current_snapshot(None)?;
        let mut allowance = budget()?;
        let graph = snapshot.graph(&mut allowance)?;
        let mut after = None;
        let mut offsets = std::collections::BTreeSet::new();
        loop {
            let (rows, cursor) = graph.reference_records(after.as_ref(), 256, &mut allowance)?;
            for row in rows {
                assert_eq!(row.target_symbol_id, Some(symbol(1)));
                assert_eq!(row.start_byte % sequence, 0);
                assert!(offsets.insert(row.start_byte));
            }
            after = cursor;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(offsets.len(), 12_000 / usize::try_from(sequence)?);
    }
    Ok(())
}

#[test]
fn large_vertices_use_dense_bounded_blocks_and_stable_pagination() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let edges = (2..=2_000)
        .map(|target| edge(1, target, "native"))
        .collect();
    let first = store.publish_graph(input(1, facts(4_000, edges)?)?, &request()?)?;
    assert_eq!(first.report.maximum_degree, 1_999);
    // Every destination has one incoming block; the hub needs at most 64.
    assert!(first.report.adjacency_blocks - 1_999 <= 64);
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    let mut cursor = None;
    let mut observed = Vec::new();
    let mut read_budget = budget()?;
    loop {
        let page = graph.neighbors(
            &symbol(1),
            GraphDirection::Outgoing,
            cursor.as_ref(),
            31,
            &mut read_budget,
        )?;
        for link in page.links {
            observed.push(link.target_symbol_id().clone());
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(observed, (2..=2_000).map(symbol).collect::<Vec<_>>());
    let mut edges = (2..=2_000)
        .map(|target| edge(1, target, "native"))
        .collect::<Vec<_>>();
    edges.push(edge(1, 3_500, "native"));
    let updated = store.publish_graph(input(2, facts(4_000, edges)?)?, &request()?)?;
    assert!(updated.report.written_records <= 8);
    assert!(updated.report.deleted_records <= 2);
    assert!(updated.report.reused_records > 7_000);
    Ok(())
}

#[test]
fn unchanged_graph_writes_no_payload_and_property_edits_do_not_rewrite_topology() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let edges = || {
        (2..=200)
            .map(|target| edge(1, target, "native"))
            .collect::<Vec<_>>()
    };
    store.publish_graph(input(1, facts(200, edges())?)?, &request()?)?;
    let unchanged = store.publish_graph(input(2, facts(200, edges())?)?, &request()?)?;
    assert_eq!(unchanged.revision.changed_rows, 0);
    assert_eq!(unchanged.report.written_records, 0);
    assert_eq!(unchanged.report.deleted_records, 0);
    assert_eq!(
        unchanged.report.candidate_records,
        unchanged.report.reused_records
    );
    let mut changed = edges();
    changed[0].confidence = 0.5;
    changed[0].site_count = 7;
    let property_edit = store.publish_graph(input(3, facts(200, changed)?)?, &request()?)?;
    assert_eq!(property_edit.report.written_records, 2); // one property and the logical digest
    assert_eq!(property_edit.report.deleted_records, 0);
    Ok(())
}

#[test]
fn small_vertex_split_and_merge_preserve_every_edge_without_stale_blocks() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    for (index, degree) in [128_u64, 129, 128, 0].into_iter().enumerate() {
        let sequence = u64::try_from(index)? + 1;
        store.publish_graph(
            input(
                sequence,
                facts(
                    130,
                    (2..degree + 2)
                        .map(|target| edge(1, target, "native"))
                        .collect(),
                )?,
            )?,
            &request()?,
        )?;
        let snapshot = store.current_snapshot(None)?;
        let graph = snapshot.graph(&mut budget()?)?;
        let page = graph.neighbors(
            &symbol(1),
            GraphDirection::Outgoing,
            None,
            200,
            &mut budget()?,
        )?;
        assert_eq!(page.links.len(), usize::try_from(degree)?);
        assert_eq!(
            page.links
                .iter()
                .map(|link| link.target_symbol_id().clone())
                .collect::<Vec<_>>(),
            (2..degree + 2).map(symbol).collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[test]
fn cursors_and_property_links_are_fenced_by_project_generation_and_direction() -> TestResult {
    let first_root = tempfile::tempdir()?;
    let second_root = tempfile::tempdir()?;
    let first = ProjectStore::open(first_root.path(), &digest(1), StorePolicy::default())?;
    let second = ProjectStore::open(second_root.path(), &digest(2), StorePolicy::default())?;
    for store in [&first, &second] {
        store.publish_graph(
            input(
                1,
                facts(3, vec![edge(1, 2, "native"), edge(1, 3, "native")])?,
            )?,
            &request()?,
        )?;
    }
    let snapshot = first.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    let page = graph.neighbors(
        &symbol(1),
        GraphDirection::Outgoing,
        None,
        1,
        &mut budget()?,
    )?;
    let cursor = page.next.as_ref().ok_or("missing cursor")?;
    assert!(matches!(
        graph.neighbors(
            &symbol(1),
            GraphDirection::Incoming,
            Some(cursor),
            1,
            &mut budget()?
        ),
        Err(RocksError::GenerationChanged)
    ));
    assert!(matches!(
        graph.neighbors(
            &symbol(2),
            GraphDirection::Outgoing,
            Some(cursor),
            1,
            &mut budget()?
        ),
        Err(RocksError::GenerationChanged)
    ));
    let second_view = second.current_snapshot(None)?;
    let second_graph = second_view.graph(&mut budget()?)?;
    assert!(matches!(
        second_graph.neighbors(
            &symbol(1),
            GraphDirection::Outgoing,
            Some(cursor),
            1,
            &mut budget()?
        ),
        Err(RocksError::GenerationChanged)
    ));
    assert!(matches!(
        second_graph.edge(&page.links[0], &mut budget()?),
        Err(RocksError::GenerationChanged)
    ));
    first.publish_graph(
        input(2, facts(3, vec![edge(1, 3, "native")])?)?,
        &request()?,
    )?;
    let current = first.current_snapshot(None)?;
    let current_graph = current.graph(&mut budget()?)?;
    assert!(matches!(
        current_graph.neighbors(
            &symbol(1),
            GraphDirection::Outgoing,
            Some(cursor),
            1,
            &mut budget()?
        ),
        Err(RocksError::GenerationChanged)
    ));
    assert!(matches!(
        current_graph.edge(&page.links[0], &mut budget()?),
        Err(RocksError::GenerationChanged)
    ));
    Ok(())
}

#[test]
fn every_relationship_kind_survives_binary_encoding_and_budget_limits() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let edges = EdgeKind::ALL
        .into_iter()
        .map(|kind| {
            let mut row = edge(1, 2, "native");
            row.kind = kind;
            row
        })
        .collect();
    store.publish_graph(input(1, facts(2, edges)?)?, &request()?)?;
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    let page = graph.neighbors(
        &symbol(1),
        GraphDirection::Outgoing,
        None,
        100,
        &mut budget()?,
    )?;
    assert_eq!(
        page.links
            .iter()
            .map(cartograph_db::rocks::GraphLink::kind)
            .collect::<Vec<_>>(),
        EdgeKind::ALL
    );
    assert!(
        graph
            .neighbors(
                &symbol(1),
                GraphDirection::Outgoing,
                None,
                0,
                &mut budget()?
            )
            .is_err()
    );
    assert!(
        graph
            .neighbors(
                &symbol(1),
                GraphDirection::Outgoing,
                None,
                4_097,
                &mut budget()?
            )
            .is_err()
    );
    assert!(
        graph
            .neighbors(
                &symbol(1),
                GraphDirection::Outgoing,
                None,
                100,
                &mut ReadBudget::new(1, 1, Duration::from_secs(1))?
            )
            .is_err()
    );
    Ok(())
}
