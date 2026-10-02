//! Integration coverage for Cartograph project-runtime and agent evidence contracts.

mod dependency_ownership;

use std::{path::Path, process::Command};

use cartograph_agent::{
    GitChangeKind, GitLineHistoryRequest, GitLineRange, ReviewError, ReviewOptions,
    SourceCompareOptions, discover_git_blame, discover_git_commit_paths, discover_git_comparison,
    discover_git_history, discover_git_line_history, discover_git_rename_evidence,
    discover_source_comparison, trace_git_culprits,
};
use cartograph_domain::NormalizedPath;

#[tokio::test]
async fn compare_to_head_discovers_dirty_tracked_and_untracked_files_deterministically() {
    let repository = repository_fixture();
    write(
        repository.path().join("src/z.rs"),
        "pub fn z() { println!(\"dirty\"); }\n",
    );
    write(repository.path().join("src/a.rs"), "pub fn a() {}\n");

    let options =
        ReviewOptions::new("HEAD").unwrap_or_else(|error| panic!("review options failed: {error}"));
    let comparison = discover_git_comparison(repository.path(), &options)
        .await
        .unwrap_or_else(|error| panic!("git comparison failed: {error}"));

    assert!(comparison.worktree_dirty());
    assert!(!comparison.truncated());
    assert_eq!(
        comparison
            .files()
            .iter()
            .map(|file| (file.path().as_str(), file.kind()))
            .collect::<Vec<_>>(),
        vec![
            ("src/a.rs", GitChangeKind::Untracked),
            ("src/z.rs", GitChangeKind::Modified),
        ]
    );
    assert_eq!(comparison.base_commit().len(), 40);
}

#[tokio::test]
async fn compare_to_ancestor_includes_committed_changes_without_marking_worktree_dirty() {
    let repository = repository_fixture();
    write(
        repository.path().join("src/z.rs"),
        "pub fn z() { println!(\"committed\"); }\n",
    );
    git(repository.path(), &["add", "src/z.rs"]);
    git(repository.path(), &["commit", "-m", "change z"]);

    let options = ReviewOptions::new("HEAD~1")
        .unwrap_or_else(|error| panic!("review options failed: {error}"));
    let comparison = discover_git_comparison(repository.path(), &options)
        .await
        .unwrap_or_else(|error| panic!("git comparison failed: {error}"));

    assert!(!comparison.worktree_dirty());
    assert_eq!(comparison.files().len(), 1);
    assert_eq!(comparison.files()[0].path().as_str(), "src/z.rs");
    assert_eq!(comparison.files()[0].kind(), GitChangeKind::Modified);
}

#[tokio::test]
async fn malformed_and_missing_refs_fail_with_distinct_redacted_errors() {
    let repository = repository_fixture();
    assert_eq!(
        ReviewOptions::new("--upload-pack=/tmp/evil"),
        Err(ReviewError::InvalidRef)
    );
    assert_eq!(ReviewOptions::new("bad ref"), Err(ReviewError::InvalidRef));

    let options = ReviewOptions::new("refs/heads/does-not-exist")
        .unwrap_or_else(|error| panic!("missing-ref options failed: {error}"));
    let Err(error) = discover_git_comparison(repository.path(), &options).await else {
        panic!("missing Git ref unexpectedly resolved");
    };
    assert_eq!(error, ReviewError::GitRefNotFound);
    let rendered = error.to_string();
    assert!(!rendered.contains("does-not-exist"));

    let shell_shaped = ReviewOptions::new("HEAD;touch-owned")
        .unwrap_or_else(|error| panic!("shell-shaped options failed: {error}"));
    assert_eq!(
        discover_git_comparison(repository.path(), &shell_shaped).await,
        Err(ReviewError::GitRefNotFound)
    );
    assert!(!repository.path().join("touch-owned").exists());
}

#[tokio::test]
async fn changed_file_limit_is_explicit_and_keeps_the_stable_prefix() {
    let repository = repository_fixture();
    write(repository.path().join("src/a.rs"), "pub fn a() {}\n");
    write(repository.path().join("src/b.rs"), "pub fn b() {}\n");
    let options = ReviewOptions::new("HEAD")
        .and_then(|options| options.with_max_changed_files(1))
        .unwrap_or_else(|error| panic!("review options failed: {error}"));
    let comparison = discover_git_comparison(repository.path(), &options)
        .await
        .unwrap_or_else(|error| panic!("git comparison failed: {error}"));

    assert!(comparison.truncated());
    assert_eq!(comparison.files().len(), 1);
    assert_eq!(comparison.files()[0].path().as_str(), "src/a.rs");
}

