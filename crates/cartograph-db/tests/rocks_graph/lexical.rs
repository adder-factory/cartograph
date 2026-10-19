use super::*;
use cartograph_db::SearchDocumentInput;
use cartograph_domain::{DocumentId, DocumentKind};
use std::fmt::Write as _;

#[test]
fn persisted_postings_bound_rare_queries_and_follow_sparse_updates_and_deletions() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut raw = raw_facts(200, Vec::new());
    raw.documents = raw
        .symbols
        .iter()
        .map(|symbol| {
            Ok(SearchDocumentInput {
                document_id: DocumentId::parse(symbol.symbol_id.as_str())
                    .map_err(|_| "document ID")?,
                file_id: Some(symbol.file_id.clone()),
                symbol_id: Some(symbol.symbol_id.clone()),
                path: "src/lib.rs".into(),
                language: "rust".into(),
                kind: DocumentKind::Symbol,
                qualified_name: symbol.qualified_name.clone(),
                code: if symbol.symbol_id == super::symbol(1) {
                    "rareuniquetoken"
                } else {
                    "common"
                }
                .into(),
                natural_text: "Äpfel".into(),
                metadata: serde_json::json!({}),
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    store.publish_graph(input(1, canonical(raw.clone())?)?, &request()?)?;
    let base = canonical(raw.clone())?;
    let pinned = store.current_snapshot(None)?;
    let old = pinned.graph(&mut budget()?)?;
    let mut tight = ReadBudget::new(20, 8192, Duration::from_secs(5))?;
    let (hits, truncated) = old.search_code("rareuniquetoken", 5, &mut tight)?;
    assert_eq!(hits.len(), 1);
    assert!(!truncated);
    assert_eq!(hits[0].symbol_id(), Some(&symbol(1)));
    assert_eq!(old.search_code("ÄPFEL", 1, &mut budget()?)?.0.len(), 1);
    raw.files[0].content_hash = digest(4);
    raw.documents[0].code = "freshreplacement".into();
    let sparse = GraphPublicationInput::replace_files(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(8),
            base: GraphBase::Current(generation(1)),
        },
        base,
        raw.clone(),
        GraphPolicy::default(),
        &request()?,
    )?;
    store.publish_graph(sparse, &request()?)?;
    let current = store.current_snapshot(None)?;
    let current = current.graph(&mut budget()?)?;
    assert_eq!(
        current.search_code("rareuniquetoken", 5, &mut budget()?)?.0,
        []
    );
    assert_eq!(
        current
            .search_code("freshreplacement", 5, &mut budget()?)?
            .0
            .len(),
        1
    );
    raw.documents.remove(0);
    store.publish_graph(input(3, canonical(raw)?)?, &request()?)?;
    let newest = store.current_snapshot(None)?;
    let newest = newest.graph(&mut budget()?)?;
    assert_eq!(
        newest
            .lexical_statistics(&mut budget()?)?
            .ok_or("statistics missing")?
            .documents,
        199
    );
    assert_eq!(
        newest.search_code("freshreplacement", 5, &mut budget()?)?.0,
        []
    );
    assert_eq!(
        old.search_code("rareuniquetoken", 5, &mut budget()?)?
            .0
            .len(),
        1
    );
    assert_eq!(
        old.search_code("freshreplacement", 5, &mut budget()?)?.0,
        []
    );
    Ok(())
}

#[test]
fn optional_posting_workspace_never_rejects_a_complete_graph() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut raw = raw_facts(1, Vec::new());
    let mut code = String::new();
    for number in 0..2500 {
        write!(&mut code, "unique{number} ")?;
    }
    raw.documents.push(SearchDocumentInput {
        document_id: DocumentId::from_uuid_v8([1; 16]),
        file_id: Some(raw.files[0].file_id.clone()),
        symbol_id: Some(symbol(1)),
        path: "src/lib.rs".into(),
        language: "rust".into(),
        kind: DocumentKind::Symbol,
        qualified_name: "function_1".into(),
        code,
        natural_text: String::new(),
        metadata: serde_json::json!({}),
    });
    let publication = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(1),
            source_revision: digest(8),
            base: GraphBase::Empty,
        },
        canonical(raw.clone())?,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    store.publish_graph(publication, &request()?)?;
    let pinned = store.current_snapshot(None)?;
    let graph = pinned.graph(&mut budget()?)?;
    assert_eq!(
        graph
            .lexical_statistics(&mut budget()?)?
            .ok_or("statistics missing")?
            .scan_documents,
        1
    );
    assert_eq!(
        graph.search_code("unique2499", 1, &mut budget()?)?.0[0].symbol_id(),
        Some(&symbol(1))
    );
    raw.documents[0].code = "smallreplacement".into();
    store.publish_graph(input(2, canonical(raw)?)?, &request()?)?;
    let current = store.current_snapshot(None)?;
    let current = current.graph(&mut budget()?)?;
    assert_eq!(
        current
            .lexical_statistics(&mut budget()?)?
            .ok_or("statistics missing")?
            .scan_documents,
        0
    );
    assert_eq!(
        current
            .search_code("smallreplacement", 1, &mut budget()?)?
            .0
            .len(),
        1
    );
    assert_eq!(current.search_code("unique2499", 1, &mut budget()?)?.0, []);
    assert_eq!(
        graph.search_code("unique2499", 1, &mut budget()?)?.0.len(),
        1
    );
    Ok(())
}

