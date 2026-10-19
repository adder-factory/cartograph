use super::*;

#[test]
fn names_are_indexed_in_the_same_snapshot_and_survive_sparse_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut raw = raw_facts(1000, Vec::new());
    raw.symbols[0].qualified_name = "module::Älpha".into();
    raw.symbols[1].qualified_name = "other::Älpha".into();
    store.publish_graph(input(1, canonical(raw.clone())?)?, &request()?)?;
    let pinned = store.current_snapshot(None)?;
    let graph = pinned.graph(&mut budget()?)?;
    assert_eq!(graph.info().name_index_version, 1);
    let mut tight = ReadBudget::new(64, 32 * 1024, Duration::from_secs(5))?;
    let (first, cursor) = graph.symbols_by_name("ÄLPHA", None, 1, &mut tight)?;
    let (second, _) = graph.symbols_by_name("älpha", cursor.as_ref(), 4, &mut tight)?;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(first[0].symbol_id, second[0].symbol_id);
    assert_eq!(
        graph
            .symbols_by_name("MODULE::ÄLPHA", None, 4, &mut tight)?
            .0
            .len(),
        1
    );
    assert_eq!(
        graph.symbols_by_name("not_found", None, 4, &mut tight)?.0,
        []
    );
    let mut local = raw.clone();
    local.files[0].content_hash = digest(4);
    local.symbols[0].signature = "updated signature".into();
    let patch = GraphPublicationInput::replace_files(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(8),
            base: GraphBase::Current(generation(1)),
        },
        canonical(raw.clone())?,
        local,
        GraphPolicy::default(),
        &request()?,
    )?;
    store.publish_graph(patch, &request()?)?;
    let updated = store.current_snapshot(None)?;
    let updated = updated.graph(&mut budget()?)?;
    assert_eq!(
        updated
            .symbols_by_name("module::älpha", None, 4, &mut budget()?)?
            .0[0]
            .signature,
        "updated signature"
    );
    assert_eq!(
        graph
            .symbols_by_name("module::älpha", None, 4, &mut budget()?)?
            .0[0]
            .signature,
        "fn function()"
    );
    raw.symbols[0].qualified_name = "module::Renamed".into();
    store.publish_graph(input(3, canonical(raw)?)?, &request()?)?;
    let newest = store.current_snapshot(None)?;
    let newest = newest.graph(&mut budget()?)?;
    assert_eq!(
        newest
            .symbols_by_name("module::älpha", None, 4, &mut budget()?)?
            .0,
        []
    );
    assert_eq!(
        newest
            .symbols_by_name("Renamed", None, 4, &mut budget()?)?
            .0
            .len(),
        1
    );
    assert_eq!(
        graph
            .symbols_by_name("älpha", None, 4, &mut budget()?)?
            .0
            .len(),
        2
    );
    Ok(())
}

#[test]
fn optional_name_postings_never_expand_the_admitted_workspace() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let input = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(1),
            source_revision: digest(8),
            base: GraphBase::Empty,
        },
        facts(12_000, Vec::new())?,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    store.publish_graph(input, &request()?)?;
    let current = store.current_snapshot(None)?;
    let graph = current.graph(&mut budget()?)?;
    assert_eq!(graph.info().name_index_version, 0);
    assert!(
        graph
            .symbols_by_name("function_1", None, 4, &mut budget()?)
            .is_err()
    );
    assert_eq!(graph.symbols(None, 10, &mut budget()?)?.0.len(), 10);
    let mut old_manifest = serde_json::to_value(graph.info())?;
    old_manifest
        .as_object_mut()
        .ok_or("manifest not an object")?
        .remove("name_index_version");
    let old: cartograph_db::rocks::GraphInfo = serde_json::from_value(old_manifest)?;
    assert_eq!(old.name_index_version, 0);
    Ok(())
}
