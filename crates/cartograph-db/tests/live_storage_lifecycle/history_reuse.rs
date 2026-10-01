use super::*;
use cartograph_db::{
    HistoryRefreshInput, HistoryRefreshMetadata, HistoryRefreshParameters, HistoryRefreshRequest,
};

const ROOT_IDENTITY: &str = "storage/history-reuse";

/// Reuse lookup for the test project's checkout state.
const fn reuse_query(
    head_commit: &str,
    shallow_history: bool,
    parameters: HistoryRefreshParameters,
) -> cartograph_db::HistoryReuseQuery<'_> {
    cartograph_db::HistoryReuseQuery {
        root_identity: ROOT_IDENTITY,
        head_commit,
        shallow_history,
        parameters,
    }
}

/// A stored churn/co-change refresh is reused only for the exact HEAD,
/// shallowness and inputs that produced it, and never outlives a refresh
/// without inputs or an explicit clear.
pub(super) async fn assert_history_reuse_record_lifecycle(database: &CartographDatabase) {
    let project = database
        .register_project(NewProject::new(ROOT_IDENTITY, digest(b"history-reuse")))
        .await
        .unwrap_or_else(|error| panic!("history project failed: {error}"));
    let head = "a".repeat(40);
    let parameters = HistoryRefreshParameters {
        max_commits: 100,
        commits_available: 3,
        algorithm_version: 1,
        churn: true,
        co_change: true,
    };
    let request = || {
        HistoryRefreshRequest::new(
            project.clone(),
            head.clone(),
            HistoryRefreshInput {
                metadata: HistoryRefreshMetadata {
                    shallow_history: false,
                    commits_scanned: 3,
                    truncated: false,
                    oversized_commits_skipped: 0,
                },
                files: Vec::new(),
                cochanges: Vec::new(),
            },
        )
        .unwrap_or_else(|error| panic!("history request failed: {error}"))
    };
    let reusable = |head: String, shallow: bool, parameters: HistoryRefreshParameters| async move {
        database
            .reusable_history_refresh(reuse_query(&head, shallow, parameters))
            .await
            .unwrap_or_else(|error| panic!("history reuse lookup failed: {error}"))
    };
    database
        .replace_file_history(request().with_parameters(parameters))
        .await
        .unwrap_or_else(|error| panic!("recorded refresh failed: {error}"));
    let reused = reusable(head.clone(), false, parameters)
        .await
        .unwrap_or_else(|| panic!("an identical refresh must be reusable"));
    assert!(reused.reused());
    for (other_head, shallow, changed) in [
        ("b".repeat(40), false, parameters),
        (head.clone(), true, parameters),
        (
            head.clone(),
            false,
            HistoryRefreshParameters {
                commits_available: 4,
                ..parameters
            },
        ),
        (
            head.clone(),
            false,
            HistoryRefreshParameters {
                algorithm_version: 2,
                ..parameters
            },
        ),
        (
            head.clone(),
            false,
            HistoryRefreshParameters {
                max_commits: 101,
                ..parameters
            },
        ),
        (
            head.clone(),
            false,
            HistoryRefreshParameters {
                co_change: false,
                ..parameters
            },
        ),
    ] {
        assert!(
            reusable(other_head, shallow, changed).await.is_none(),
            "any change to the reuse key forces a rescan"
        );
    }
    database
        .replace_file_history(request())
        .await
        .unwrap_or_else(|error| panic!("unrecorded refresh failed: {error}"));
    assert!(reusable(head.clone(), false, parameters).await.is_none());
    database
        .replace_file_history(request().with_parameters(parameters))
        .await
        .unwrap_or_else(|error| panic!("re-recorded refresh failed: {error}"));
    database
        .clear_file_history(&project)
        .await
        .unwrap_or_else(|error| panic!("history clear failed: {error}"));
    assert!(reusable(head, false, parameters).await.is_none());
}