fn document(raw: &GenerationFacts, id: u64, code: String) -> TestResult<SearchDocumentInput> {
    Ok(SearchDocumentInput {
        document_id: DocumentId::parse(symbol(id).as_str())?,
        file_id: Some(raw.files[0].file_id.clone()),
        symbol_id: Some(symbol(1)),
        path: "src/lib.rs".into(),
        language: "rust".into(),
        kind: DocumentKind::Symbol,
        qualified_name: "function_1".into(),
        code,
        natural_text: String::new(),
        metadata: serde_json::json!({}),
    })
}

#[test]
fn uuid_table_streaming_leaves_workspace_for_lexical_postings() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut raw = raw_facts(1, Vec::new());
    // The UUID-ordered document table must stream without a second sort array,
    // leaving the complete 1 MiB allowance for its bounded lexical vocabulary.
    for id in 1..=14_560 {
        raw.documents
            .push(document(&raw, id, "raretabletoken".into())?);
    }
    let publication = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(1),
            source_revision: digest(8),
            base: GraphBase::Empty,
        },
        canonical(raw)?,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    store.publish_graph(publication, &request()?)?;
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    let statistics = graph
        .lexical_statistics(&mut budget()?)?
        .ok_or("statistics missing")?;
    assert_eq!(statistics.documents, 14_560);
    assert_eq!(statistics.scan_documents, 0);
    assert_eq!(
        graph
            .search_code("raretabletoken", 1, &mut budget()?)?
            .0
            .len(),
        1
    );
    Ok(())
}

#[test]
fn sparse_payload_and_vocabulary_share_the_working_budget() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut raw = raw_facts(1, Vec::new());
    for id in 1..=8 {
        raw.documents.push(document(&raw, id, "original".into())?);
    }
    let base = canonical(raw.clone())?;
    store.publish_graph(input(1, canonical(raw.clone())?)?, &request()?)?;
    raw.files[0].content_hash = digest(4);
    for document in &mut raw.documents {
        document.code.clear();
        for i in 0..450 {
            write!(document.code, "sparse{i} ")?;
        }
        document.metadata = serde_json::json!({"payload":"x".repeat(58_000)});
    }
    let publication = GraphPublicationInput::replace_files(
        GraphRevision {
            generation_id: generation(2),
            source_revision: digest(8),
            base: GraphBase::Current(generation(1)),
        },
        base,
        raw,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    store.publish_graph(publication, &request()?)?;
    let snapshot = store.current_snapshot(None)?;
    let graph = snapshot.graph(&mut budget()?)?;
    assert_eq!(
        graph
            .lexical_statistics(&mut budget()?)?
            .ok_or("statistics missing")?
            .scan_documents,
        8
    );
    assert_eq!(
        graph.search_code("sparse449", 1, &mut budget()?)?.0.len(),
        1
    );
    assert_eq!(graph.search_code("original", 1, &mut budget()?)?.0, []);
    Ok(())
}

#[test]
fn prior_large_vocabulary_streams_across_batch_boundaries_and_lower_policy() -> TestResult {
    let root = tempfile::tempdir()?;
    let store = ProjectStore::open(root.path(), &digest(1), StorePolicy::default())?;
    let mut raw = raw_facts(1, Vec::new());
    let mut code = String::from("sharedtoken ");
    for id in 0..10_000 {
        write!(&mut code, "original{id} ")?;
    }
    raw.documents.push(document(&raw, 1, code)?);
    store.publish_graph(input(1, canonical(raw.clone())?)?, &request()?)?;
    let pinned = store.current_snapshot(None)?;
    let mut replacement = String::from("sharedtoken ");
    for id in 0..5000 {
        write!(&mut replacement, "original{id} replacement{id} ")?;
    }
    raw.documents[0].code = replacement;
    store.publish_graph(input(2, canonical(raw.clone())?)?, &request()?)?;
    let second = store.current_snapshot(None)?;
    let graph = second.graph(&mut budget()?)?;
    assert_eq!(
        graph
            .lexical_statistics(&mut budget()?)?
            .ok_or("statistics missing")?
            .scan_documents,
        0
    );
    assert_eq!(
        graph
            .search_code("original4999", 1, &mut budget()?)?
            .0
            .len(),
        1
    );
    assert_eq!(graph.search_code("original9999", 1, &mut budget()?)?.0, []);
    raw.documents[0].code = "sharedtoken afterlower".into();
    let lower = GraphPublicationInput::new(
        GraphRevision {
            generation_id: generation(3),
            source_revision: digest(8),
            base: GraphBase::Current(generation(2)),
        },
        canonical(raw)?,
        GraphPolicy {
            maximum_working_bytes: 1_048_576,
            ..GraphPolicy::default()
        },
        &request()?,
    )?;
    store.publish_graph(lower, &request()?)?;
    let current = store.current_snapshot(None)?;
    let graph = current.graph(&mut budget()?)?;
    assert_eq!(
        graph.search_code("sharedtoken", 1, &mut budget()?)?.0.len(),
        1
    );
    assert_eq!(
        graph.search_code("afterlower", 1, &mut budget()?)?.0.len(),
        1
    );
    assert_eq!(
        graph.search_code("replacement4999", 1, &mut budget()?)?.0,
        []
    );
    assert_eq!(
        pinned
            .graph(&mut budget()?)?
            .search_code("original9999", 1, &mut budget()?)?
            .0
            .len(),
        1
    );
    Ok(())
}
