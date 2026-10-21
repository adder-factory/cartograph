//! Real native impact discovery after Rust leaf test modules acquire test identity.

mod dependency_ownership;

use cartograph_agent::{IndexOptions, ProjectRuntime};
use cartograph_db::{
    CurrentGenerationLookup, CurrentGraphLookup, ExactTextLookup, FileTestImpactQuery,
    GraphDirection,
};
use cartograph_domain::NormalizedPath;
use cartograph_search::{DeterministicRetriever, TraversalBudget, TraversalRequest};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_leaf_test_identity_keeps_real_calls_and_both_affected_test_paths() -> TestResult {
    let project = tempfile::tempdir()?;
    std::fs::create_dir(project.path().join("src"))?;
    for (path, source) in [
        (
            "src/lib.rs",
            "pub fn store() {}\n#[cfg(test)] mod tests;\n#[cfg(test)] mod test;\n#[test] fn inline_store() { store(); }\n",
        ),
        (
            "src/tests.rs",
            "use crate::store;\nfn prepare_fixture() { store(); }\n#[test] fn stores_many() { prepare_fixture(); }\n",
        ),
        (
            "src/test.rs",
            "use crate::store;\n#[test] fn stores_one() { store(); }\n",
        ),
    ] {
        std::fs::write(project.path().join(path), source)?;
    }
    let runtime = ProjectRuntime::open(project.path()).await?;
    let indexed = runtime
        .index(IndexOptions::default().with_history_refresh(false))
        .await?;
    let generation = CurrentGenerationLookup::new(&indexed.project_id, &indexed.generation_id);
    let targets = runtime
        .database()
        .exact_current_symbols_by_name(ExactTextLookup::new(generation, "store", 10))
        .await?
        .into_iter()
        .filter(|symbol| symbol.path().as_str() == "src/lib.rs")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1);
    let callers = runtime
        .database()
        .current_graph_edges(CurrentGraphLookup::new(
            generation,
            std::slice::from_ref(targets[0].symbol_id()),
            GraphDirection::Incoming,
        ))
        .await?;
    assert_eq!(
        callers
            .iter()
            .filter(|edge| edge.edge_kind() == "calls")
            .count(),
        3
    );

    let retriever = DeterministicRetriever::new(runtime.database().clone());
    let symbol_impact = retriever
        .affected_tests(
            &TraversalRequest::new(
                indexed.project_id.clone(),
                [targets[0].symbol_id().clone()],
                TraversalBudget::new(5, 40)?,
            )?,
            40,
        )
        .await?;
    let file_impact = runtime
        .database()
        .current_file_test_impact(FileTestImpactQuery {
            project_id: &indexed.project_id,
            paths: &[NormalizedPath::parse("src/lib.rs")?],
            max_depth: 5,
            max_nodes: 40,
            limit: 40,
            test_path_regex: None,
        })
        .await?
        .ok_or("missing file impact generation")?;
    assert!(!symbol_impact.truncated());
    assert!(!file_impact.nodes_truncated());
    for path in ["src/tests.rs", "src/test.rs"] {
        assert!(
            symbol_impact
                .tests()
                .iter()
                .any(|test| test.symbol().path().as_str() == path)
        );
        assert!(file_impact.tests().iter().any(|test| test.path() == path));
    }
    for (path, has_inline_self_edge) in [
        ("src/lib.rs", true),
        ("src/tests.rs", false),
        ("src/test.rs", false),
    ] {
        assert_inline_test_self_edge(&runtime, generation, path, has_inline_self_edge).await?;
    }
    runtime.close().await?;
    Ok(())
}

async fn assert_inline_test_self_edge(
    runtime: &ProjectRuntime,
    generation: CurrentGenerationLookup<'_>,
    path: &str,
    expected: bool,
) -> TestResult {
    let file_symbols = runtime
        .database()
        .exact_current_symbols_by_name(ExactTextLookup::new(generation, path, 10))
        .await?;
    assert_eq!(file_symbols.len(), 1, "{path}");
    let edges = runtime
        .database()
        .current_graph_edges(CurrentGraphLookup::new(
            generation,
            std::slice::from_ref(file_symbols[0].symbol_id()),
            GraphDirection::Outgoing,
        ))
        .await?;
    assert_eq!(
        edges.iter().any(|edge| edge.edge_kind() == "tests"
            && edge.target_symbol_id() == file_symbols[0].symbol_id()
            && edge.provenance() == "native-rust-inline-test"),
        expected,
        "{path}"
    );
    Ok(())
}