#[tokio::test]
async fn review_path_filter_is_canonical_and_does_not_match_sibling_prefixes() {
    let repository = repository_fixture();
    for directory in ["src2", "tests"] {
        std::fs::create_dir_all(repository.path().join(directory))
            .unwrap_or_else(|error| panic!("filtered fixture directory failed: {error}"));
    }
    write(repository.path().join("src/a.rs"), "pub fn a() {}\n");
    write(repository.path().join("src2/b.rs"), "pub fn b() {}\n");
    write(repository.path().join("tests/c.rs"), "pub fn c() {}\n");
    let options = ReviewOptions::new("HEAD")
        .and_then(|options| options.with_path_filter(Some("./src/")))
        .unwrap_or_else(|error| panic!("review path filter failed: {error}"));
    let comparison = discover_git_comparison(repository.path(), &options)
        .await
        .unwrap_or_else(|error| panic!("filtered Git comparison failed: {error}"));

    assert_eq!(comparison.files().len(), 1);
    assert_eq!(comparison.files()[0].path().as_str(), "src/a.rs");
    assert_eq!(
        ReviewOptions::new("HEAD").and_then(|options| options.with_path_filter(Some("../src"))),
        Err(ReviewError::InvalidOptions)
    );
}

#[tokio::test]
async fn nested_project_review_excludes_changes_outside_its_root() {
    let repository = repository_fixture();
    std::fs::create_dir_all(repository.path().join("workspace"))
        .unwrap_or_else(|error| panic!("nested workspace failed: {error}"));
    write(
        repository.path().join("workspace/app.rs"),
        "pub fn app() {}\n",
    );
    write(
        repository.path().join("outside.rs"),
        "pub fn outside() {}\n",
    );
    git(
        repository.path(),
        &["add", "workspace/app.rs", "outside.rs"],
    );
    git(repository.path(), &["commit", "-m", "nested baseline"]);
    write(
        repository.path().join("workspace/app.rs"),
        "pub fn app() { println!(\"dirty\"); }\n",
    );
    write(
        repository.path().join("outside.rs"),
        "pub fn outside() { println!(\"dirty\"); }\n",
    );

    let options =
        ReviewOptions::new("HEAD").unwrap_or_else(|error| panic!("review options failed: {error}"));
    let comparison = discover_git_comparison(repository.path().join("workspace"), &options)
        .await
        .unwrap_or_else(|error| panic!("nested comparison failed: {error}"));
    assert!(comparison.worktree_dirty());
    assert_eq!(comparison.files().len(), 1);
    assert_eq!(comparison.files()[0].path().as_str(), "app.rs");
}

#[tokio::test]
async fn history_blame_and_trace_culprits_use_bounded_exact_git_evidence() {
    let repository = repository_fixture();
    write(
        repository.path().join("src/z.rs"),
        "pub fn z() { println!(\"second\"); }\n",
    );
    git(repository.path(), &["add", "src/z.rs"]);
    git(repository.path(), &["commit", "-m", "second change"]);
    let path = NormalizedPath::parse("src/z.rs")
        .unwrap_or_else(|error| panic!("fixture path failed: {error}"));

    let history = discover_git_history(repository.path(), path.clone(), 10)
        .await
        .unwrap_or_else(|error| panic!("history failed: {error}"));
    assert_ne!(history.commits(), []);
    let line_history = discover_git_line_history(
        repository.path(),
        GitLineHistoryRequest::new(GitLineRange::new(path.clone(), 1, 1), 10),
    )
    .await
    .unwrap_or_else(|error| panic!("line history failed: {error}"));
    assert_ne!(line_history.commits(), []);
    assert!(!line_history.truncated());
    let commit_paths = discover_git_commit_paths(
        repository.path(),
        &[line_history.commits()[0].commit().to_owned()],
        20,
    )
    .await
    .unwrap_or_else(|error| panic!("commit paths failed: {error}"));
    assert_eq!(commit_paths.len(), 1);
    assert!(
        commit_paths[0]
            .paths()
            .iter()
            .any(|path| path.as_str() == "src/z.rs")
    );
    assert!(!commit_paths[0].truncated());
    let rename = discover_git_rename_evidence(repository.path(), path.clone())
        .await
        .unwrap_or_else(|error| panic!("rename evidence failed: {error}"));
    assert!(!rename.renamed());
    assert!(rename.earliest_unix_seconds().is_some());
    let blame = discover_git_blame(repository.path(), GitLineRange::new(path, 1, 1))
        .await
        .unwrap_or_else(|error| panic!("blame failed: {error}"));
    assert_eq!(blame.len(), 1);
    let report = trace_git_culprits(repository.path(), "panic at src/z.rs:1:12")
        .await
        .unwrap_or_else(|error| panic!("trace culprit lookup failed: {error}"));
    assert_eq!(report.frames().len(), 1);
}

#[tokio::test]
async fn rename_evidence_requires_an_actual_historical_path_change() {
    let repository = repository_fixture();
    git(repository.path(), &["mv", "src/z.rs", "src/renamed.rs"]);
    git(repository.path(), &["commit", "-m", "rename source"]);
    let path = NormalizedPath::parse("src/renamed.rs")
        .unwrap_or_else(|error| panic!("renamed fixture path failed: {error}"));
    let evidence = discover_git_rename_evidence(repository.path(), path)
        .await
        .unwrap_or_else(|error| panic!("rename evidence failed: {error}"));
    assert!(evidence.renamed());
    assert!(evidence.earliest_unix_seconds().is_some());
}

#[tokio::test]
async fn structural_compare_reads_two_refs_without_checking_out_head() {
    let repository = repository_fixture();
    write(
        repository.path().join("src/api.ts"),
        "export function api(): number { return 1; }\n",
    );
    git(repository.path(), &["add", "src/api.ts"]);
    git(repository.path(), &["commit", "-m", "typescript baseline"]);
    git(repository.path(), &["switch", "-c", "feature"]);
    write(
        repository.path().join("src/api.ts"),
        "export function danger(): number { return eval('1'); }\n\
         export function api(): number { return danger(); }\n",
    );
    git(repository.path(), &["add", "src/api.ts"]);
    git(repository.path(), &["commit", "-m", "feature change"]);
    git(repository.path(), &["switch", "main"]);

    let options = SourceCompareOptions::new("main")
        .and_then(|options| options.with_head("feature"))
        .and_then(|options| options.with_path_filter(Some("src/")))
        .map_or_else(
            |error| panic!("source compare options failed: {error}"),
            |options| {
                options
                    .with_edges(true)
                    .with_findings(true)
                    .with_findings_delta(true)
            },
        );
    let report = discover_source_comparison(repository.path(), options)
        .await
        .unwrap_or_else(|error| panic!("source comparison failed: {error}"));
    let report = serde_json::to_value(report)
        .unwrap_or_else(|error| panic!("source comparison serialization failed: {error}"));
    assert_eq!(report["comparison"], "main..feature");
    assert_eq!(report["comparesWorktree"], false);
    assert_eq!(report["changedBeforeFilter"], 1);
    assert_eq!(report["changedAfterFilter"], 1);
    assert_eq!(report["filesAnalyzed"], 1);
    assert_eq!(report["files"][0]["path"], "src/api.ts");
    assert!(report["symbolsAdded"].as_u64().unwrap_or_default() >= 1);
    assert!(report["symbolsModified"].as_u64().unwrap_or_default() >= 1);
    assert!(report["edgesAdded"].as_u64().unwrap_or_default() >= 1);
    assert!(report["findingsIntroduced"].as_u64().unwrap_or_default() >= 1);
    assert!(
        report["files"][0]["findingsDelta"]["introduced"]
            .as_array()
            .is_some_and(|findings| findings
                .iter()
                .any(|finding| finding["finding"] == "dynamic_eval"))
    );
    let worktree = std::fs::read_to_string(repository.path().join("src/api.ts"))
        .unwrap_or_else(|error| panic!("worktree read failed: {error}"));
    assert!(worktree.contains("return 1"));
    assert!(!worktree.contains("eval"));
}

fn repository_fixture() -> tempfile::TempDir {
    let repository = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
    git(repository.path(), &["init", "--initial-branch=main"]);
    git(
        repository.path(),
        &["config", "user.email", "review@example.invalid"],
    );
    git(
        repository.path(),
        &["config", "user.name", "Review Fixture"],
    );
    std::fs::create_dir_all(repository.path().join("src"))
        .unwrap_or_else(|error| panic!("fixture src directory failed: {error}"));
    write(
        repository.path().join("src/z.rs"),
        "pub fn z() { println!(\"base\"); }\n",
    );
    git(repository.path(), &["add", "src/z.rs"]);
    git(repository.path(), &["commit", "-m", "base"]);
    repository
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("git fixture command failed: {error}"));
    assert!(status.success(), "git fixture command returned {status}");
}

fn write(path: impl AsRef<Path>, content: &str) {
    std::fs::write(path, content)
        .unwrap_or_else(|error| panic!("fixture source write failed: {error}"));
}
